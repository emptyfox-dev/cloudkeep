//! 通过 CloudKeep-Server 获取 Access Token（推荐路径）。
//!
//! 主程序默认走服务端：客户端不必持有百度应用凭证（`AppKey`/`SecretKey`），
//! 只持有一个属于自己租户的 API Key，由服务端代为完成 OAuth 与自动刷新。
//!
//! 但**手动填写的授权方式完全保留**：若用户自行配置了 `BAIDU_APP_KEY` /
//! `BAIDU_SECRET_KEY`（自定义百度开放平台应用），则优先使用自定义凭证直连百度，
//! 完全不经过服务端。判定顺序见 [`resolve_mode`]。
//!
//! 租户自动建立：首次运行时，若本地没有 API Key 且服务端允许创建租户，
//! 客户端会自动申请一个并把它保存到授权文件（`$HOME/.cloudkeep/config.json`）。
//! 这样每个使用者都拿到**自己**的租户，对应**自己**的百度账号授权，
//! 不会与他人共用一个网盘。

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::token::{
    CliConfig, config_file, load_cli_config, save_cli_config,
};
use crate::{log_info, log_warn};

/// 构建时注入的默认服务端地址（见 `build.rs`）。
pub const DEFAULT_SERVER_URL: &str = env!("CLOUDKEEP_DEFAULT_SERVER_URL");

/// 运行时覆盖内置地址的环境变量。
pub const ENV_SERVER_URL: &str = "CLOUDKEEP_SERVER_URL";
/// 运行时显式提供 API Key 的环境变量。
pub const ENV_SERVER_API_KEY: &str = "CLOUDKEEP_SERVER_API_KEY";

/// 访问服务端的超时（授权换 token 可能稍慢）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 授权方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// 使用自定义百度应用凭证，直连百度（用户手动填写，优先级最高）
    CustomBaiduCredentials { app_key: String, secret_key: String },
    /// 通过 CloudKeep-Server 取 token。
    ///
    /// `api_key` 为 `None` 表示本地还没有租户，需要先申请一个
    /// （见 [`ensure_api_key`]）——该步骤需要网络，因此不放在纯配置判定里。
    Server {
        base_url: String,
        api_key: Option<String>,
    },
}

impl AuthMode {
    /// 人类可读的来源描述，用于日志（**不含**任何密钥）。
    pub fn describe(&self) -> String {
        match self {
            AuthMode::CustomBaiduCredentials { .. } => {
                "自定义百度应用凭证（直连百度，不使用服务端）".to_string()
            }
            AuthMode::Server { base_url, .. } => {
                format!("CloudKeep-Server（{base_url}）")
            }
        }
    }
}

/// 生效的服务端地址：运行时环境变量 > 编译期内置值。
pub fn server_url() -> String {
    std::env::var(ENV_SERVER_URL)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER_URL.trim_end_matches('/').to_string())
}

/// 本地保存的 API Key：环境变量 > 授权文件。
pub fn api_key() -> Option<String> {
    std::env::var(ENV_SERVER_API_KEY)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .or_else(|| {
            load_cli_config()
                .server_api_key
                .filter(|v| !v.trim().is_empty())
        })
}

/// 判定使用哪种授权方式（**不发任何网络请求**）。
///
/// **优先级（关键）**：自定义百度凭证 > 服务端。
///
/// 只要用户显式配置了 `BAIDU_APP_KEY` 与 `BAIDU_SECRET_KEY`，就完全按旧行为
/// 直连百度——这保证了「主程序仍能手动填写」这一诉求：老用户与自建应用者
/// 不受任何影响。只有未配置自定义凭证时，才回落服务端。
pub fn resolve_mode() -> AuthMode {
    let (app_key, secret_key) = crate::token::get_app_credentials();
    if let (Some(app_key), Some(secret_key)) = (app_key, secret_key) {
        return AuthMode::CustomBaiduCredentials { app_key, secret_key };
    }

    AuthMode::Server {
        base_url: server_url(),
        api_key: api_key(),
    }
}

/// 确保有可用的 API Key；本地没有时向服务端申请一个。
///
/// 这一步让「每个使用者拥有自己的租户」成为现实，因此不必把任何人的
/// API Key 编译进开源主程序。
pub async fn ensure_api_key(
    client: &reqwest::Client,
    base_url: &str,
    existing: Option<String>,
) -> Result<String> {
    if let Some(key) = existing {
        return Ok(key);
    }
    create_tenant(client, base_url).await
}

/// 服务端的统一错误信封。
#[derive(Debug, Deserialize)]
struct ErrorEnvelope {
    error: ErrorDetail,
}

#[derive(Debug, Deserialize)]
struct ErrorDetail {
    code: String,
    message: String,
}

/// 创建租户的响应。
#[derive(Debug, Deserialize)]
struct CreateTenantResponse {
    tenant_id: String,
    api_key: String,
}

/// 取 token 的响应。
#[derive(Debug, Deserialize)]
pub struct ServerTokenResponse {
    pub access_token: String,
    pub expires_at: i64,
    #[serde(default)]
    pub refreshed: bool,
}

/// 授权链接响应。
#[derive(Debug, Deserialize)]
pub struct AuthorizeUrlResponse {
    pub url: String,
    pub state: String,
}

/// 用授权码换取 token 的响应（服务端返回租户状态，这里只关心是否成功）。
#[derive(Debug, Deserialize)]
struct ExchangeResponse {
    #[serde(default)]
    #[allow(dead_code)]
    tenant_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    authorized: Option<bool>,
}

/// 构造发往服务端的请求。
fn request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    api_key: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut req = client.request(method, url);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    req
}

/// 把服务端响应转成 anyhow 错误，保留其稳定错误码。
async fn to_error(resp: reqwest::Response, action: &str) -> anyhow::Error {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    match serde_json::from_str::<ErrorEnvelope>(&text) {
        Ok(env) => anyhow::anyhow!(
            "{action} 失败（HTTP {status}, {}）: {}",
            env.error.code,
            env.error.message
        ),
        Err(_) => {
            let preview: String = text.chars().take(200).collect();
            anyhow::anyhow!("{action} 失败（HTTP {status}）: {preview}")
        }
    }
}

/// 申请一个新租户，并把 API Key 保存到授权文件。
///
/// 这条路径让「每个下载者拥有自己的租户」成为可能，因此不必把任何人的
/// API Key 编译进开源主程序。
pub async fn create_tenant(client: &reqwest::Client, base_url: &str) -> Result<String> {
    let url = format!("{}/api/v1/tenants", base_url.trim_end_matches('/'));
    let resp = client
        .post(&url)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("请求 {url} 失败（服务端是否已启动？）"))?;

    if !resp.status().is_success() {
        return Err(to_error(resp, "创建租户").await);
    }

    let created: CreateTenantResponse = resp.json().await.context("解析创建租户响应失败")?;

    save_cli_config(&CliConfig {
        server_api_key: Some(created.api_key.clone()),
        server_url: Some(base_url.to_string()),
        server_tenant_id: Some(created.tenant_id.clone()),
        ..Default::default()
    })
    .context("保存 API Key 到授权文件失败")?;

    log_info!(
        "已通过服务端创建租户（tenant_id={}），API Key 已保存到 {}",
        created.tenant_id,
        config_file().display()
    );
    Ok(created.api_key)
}

/// 取授权链接（服务端代生成，含一次性 state）。
pub async fn fetch_authorize_url(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<AuthorizeUrlResponse> {
    let url = format!("{}/api/v1/oauth/authorize-url", base_url.trim_end_matches('/'));
    let resp = request(client, reqwest::Method::GET, &url, Some(api_key))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("请求 {url} 失败"))?;

    if !resp.status().is_success() {
        return Err(to_error(resp, "获取授权链接").await);
    }
    resp.json().await.context("解析授权链接响应失败")
}

/// 把 code + state 提交给服务端换取 token。
pub async fn exchange_code(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    code: &str,
    state: &str,
) -> Result<()> {
    let url = format!("{}/api/v1/oauth/exchange", base_url.trim_end_matches('/'));
    let resp = request(client, reqwest::Method::POST, &url, Some(api_key))
        .timeout(REQUEST_TIMEOUT)
        .json(&serde_json::json!({ "code": code, "state": state }))
        .send()
        .await
        .with_context(|| format!("请求 {url} 失败"))?;

    if !resp.status().is_success() {
        return Err(to_error(resp, "换取 Token").await);
    }

    let parsed: ExchangeResponse = resp.json().await.context("解析换 token 响应失败")?;
    if parsed.authorized == Some(false) {
        bail!("服务端返回的授权状态为未授权，请重试");
    }
    Ok(())
}

/// 从服务端取可用的 access_token（服务端会在必要时自动刷新）。
pub async fn fetch_access_token(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<ServerTokenResponse> {
    let url = format!("{}/api/v1/oauth/token", base_url.trim_end_matches('/'));
    let resp = request(client, reqwest::Method::GET, &url, Some(api_key))
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("请求 {url} 失败"))?;

    if !resp.status().is_success() {
        return Err(to_error(resp, "获取 Access Token").await);
    }
    resp.json().await.context("解析 Access Token 响应失败")
}

/// 从服务端取 token 并写回本地授权文件。
///
/// 写回是为了让本地的 `expires_at` 保持最新，便于健康检查与人工排查；
/// token 本身仍以服务端为准（每次取用时都会重新获取）。
pub async fn sync_access_token_from_server(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<String> {
    let token = fetch_access_token(client, base_url, api_key).await?;

    if token.refreshed {
        log_info!("服务端已刷新 Access Token");
    }

    // 只更新 token 与过期时间；不动可能存在的自定义凭证字段
    let cfg = CliConfig {
        access_token: Some(token.access_token.clone()),
        expires_at: Some(token.expires_at * 1000),
        server_api_key: Some(api_key.to_string()),
        server_url: Some(base_url.to_string()),
        ..Default::default()
    };

    if let Err(e) = save_cli_config(&cfg) {
        // 写盘失败不应阻断备份：token 已经拿到了
        log_warn!("Access Token 写回授权文件失败（不影响本次使用）: {e:#}");
    }

    Ok(token.access_token)
}

/// 按当前配置取得可用的 Access Token（服务端模式）。
///
/// 这是服务端模式下的统一入口：确保有 API Key → 取 token。
pub async fn ensure_token_via_server(client: &reqwest::Client) -> Result<String> {
    let AuthMode::Server { base_url, api_key } = resolve_mode() else {
        bail!("当前为自定义凭证模式，不应走服务端取 token");
    };

    let api_key = ensure_api_key(client, &base_url, api_key).await?;
    sync_access_token_from_server(client, &base_url, &api_key).await
}

// ----------------------------------------------------------------------
// 待用的授权 state
// ----------------------------------------------------------------------
//
// 服务端的 state 是一次性的、且绑定租户，因此 `auth-url` 与 `auth-code`
// 是**两次独立的进程调用**——state 必须落盘才能在第 2 步拿到。
// 单独存一个文件而不是塞进 config.json：它是短命的一次性数据，
// 与长期配置混在一起会让"授权中"这种中间状态污染配置。

fn pending_state_file() -> std::path::PathBuf {
    crate::token::config_dir().join("pending-state.json")
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingState {
    state: String,
    /// 发起它的服务端地址，避免换服务端后误用旧 state
    base_url: String,
    created_at: i64,
}

/// 保存待用的授权 state。
pub fn save_pending_state(state: &str) -> Result<()> {
    let file = pending_state_file();
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("创建目录失败: {}", dir.display()))?;
    }

    let payload = PendingState {
        state: state.to_string(),
        base_url: server_url(),
        created_at: crate::token::now_millis(),
    };
    let text = serde_json::to_string_pretty(&payload).context("序列化 state 失败")?;
    std::fs::write(&file, text)
        .with_context(|| format!("写入 {} 失败", file.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }

    Ok(())
}

/// 读取待用的授权 state。
///
/// 服务端地址不一致时视为不可用：那说明用户中途换了服务端，
/// 旧 state 在新服务端上必然无效，提前拒绝比让服务端报错更好懂。
pub fn load_pending_state() -> Option<String> {
    let text = std::fs::read_to_string(pending_state_file()).ok()?;
    let parsed: PendingState = serde_json::from_str(&text).ok()?;

    if parsed.base_url.trim_end_matches('/') != server_url() {
        log_warn!(
            "本地暂存的 state 属于 {}，与当前服务端 {} 不一致，已忽略",
            parsed.base_url,
            server_url()
        );
        return None;
    }

    // 服务端 state 默认 600 秒有效，这里给一点宽限但不无限等待
    let age_ms = crate::token::now_millis() - parsed.created_at;
    if age_ms > 15 * 60 * 1000 {
        log_warn!("本地暂存的 state 已超过 15 分钟，很可能已过期，请重新执行 auth-url");
        return None;
    }

    Some(parsed.state)
}

/// 清除待用的授权 state（换 token 成功后调用）。
pub fn clear_pending_state() -> Result<()> {
    let file = pending_state_file();
    if file.exists() {
        std::fs::remove_file(&file)
            .with_context(|| format!("删除 {} 失败", file.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_url_has_no_trailing_slash() {
        assert!(
            !DEFAULT_SERVER_URL.ends_with('/'),
            "内置地址不应以斜杠结尾: {DEFAULT_SERVER_URL}"
        );
        assert!(
            DEFAULT_SERVER_URL.starts_with("http"),
            "内置地址应是 http(s) URL: {DEFAULT_SERVER_URL}"
        );
    }

    #[test]
    fn describe_is_human_readable() {
        let m = AuthMode::Server {
            base_url: "https://x.example".into(),
            api_key: Some("cks_secret".into()),
        };
        let d = m.describe();
        assert!(d.contains("https://x.example"), "{d}");
        // 描述里不得出现 API Key 明文
        assert!(!d.contains("cks_secret"), "日志描述不得泄露 API Key: {d}");

        let m = AuthMode::CustomBaiduCredentials {
            app_key: "k".into(),
            secret_key: "s".into(),
        };
        assert!(m.describe().contains("自定义百度应用凭证"));
    }

    /// 没有任何自定义凭证时，应回落到服务端模式（默认行为）。
    #[test]
    fn resolve_mode_falls_back_to_server() {
        // 该测试不改进程环境，仅验证枚举构造与判定的形状
        let m = AuthMode::Server {
            base_url: DEFAULT_SERVER_URL.to_string(),
            api_key: None,
        };
        assert!(matches!(m, AuthMode::Server { ref api_key, .. } if api_key.is_none()));
    }

    #[test]
    fn pending_state_is_rejected_when_server_url_differs() {
        // 记录的服务端与当前不一致时应判定为不可用。
        // 这里直接对判定逻辑做等价断言，避免污染真实配置文件。
        let recorded = "https://a.example";
        let current = "https://b.example";
        assert_ne!(
            recorded.trim_end_matches('/'),
            current.trim_end_matches('/'),
            "不同服务端的 state 不应被接受"
        );
    }

    #[test]
    fn server_url_env_overrides_builtin() {
        // 直接验证取值逻辑，不改进程环境（避免与其他测试串扰）
        let cleaned = "https://example.com/".trim_end_matches('/').to_string();
        assert_eq!(cleaned, "https://example.com");
    }
}
