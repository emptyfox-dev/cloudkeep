//! 备份主流程：打包 → 上传 → 清理远程历史备份。
//!
//! 每个源目录独立打包、上传、清理；单个目录失败不影响其余目录。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Timelike};
use chrono_tz::Tz;

use crate::api::PanClient;
use crate::archive::{ExcludeFilter, create_archive, format_size};
use crate::config::{Config, Target, build_targets};
use crate::remote::ensure_remote_dir;
use crate::token::ensure_access_token;
use crate::upload::upload_file;
use crate::{log_err, log_info, log_warn};

pub use crate::archive::format_size as format_bytes;

/// 单个目录的备份结果。
#[derive(Debug, Clone)]
pub struct BackupResult {
    pub name: String,
    pub file_name: String,
    pub remote_path: Option<String>,
    pub size: u64,
    pub skipped: bool,
    pub duration_ms: u128,
}

/// 一次备份的汇总结果。
#[derive(Debug, Clone)]
pub struct BackupSummary {
    pub results: Vec<BackupResult>,
    pub duration_ms: u128,
}

/// 生成备份文件名的时间戳 `YYYYMMDD_HHMMSS`（按配置时区）。
///
/// 用 24 小时制取小时，避免午夜生成 24 点时间戳。
pub fn timestamp(timezone: &str, at: chrono::DateTime<chrono::Utc>) -> Result<String> {
    let tz: Tz = timezone
        .parse::<Tz>()
        .with_context(|| format!("无法解析时区: {timezone}（可取值如 Asia/Shanghai、UTC）"))?;
    let local = at.with_timezone(&tz);

    Ok(format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute(),
        local.second()
    ))
}

/// 清理该网盘目录下的历史备份，保留最近 `BACKUP_RETENTION` 份。
async fn prune_remote(client: &PanClient, token: &str, cfg: &Config, remote_dir: &str) {
    if cfg.retention <= 0 {
        return;
    }

    let entries = match client.list(token, remote_dir).await {
        Ok(entries) => entries,
        Err(err) => {
            log_warn!("无法读取远程目录 {remote_dir}，跳过历史备份清理: {err:#}");
            return;
        }
    };

    // 只清理自己产生的备份文件，且必须是文件而非目录
    let mut mine: Vec<String> = entries
        .iter()
        .filter(|e| e.isdir != Some(true))
        .map(|e| e.name.clone())
        .filter(|name| {
            name.starts_with(&format!("{}_", cfg.prefix)) && name.ends_with(".tar.gz")
        })
        .collect();

    // 文件名内嵌时间戳，倒序即「最新在前」
    mine.sort_by(|a, b| b.cmp(a));

    if mine.len() as i64 <= cfg.retention {
        log_info!(
            "{remote_dir} 现有 {} 份备份，无需清理（保留 {} 份）",
            mine.len(),
            cfg.retention
        );
        return;
    }

    let stale: Vec<String> = mine.split_off(cfg.retention as usize);
    let targets: Vec<String> = stale
        .iter()
        .map(|name| format!("{remote_dir}/{name}"))
        .collect();

    match client.delete_files(token, &targets).await {
        Ok(err) if !err.is_error() => {
            log_info!(
                "已清理 {remote_dir} 历史备份 {} 份: {}",
                targets.len(),
                targets.join(", ")
            );
        }
        Ok(err) => {
            log_warn!("清理 {remote_dir} 历史备份失败: {}", err.describe());
        }
        Err(err) => {
            log_warn!("清理 {remote_dir} 历史备份失败: {err:#}");
        }
    }
}

/// 备份单个目录。
async fn backup_one(
    client: &PanClient,
    token: &str,
    cfg: &Config,
    target: &Target,
    filter: &ExcludeFilter,
    dry_run: bool,
) -> Result<BackupResult> {
    let started = Instant::now();
    let stamp = timestamp(&cfg.timezone, chrono::Utc::now())?;
    let file_name = format!("{}_{}.tar.gz", cfg.prefix, stamp);
    let archive_path = cfg.work_dir.join(format!("{}_{}", target.name, file_name));
    let remote_path = format!("{}/{}", target.remote_dir, file_name);

    let result = backup_one_inner(
        client, token, cfg, target, filter, dry_run, &archive_path, &remote_path, &file_name,
    )
    .await;

    // 无论成败都清掉本地临时包
    match tokio::fs::remove_file(&archive_path).await {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => log_warn!("删除本地归档失败 {}: {err}", archive_path.display()),
    }

    result.map(|mut r| {
        r.duration_ms = started.elapsed().as_millis();
        r
    })
}

#[allow(clippy::too_many_arguments)]
async fn backup_one_inner(
    client: &PanClient,
    token: &str,
    cfg: &Config,
    target: &Target,
    filter: &ExcludeFilter,
    dry_run: bool,
    archive_path: &Path,
    remote_path: &str,
    file_name: &str,
) -> Result<BackupResult> {
    if !dry_run {
        // 自定义备份路径可能尚不存在，先逐级创建
        ensure_remote_dir(client, token, &target.remote_dir).await?;
    }

    log_info!(
        "执行打包: {} -> {}",
        target.source,
        archive_path.display()
    );
    create_archive(archive_path, Path::new(&target.source), filter)
        .with_context(|| format!("打包失败: {}", target.source))?;

    let size = tokio::fs::metadata(archive_path)
        .await
        .with_context(|| format!("读取归档大小失败: {}", archive_path.display()))?
        .len();

    log_info!(
        "打包完成: {} -> {file_name}（{}）",
        target.name,
        format_size(size)
    );

    if dry_run {
        return Ok(BackupResult {
            name: target.name.clone(),
            file_name: file_name.to_string(),
            remote_path: None,
            size,
            skipped: true,
            duration_ms: 0,
        });
    }

    upload_with_retry(client, token, cfg, archive_path, remote_path).await?;
    log_info!("备份已上传: {remote_path}");

    prune_remote(client, token, cfg, &target.remote_dir).await;

    Ok(BackupResult {
        name: target.name.clone(),
        file_name: file_name.to_string(),
        remote_path: Some(remote_path.to_string()),
        size,
        skipped: false,
        duration_ms: 0,
    })
}

/// 上传并在失败时按递增间隔重试。
async fn upload_with_retry(
    client: &PanClient,
    token: &str,
    cfg: &Config,
    local_path: &Path,
    remote_path: &str,
) -> Result<()> {
    let mut last_err: Option<String> = None;

    for attempt in 1..=cfg.upload_attempts {
        log_info!(
            "上传到网盘 {remote_path}（第 {attempt}/{} 次尝试）",
            cfg.upload_attempts
        );

        match upload_file(client, token, local_path, remote_path, cfg.concurrency).await {
            Ok(_) => return Ok(()),
            Err(err) => {
                let text = format!("{err:#}");
                log_warn!("上传失败（第 {attempt} 次）: {text}");
                last_err = Some(text);

                if attempt < cfg.upload_attempts {
                    let delay = attempt as u64 * 5;
                    log_warn!("{delay}s 后重试");
                    tokio::time::sleep(Duration::from_secs(delay)).await;
                }
            }
        }
    }

    bail!(
        "上传失败（已尝试 {} 次）: {}",
        cfg.upload_attempts,
        last_err.unwrap_or_default()
    )
}

/// 清理工作目录里遗留的压缩包。
async fn clean_work_dir(work_dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(work_dir).await else {
        return;
    };

    let mut removed = 0usize;
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(".tar.gz") {
                    let path = entry.path();
                    if tokio::fs::remove_file(&path).await.is_ok() {
                        removed += 1;
                    }
                }
            }
            Ok(None) => break,
            Err(_) => break,
        }
    }

    if removed > 0 {
        log_info!("已清理本地残留备份 {removed} 个");
    }
}

/// 执行一次备份。
///
/// 每个源目录单独打包上传，单个失败不影响其余，最终以汇总结果判定整体成功与否。
pub async fn run_backup(
    client: &PanClient,
    cfg: &Config,
    dry_run: Option<bool>,
) -> Result<BackupSummary> {
    let started = Instant::now();
    let dry_run = dry_run.unwrap_or(cfg.dry_run);

    tokio::fs::create_dir_all(&cfg.work_dir)
        .await
        .with_context(|| format!("创建工作目录失败: {}", cfg.work_dir.display()))?;

    // 过滤不存在的源目录：不存在则跳过并告警，全部不存在则本次失败
    let mut existing: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for source in &cfg.sources {
        if Path::new(source).exists() {
            existing.push(source.clone());
        } else {
            missing.push(source.clone());
        }
    }

    if !missing.is_empty() {
        log_warn!("以下备份源不存在，已跳过: {}", missing.join(", "));
    }
    if existing.is_empty() {
        bail!("没有可备份的目录: {}", cfg.sources.join(", "));
    }

    let targets = build_targets(&cfg.remote_dir, &existing);
    clean_work_dir(&cfg.work_dir).await;

    log_info!(
        "开始备份 {} 个目录{}:",
        targets.len(),
        if dry_run { "（DRY_RUN，跳过上传）" } else { "" }
    );
    for target in &targets {
        log_info!("  - {} -> {}/", target.source, target.remote_dir);
    }

    // DRY_RUN 不需要网盘凭证；真实备份才需要
    let token = if dry_run {
        String::new()
    } else {
        ensure_access_token(client.http()).await?
    };

    let filter = ExcludeFilter::new(&cfg.excludes);
    let mut succeeded: Vec<BackupResult> = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();

    for target in &targets {
        match backup_one(client, &token, cfg, target, &filter, dry_run).await {
            Ok(result) => {
                log_info!(
                    "目录 {} 备份完成（{}，耗时 {}s）",
                    result.name,
                    format_size(result.size),
                    result.duration_ms as f64 / 1000.0
                );
                succeeded.push(result);
            }
            Err(err) => {
                let message = format!("{err:#}");
                log_err!("目录 {} 备份失败: {message}", target.name);
                failed.push((target.name.clone(), message));
            }
        }
    }

    let duration_ms = started.elapsed().as_millis();
    log_info!(
        "本次备份结束: 成功 {}/{}，耗时 {:.1}s",
        succeeded.len(),
        targets.len(),
        duration_ms as f64 / 1000.0
    );

    if !failed.is_empty() {
        let detail = failed
            .iter()
            .map(|(name, msg)| format!("{name}({msg})"))
            .collect::<Vec<_>>()
            .join("; ");
        bail!("{} 个目录备份失败: {detail}", failed.len());
    }

    Ok(BackupSummary { results: succeeded, duration_ms })
}

/// 工作目录下的归档路径（供测试与调试使用）。
pub fn archive_path(work_dir: &Path, target_name: &str, file_name: &str) -> PathBuf {
    work_dir.join(format!("{target_name}_{file_name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::NaiveDate::from_ymd_opt(y, mo, d)
            .unwrap()
            .and_hms_opt(h, mi, s)
            .unwrap()
            .and_utc()
    }

    #[test]
    fn timestamp_uses_configured_timezone() {
        let at = utc(2024, 3, 5, 18, 30, 45);

        let shanghai = timestamp("Asia/Shanghai", at).unwrap();
        assert_eq!(shanghai, "20240306_023045", "UTC+8 应为次日 02:30:45");

        let utc_stamp = timestamp("UTC", at).unwrap();
        assert_eq!(utc_stamp, "20240305_183045");
    }

    #[test]
    fn timestamp_midnight_is_00_not_24() {
        // 2024-01-01 16:00 UTC = Asia/Shanghai 2024-01-02 00:00
        let at = utc(2024, 1, 1, 16, 0, 0);
        let stamp = timestamp("Asia/Shanghai", at).unwrap();
        assert_eq!(stamp, "20240102_000000", "午夜必须是 00 点，不能是 24 点");
    }

    #[test]
    fn timestamp_pads_all_fields() {
        let at = utc(2024, 1, 9, 1, 2, 3);
        assert_eq!(timestamp("UTC", at).unwrap(), "20240109_010203");
    }

    #[test]
    fn timestamp_rejects_invalid_timezone() {
        let at = chrono::Utc::now();
        let err = timestamp("Not/AZone", at).unwrap_err().to_string();
        assert!(err.contains("时区"), "实际: {err}");
    }

    #[test]
    fn archive_path_helper() {
        let p = archive_path(
            Path::new("/tmp/ck"),
            "nginx",
            "backup_20240101_000000.tar.gz",
        );
        assert_eq!(
            p,
            PathBuf::from("/tmp/ck/nginx_backup_20240101_000000.tar.gz")
        );
    }
}
