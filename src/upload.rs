//! 分块上传：MD5 计算 → precreate → 并发上传分块 → create。
//!
//! 内存占用与文件大小无关：每次只读取一个分块（4 MB），
//! 并发数由 `UPLOAD_CONCURRENCY` 控制。

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::stream::StreamExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::api::{ApiError, CHUNK_SIZE, PanClient, PrecreateResult, md5_hex, total_chunks};
use crate::token::refresh_access_token;
use crate::{log_info, log_warn};

/// 上传结果。
#[derive(Debug, Clone, PartialEq)]
pub struct UploadOutcome {
    pub quick_upload: bool,
    pub bytes: u64,
    pub chunks_total: u64,
    pub chunks_uploaded: u64,
}

/// 逐块计算文件 MD5（不把整个文件读进内存）。
async fn compute_chunk_md5s(path: &Path, file_size: u64) -> Result<Vec<String>> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("打开文件失败: {}", path.display()))?;

    let total = total_chunks(file_size);
    let mut md5s = Vec::with_capacity(total as usize);
    let mut buf = vec![0u8; CHUNK_SIZE as usize];

    for _ in 0..total {
        let read = read_chunk_full(&mut file, &mut buf).await?;
        md5s.push(md5_hex(&buf[..read]));
    }

    Ok(md5s)
}

/// 读满一个分块；返回实际字节数（最后一个分块可能不足 CHUNK_SIZE）。
async fn read_chunk_full(file: &mut tokio::fs::File, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]).await? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// 执行一次上传（含 token 过期自动刷新重试）。
pub async fn upload_file(
    client: &PanClient,
    token: &str,
    local_path: &Path,
    remote_path: &str,
    concurrency: usize,
) -> Result<UploadOutcome> {
    let file_size = tokio::fs::metadata(local_path)
        .await
        .with_context(|| format!("读取文件属性失败: {}", local_path.display()))?
        .len();

    log_info!("上传目标: {remote_path}");
    log_info!("文件大小: {}", crate::archive::format_size(file_size));

    let md5_list = compute_chunk_md5s(local_path, file_size).await?;
    let chunks_total = md5_list.len() as u64;
    log_info!("分块数: {chunks_total}，正在预创建...");

    let (token, pre) = precreate_with_retry(client, token, remote_path, file_size, &md5_list).await?;

    if pre.quick_upload {
        log_info!("秒传成功（网盘已存在相同内容的文件）");
        return Ok(UploadOutcome {
            quick_upload: true,
            bytes: file_size,
            chunks_total,
            chunks_uploaded: 0,
        });
    }

    let Some(upload_id) = pre.upload_id.clone().filter(|s| !s.is_empty()) else {
        bail!("预创建成功但未返回 uploadid");
    };

    let uploaded_md5s = upload_chunks(
        client,
        &token,
        remote_path,
        &upload_id,
        local_path,
        file_size,
        &pre,
        concurrency,
    )
    .await?;

    let err = create_file_with_retry(
        client,
        &token,
        remote_path,
        file_size,
        &upload_id,
        &uploaded_md5s,
    )
    .await?;

    if let Some(err) = err
        && err.is_error()
    {
        bail!("提交上传文件失败: {}", err.describe());
    }

    log_info!("上传完成: {remote_path}");
    Ok(UploadOutcome {
        quick_upload: false,
        bytes: file_size,
        chunks_total,
        chunks_uploaded: uploaded_md5s.len() as u64,
    })
}

/// 判断错误文本是否表示 token 过期。
fn is_token_expired_text(text: &str) -> bool {
    text.contains("errno=-6")
        || text.contains("errno=111")
        || text.contains("error_code=110")
        || text.contains("error_code=111")
}

/// 预创建；token 过期时刷新后重试一次。
async fn precreate_with_retry(
    client: &PanClient,
    token: &str,
    remote_path: &str,
    file_size: u64,
    md5_list: &[String],
) -> Result<(String, PrecreateResult)> {
    match client.precreate(token, remote_path, file_size, md5_list).await {
        Ok(pre) => Ok((token.to_string(), pre)),
        Err(err) => {
            let text = format!("{err:#}");
            if !is_token_expired_text(&text) {
                return Err(err);
            }
            let Some(new_token) = refresh_access_token(client.http()).await? else {
                bail!(
                    "Access Token 已过期且无法自动刷新，请重新执行授权: cloudkeep auth-url && cloudkeep auth-code <code>"
                );
            };
            log_warn!("Access Token 已过期，已自动刷新并重试");
            let pre = client.precreate(&new_token, remote_path, file_size, md5_list).await?;
            Ok((new_token, pre))
        }
    }
}

/// 提交创建；token 过期时刷新后重试一次。
async fn create_file_with_retry(
    client: &PanClient,
    token: &str,
    remote_path: &str,
    file_size: u64,
    upload_id: &str,
    md5_list: &[String],
) -> Result<Option<ApiError>> {
    match client
        .create_file(token, remote_path, file_size, upload_id, md5_list)
        .await
    {
        Ok(err) => Ok(Some(err)),
        Err(err) => {
            let text = format!("{err:#}");
            if !is_token_expired_text(&text) {
                return Err(err);
            }
            let Some(new_token) = refresh_access_token(client.http()).await? else {
                bail!("Access Token 已过期且无法自动刷新，请重新执行授权");
            };
            let err = client
                .create_file(&new_token, remote_path, file_size, upload_id, md5_list)
                .await?;
            Ok(Some(err))
        }
    }
}

/// 并发上传需要补传的分块。
///
/// 每个分块单独打开文件句柄，避免共享 seek 游标导致并发读取错乱。
#[allow(clippy::too_many_arguments)]
async fn upload_chunks(
    client: &PanClient,
    token: &str,
    remote_path: &str,
    upload_id: &str,
    local_path: &Path,
    file_size: u64,
    pre: &PrecreateResult,
    concurrency: usize,
) -> Result<Vec<String>> {
    let blocks = &pre.blocks_to_upload;
    // 以本地计算的全量 MD5 为基准，未上传的分块保持原值
    let mut merged = pre.md5_list.clone();

    if blocks.is_empty() {
        return Ok(merged);
    }

    let client = Arc::new(client.clone());
    let token = Arc::new(token.to_string());
    let remote_path = Arc::new(remote_path.to_string());
    let upload_id = Arc::new(upload_id.to_string());

    let stream = futures::stream::iter(blocks.iter().copied())
        .map(|block_index| {
            let client = Arc::clone(&client);
            let token = Arc::clone(&token);
            let remote_path = Arc::clone(&remote_path);
            let upload_id = Arc::clone(&upload_id);
            let path = local_path.to_path_buf();
            async move {
                let offset = block_index * CHUNK_SIZE;
                let len = CHUNK_SIZE.min(file_size.saturating_sub(offset));

                let mut file = tokio::fs::File::open(&path).await?;
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                let mut buf = vec![0u8; len as usize];
                file.read_exact(&mut buf).await?;

                let md5 = client
                    .upload_chunk(&token, &remote_path, &upload_id, block_index, buf)
                    .await?;
                Ok::<(u64, String), anyhow::Error>((block_index, md5))
            }
        })
        .buffer_unordered(concurrency.max(1));

    futures::pin_mut!(stream);

    let total = blocks.len() as u64;
    let mut completed = 0u64;
    let mut results: Vec<(u64, String)> = Vec::with_capacity(blocks.len());

    while let Some(item) = stream.next().await {
        let (block_index, md5) = item?;
        completed += 1;
        results.push((block_index, md5));
        log_progress(completed, total);
    }

    // 按分块序号写回，保证顺序与本地计算一致
    for (idx, md5) in results {
        if let Some(slot) = merged.get_mut(idx as usize) {
            *slot = md5;
        }
    }

    Ok(merged)
}

fn log_progress(completed: u64, total: u64) {
    let pct = completed
        .saturating_mul(100)
        .checked_div(total)
        .map(|v| v.min(100))
        .unwrap_or(100);
    log_info!("上传中: {completed}/{total} ({pct}%)");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_expiry_text_detection() {
        assert!(is_token_expired_text("预创建失败: errno=-6"));
        assert!(is_token_expired_text("errno=111"));
        assert!(is_token_expired_text("error_code=110"));
        assert!(is_token_expired_text("error_code=111"));
        assert!(!is_token_expired_text("errno=-7"));
        assert!(!is_token_expired_text("网络连接失败"));
    }

    #[tokio::test]
    async fn compute_md5s_splits_file_into_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");

        // 跨越两个分块：CHUNK_SIZE + 100 字节
        let data = vec![7u8; (CHUNK_SIZE + 100) as usize];
        std::fs::write(&path, &data).unwrap();

        let md5s = compute_chunk_md5s(&path, data.len() as u64).await.unwrap();
        assert_eq!(md5s.len(), 2, "应切分为 2 个分块");
        assert_eq!(md5s[0], md5_hex(&data[..CHUNK_SIZE as usize]));
        assert_eq!(md5s[1], md5_hex(&data[CHUNK_SIZE as usize..]));
    }

    #[tokio::test]
    async fn compute_md5s_handles_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();

        let md5s = compute_chunk_md5s(&path, 0).await.unwrap();
        assert_eq!(md5s.len(), 0, "空文件没有分块");
    }

    #[tokio::test]
    async fn compute_md5s_handles_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.bin");
        let data = b"hello world".to_vec();
        std::fs::write(&path, &data).unwrap();

        let md5s = compute_chunk_md5s(&path, data.len() as u64).await.unwrap();
        assert_eq!(md5s.len(), 1);
        assert_eq!(md5s[0], md5_hex(&data));
    }

    #[tokio::test]
    async fn compute_md5s_handles_exact_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("exact.bin");
        let data = vec![3u8; (CHUNK_SIZE * 2) as usize];
        std::fs::write(&path, &data).unwrap();

        let md5s = compute_chunk_md5s(&path, data.len() as u64).await.unwrap();
        assert_eq!(md5s.len(), 2, "整除时不应多出空分块");
        assert_eq!(md5s[1], md5_hex(&data[CHUNK_SIZE as usize..]));
    }
}
