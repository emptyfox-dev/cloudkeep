//! 网盘目录逐级创建。
//!
//! 开放平台没有 mkdir 接口，创建目录走 `create` + `isdir=1`；
//! 因为上传不会自动创建父目录，这里在备份前逐级建好，已存在的层级自动跳过。

use anyhow::{Result, bail};

use crate::api::{PanClient, errno};
use crate::token::refresh_access_token;
use crate::{log_info, log_warn};

/// 目录名中的非法字符（百度侧 errno=-7）。
const ILLEGAL_CHARS: &[char] = &['\\', '/', ':', '*', '?', '"', '<', '>', '|'];

/// 校验单级目录名是否合法，提前拦截以免白跑一次上传。
pub fn validate_segment(segment: &str) -> Result<()> {
    if segment.is_empty() {
        bail!("网盘目录名不能为空");
    }
    if segment == "." || segment == ".." {
        bail!("网盘目录名不合法: {segment}");
    }
    if segment.chars().any(|c| ILLEGAL_CHARS.contains(&c)) {
        bail!("网盘目录名不合法: {segment}（不能包含 \\ / : * ? \" < > | 等字符）");
    }
    Ok(())
}

/// 判断网盘目录是否已存在：列出父目录并查找同名目录。
///
/// 读取父目录失败时按「不存在」处理：后续创建仍会执行，
/// 因此不会因为一次 list 失败就中断整个备份。
pub async fn remote_dir_exists(client: &PanClient, token: &str, dir: &str) -> bool {
    let idx = dir.rfind('/');
    let parent = match idx {
        Some(0) | None => "/",
        Some(i) => &dir[..i],
    };
    let name = match idx {
        Some(i) => &dir[i + 1..],
        None => dir,
    };
    if name.is_empty() {
        return true;
    }

    match client.list(token, parent).await {
        Ok(entries) => entries
            .iter()
            .any(|e| e.name == name && e.isdir != Some(false)),
        Err(_) => false,
    }
}

/// 确保网盘目录存在（逐级创建，已存在则跳过）。
pub async fn ensure_remote_dir(client: &PanClient, token: &str, dir: &str) -> Result<String> {
    let segments: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        bail!("无效的网盘目录: {dir}");
    }
    for segment in &segments {
        validate_segment(segment)?;
    }

    let mut created: Vec<String> = Vec::new();
    let mut current = String::new();

    for segment in segments {
        current.push('/');
        current.push_str(segment);

        // 已存在的目录直接跳过：重复创建时接口可能返回无 errno 的响应，导致误判为失败
        if remote_dir_exists(client, token, &current).await {
            continue;
        }

        let err = create_dir_level(client, token, &current).await?;
        match err.errno {
            Some(errno::OK) => created.push(current.clone()),
            Some(errno::EXISTS) => {
                // 目录已存在，无需处理
            }
            Some(errno::NAME_INVALID) => {
                bail!("网盘目录名不合法: {current}（不能包含 \\ / : * ? \" < > | 等字符）");
            }
            Some(errno::NOT_FOUND) => {
                bail!("创建网盘目录失败，父目录不存在: {current}");
            }
            _ if remote_dir_exists(client, token, &current).await => {
                // 接口返回了非 errno 形式的响应（如目录已存在的 error_code），以实际状态为准
                log_warn!(
                    "创建网盘目录 {current} 返回异常（{}），但目录已存在，继续备份",
                    err.describe()
                );
            }
            _ => {
                bail!("创建网盘目录 {current} 失败: {}", err.describe());
            }
        }
    }

    if created.is_empty() {
        log_info!("网盘目录已存在: {dir}");
    } else {
        log_info!("已创建网盘目录: {}", created.join(", "));
    }

    Ok(dir.to_string())
}

/// 创建单级目录；token 过期时刷新后重试一次。
async fn create_dir_level(
    client: &PanClient,
    token: &str,
    dir: &str,
) -> Result<crate::api::ApiError> {
    let err = client.create_dir(token, dir).await?;

    if !err.is_token_expired() {
        return Ok(err);
    }

    let Some(new_token) = refresh_access_token(client.http()).await? else {
        bail!(
            "Access Token 已过期且无法自动刷新，请重新执行授权: cloudkeep auth-url && cloudkeep auth-code <code>"
        );
    };
    log_warn!("Access Token 已过期，已自动刷新并重试创建目录");
    client.create_dir(&new_token, dir).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legal_segments_accepted() {
        for name in ["backup", "docker", "cloud-keep", "nginx", "data_1", "a.b"] {
            assert!(validate_segment(name).is_ok(), "应接受: {name}");
        }
    }

    #[test]
    fn illegal_characters_rejected() {
        for name in ["a\\b", "a/b", "a:b", "a*b", "a?b", "a\"b", "a<b", "a>b", "a|b"] {
            let err = validate_segment(name).unwrap_err().to_string();
            assert!(err.contains("不合法"), "应拒绝 {name}: {err}");
        }
    }

    #[test]
    fn dot_segments_rejected() {
        assert!(validate_segment(".").is_err());
        assert!(validate_segment("..").is_err());
        assert!(validate_segment("").is_err());
    }

    #[test]
    fn unicode_names_allowed() {
        assert!(validate_segment("备份").is_ok());
        assert!(validate_segment("数据-2024").is_ok());
    }
}
