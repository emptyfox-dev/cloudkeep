//! 授权文件读写与 Access Token 自动刷新。
//!
//! 授权信息以 JSON 保存在 `$HOME/.cloudkeep/config.json`，
//! 可用 `CLOUDKEEP_CONFIG_DIR` 覆盖目录（便于测试与多实例）。

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{log_info, log_warn};

pub const OPENAPI_URL: &str = "https://openapi.baidu.com";
pub const PAN_URL: &str = "https://pan.baidu.com";
/// 百度接口要求携带该 UA，否则可能被拒绝
pub const USER_AGENT: &str = "pan.baidu.com";

/// 授权文件内容。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// CloudKeep-Server 的租户 API Key（走服务端时使用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_api_key: Option<String>,
    /// 该 API Key 所属的服务端地址（便于识别它属于哪个部署）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
    /// 服务端返回的租户 ID（仅用于日志与排查）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_tenant_id: Option<String>,
}

/// 授权文件所在目录，可用 `CLOUDKEEP_CONFIG_DIR` 覆盖。
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLOUDKEEP_CONFIG_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| "/".to_string());
    PathBuf::from(home).join(".cloudkeep")
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.json")
}

/// 读取默认位置的授权文件。
pub fn load_cli_config() -> CliConfig {
    load_cli_config_from(&config_file())
}

/// 从指定路径读取授权文件（测试与多实例使用，不依赖进程环境）。
pub fn load_cli_config_from(path: &Path) -> CliConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return CliConfig::default();
    };
    // 配置损坏时按未授权处理，不阻断启动
    serde_json::from_str(&text).unwrap_or_default()
}

/// 合并写入默认位置的授权文件。
pub fn save_cli_config(updates: &CliConfig) -> Result<()> {
    save_cli_config_to(&config_file(), updates)
}

/// 合并写入指定路径的授权文件（保留未提及的字段）。
pub fn save_cli_config_to(path: &Path, updates: &CliConfig) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建授权目录失败: {}", dir.display()))?;
        set_dir_permissions(dir);
    }

    let mut merged = load_cli_config_from(path);
    macro_rules! merge {
        ($($f:ident),*) => {
            $(if updates.$f.is_some() { merged.$f = updates.$f.clone(); })*
        };
    }
    merge!(access_token, refresh_token, app_key, secret_key, expires_at);
    merge!(server_api_key, server_url, server_tenant_id);

    let text = serde_json::to_string_pretty(&merged).context("序列化授权文件失败")?;
    std::fs::write(path, text)
        .with_context(|| format!("写入授权文件失败: {}", path.display()))?;
    set_file_permissions(path);

    Ok(())
}

#[cfg(unix)]
fn set_file_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_file_permissions(_path: &std::path::Path) {}

#[cfg(unix)]
fn set_dir_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn set_dir_permissions(_path: &std::path::Path) {}

/// 进程内缓存最近获取/刷新的 token，优先级高于环境变量：
/// 环境变量提供的 token 过期并刷新后，若仍读环境变量会一直使用旧值，
/// 导致每次调用都重复「报错 → 刷新」。
static RUNTIME_TOKENS: RwLock<Option<(String, String)>> = RwLock::new(None);

fn set_runtime_tokens(access: String, refresh: String) {
    if let Ok(mut guard) = RUNTIME_TOKENS.write() {
        *guard = Some((access, refresh));
    }
}

fn runtime_access() -> Option<String> {
    RUNTIME_TOKENS
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|(a, _)| a.clone()))
}

fn runtime_refresh() -> Option<String> {
    RUNTIME_TOKENS
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|(_, r)| r.clone()))
}

pub fn get_access_token() -> Option<String> {
    runtime_access()
        .or_else(|| {
            std::env::var("BAIDU_ACCESS_TOKEN")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .or_else(|| load_cli_config().access_token)
        .filter(|v| !v.is_empty())
}

pub fn get_refresh_token() -> Option<String> {
    runtime_refresh()
        .or_else(|| {
            std::env::var("BAIDU_REFRESH_TOKEN")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .or_else(|| load_cli_config().refresh_token)
        .filter(|v| !v.is_empty())
}

pub fn get_app_credentials() -> (Option<String>, Option<String>) {
    let cfg = load_cli_config();
    (
        std::env::var("BAIDU_APP_KEY")
            .ok()
            .filter(|v| !v.is_empty())
            .or(cfg.app_key),
        std::env::var("BAIDU_SECRET_KEY")
            .ok()
            .filter(|v| !v.is_empty())
            .or(cfg.secret_key),
    )
}

/// errno == -6 / 111 表示 token 过期
pub fn is_token_expired_errno(errno: i64) -> bool {
    errno == -6 || errno == 111
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    error: Option<String>,
    error_description: Option<String>,
}

/// 用 refresh_token 换新 token，并写回授权文件（refresh_token 会轮换）。
///
/// 服务端模式下本函数不做事：刷新由服务端负责，客户端只需重新取一次 token
/// （见 [`ensure_access_token`]）。这样上传/建目录等处的"过期则刷新重试"逻辑
/// 在两种模式下都能正确工作。
pub async fn refresh_access_token(client: &reqwest::Client) -> Result<Option<String>> {
    if matches!(
        crate::server::resolve_mode(),
        crate::server::AuthMode::Server { .. }
    ) {
        let token = crate::server::ensure_token_via_server(client).await?;
        return Ok(Some(token));
    }

    let Some(refresh_token) = get_refresh_token() else {
        return Ok(None);
    };
    let (app_key, secret_key) = get_app_credentials();
    let (Some(app_key), Some(secret_key)) = (app_key, secret_key) else {
        return Ok(None);
    };

    log_info!("Access Token 已过期，正在刷新...");

    let text = client
        .get(format!("{OPENAPI_URL}/oauth/2.0/token"))
        .query(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh_token),
            ("client_id", &app_key),
            ("client_secret", &secret_key),
        ])
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("请求百度授权接口失败")?
        .text()
        .await
        .context("读取刷新响应失败")?;

    let data: TokenResponse = serde_json::from_str(&text).with_context(|| {
        let preview: String = text.chars().take(200).collect();
        format!("刷新响应无法解析: {preview}")
    })?;

    let Some(access_token) = data.access_token.filter(|v| !v.is_empty()) else {
        let reason = data
            .error_description
            .or(data.error)
            .unwrap_or_else(|| "未知错误".to_string());
        log_warn!("刷新 Token 失败: {reason}");
        return Ok(None);
    };

    // 刷新响应可能不返回 refresh_token，此时保留旧的（merge 只覆盖 Some 字段）
    let new_refresh = data
        .refresh_token
        .clone()
        .unwrap_or_else(|| refresh_token.clone());
    set_runtime_tokens(access_token.clone(), new_refresh);

    save_cli_config(&CliConfig {
        access_token: Some(access_token.clone()),
        refresh_token: data.refresh_token,
        expires_at: Some(now_millis() + data.expires_in.unwrap_or(0) * 1000),
        ..Default::default()
    })?;

    log_info!("Access Token 已刷新并写回配置文件");
    Ok(Some(access_token))
}

/// 确保有可用的 Access Token：必要时先刷新。
///
/// **两条授权路径在此汇合**：
/// - 配了自定义百度凭证（`BAIDU_APP_KEY` + `BAIDU_SECRET_KEY`）→ 直连百度刷新（原有行为）；
/// - 未配自定义凭证 → 通过 CloudKeep-Server 取 token（服务端代管应用凭证）。
pub async fn ensure_access_token(client: &reqwest::Client) -> Result<String> {
    // 服务端模式：先把 token 同步下来（内部会按需自动创建租户）
    if matches!(
        crate::server::resolve_mode(),
        crate::server::AuthMode::Server { .. }
    ) {
        return crate::server::ensure_token_via_server(client).await;
    }

    if let Some(token) = get_access_token() {
        return Ok(token);
    }
    match refresh_access_token(client).await? {
        Some(token) => Ok(token),
        None => bail!(
            "未找到 Access Token，请先执行授权: cloudkeep auth-url && cloudkeep auth-code <code>\n\
             或配置 CloudKeep-Server（CLOUDKEEP_SERVER_URL + CLOUDKEEP_SERVER_API_KEY）"
        ),
    }
}

pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 所有测试都直接用显式路径操作临时文件，**不碰任何进程级环境变量**。
    ///
    /// 早期版本通过 CLOUDKEEP_CONFIG_DIR 注入临时目录，但不同模块各持一把锁，
    /// 并行执行时会互相把环境变量清掉，导致写入落到真实的 $HOME 目录。
    /// 改为显式路径后，这类串扰在结构上不可能发生。
    fn temp_config_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        (dir, path)
    }

    #[test]
    fn save_then_load_roundtrips() {
        let (_dir, path) = temp_config_path();

        save_cli_config_to(
            &path,
            &CliConfig {
                access_token: Some("acc-1".into()),
                refresh_token: Some("ref-1".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let cfg = load_cli_config_from(&path);
        assert_eq!(cfg.access_token.as_deref(), Some("acc-1"));
        assert_eq!(cfg.refresh_token.as_deref(), Some("ref-1"));
    }

    #[test]
    fn partial_update_preserves_other_fields() {
        let (_dir, path) = temp_config_path();

        save_cli_config_to(
            &path,
            &CliConfig {
                access_token: Some("acc-1".into()),
                refresh_token: Some("ref-1".into()),
                app_key: Some("key-1".into()),
                ..Default::default()
            },
        )
        .unwrap();

        // 只更新 access_token，refresh_token 与 app_key 必须保留
        save_cli_config_to(
            &path,
            &CliConfig {
                access_token: Some("acc-2".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let cfg = load_cli_config_from(&path);
        assert_eq!(cfg.access_token.as_deref(), Some("acc-2"));
        assert_eq!(cfg.refresh_token.as_deref(), Some("ref-1"));
        assert_eq!(cfg.app_key.as_deref(), Some("key-1"));
    }

    #[test]
    fn missing_config_file_loads_empty() {
        let (_dir, path) = temp_config_path();
        let cfg = load_cli_config_from(&path);
        assert!(cfg.access_token.is_none());
    }

    #[test]
    fn corrupt_config_file_loads_empty() {
        let (_dir, path) = temp_config_path();
        std::fs::write(&path, "{ not json").unwrap();

        let cfg = load_cli_config_from(&path);
        assert!(cfg.access_token.is_none(), "损坏的配置应按未授权处理");
    }

    #[test]
    fn save_creates_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        // 三层不存在的子目录
        let path = dir.path().join("a").join("b").join("config.json");

        save_cli_config_to(
            &path,
            &CliConfig {
                access_token: Some("x".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(path.exists(), "应自动创建授权目录");
        assert_eq!(
            load_cli_config_from(&path).access_token.as_deref(),
            Some("x")
        );
    }

    #[test]
    fn saved_file_is_valid_json_with_expected_fields() {
        let (_dir, path) = temp_config_path();
        save_cli_config_to(
            &path,
            &CliConfig {
                access_token: Some("acc".into()),
                refresh_token: Some("ref".into()),
                expires_at: Some(123),
                ..Default::default()
            },
        )
        .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["access_token"], "acc");
        assert_eq!(parsed["refresh_token"], "ref");
        assert_eq!(parsed["expires_at"], 123);
        // None 字段不应写入文件
        assert!(parsed.get("app_key").is_none());
    }

    /// 回归用例：写入必须严格发生在给定路径，绝不能落到默认的 $HOME 目录。
    #[test]
    fn save_never_touches_default_location() {
        let (_dir, path) = temp_config_path();
        let default_before = std::fs::read_to_string(config_file()).ok();

        save_cli_config_to(
            &path,
            &CliConfig {
                access_token: Some("isolated".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let default_after = std::fs::read_to_string(config_file()).ok();
        assert_eq!(
            default_before, default_after,
            "写入临时路径不应影响默认授权文件 {}",
            config_file().display()
        );

        // 且临时路径确实写成功了
        assert_eq!(
            load_cli_config_from(&path).access_token.as_deref(),
            Some("isolated")
        );
    }

    #[test]
    fn config_dir_defaults_to_cloudkeep_under_home() {
        // 在未设置覆盖变量的前提下，默认目录应为 $HOME/.cloudkeep
        if std::env::var("CLOUDKEEP_CONFIG_DIR").is_err() {
            let dir = config_dir();
            assert!(
                dir.ends_with(".cloudkeep"),
                "默认授权目录应为 .cloudkeep，实际: {}",
                dir.display()
            );
            assert!(config_file().ends_with("config.json"));
        }
    }

    #[test]
    fn token_expiry_errno_detection() {
        assert!(is_token_expired_errno(-6));
        assert!(is_token_expired_errno(111));
        assert!(!is_token_expired_errno(0));
        assert!(!is_token_expired_errno(-7));
    }
}
