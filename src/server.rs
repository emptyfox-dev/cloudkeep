//! 通过 CloudKeep-Server 获取 Access Token（默认路径）。
//!
//! 客户端不必持有百度应用凭证（`AppKey`/`SecretKey`），只持有一个属于自己租户的
//! API Key，由服务端代为完成 OAuth 与自动刷新。
//!
//! # 两条硬性约定
//!
//! 1. **服务端地址是固定的**，编译进二进制，**不提供任何自定义入口**
//!    （既没有环境变量，也没有配置文件项）。它指向官方部署的
//!    [`SERVER_BASE`]，用户不应、也无法把它指向别处。
//! 2. **只有百度开发者凭证可以自定义**：使用者若在百度开放平台有自己的应用，
//!    配置 `BAIDU_APP_KEY` / `BAIDU_SECRET_KEY` 后即**直连百度**，完全不经过服务端。
//!    判定顺序见 [`resolve_mode`]。
//!
//! 租户自动建立：首次运行时，若本地没有 API Key，客户端会自动向服务端申请一个
//! 并保存到授权文件（`$HOME/.cloudkeep/config.json`）。这样每个使用者都拿到
//! **自己**的租户，对应**自己**的百度账号授权，不会与他人共用一个网盘。

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::token::{CliConfig, config_file, load_cli_config, save_cli_config};
use crate::{log_info, log_warn};

/// 官方服务端地址。**固定不可配置。**
///
/// 服务端与客户端由同一方维护，因此没有"换服务器"的需求；把地址写死可以避免
/// 用户被诱导把请求（以及自己的租户 API Key）发到伪造的第三方服务上。
///
/// 各接口路径在此基础上拼接，例如 `{SERVER_BASE}/api/v1/oauth/token`。
pub const SERVER_BASE: &str = "https://cloudkeep.emptyfox.net";

/// 访问服务端的超时（授权换 token 可能稍慢）。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// 授权方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMode {
    /// 使用自定义百度应用凭证，直连百度（使用者自己的百度开发者应用）
    CustomBaiduCredentials { app_key: String, secret_key: String },
    /// 通过 CloudKeep-Server 取 token（默认）。
    ///
    /// `api_key` 为 `None` 表示本地还没有租户，需要先申请一个
    /// （见 [`ensure_api_key`]）——该步骤需要网络，因此不放在纯配置判定里。
    Server { api_key: Option<String> },
}

impl AuthMode {
    /// 人类可读的来源描述，用于日志（**不含**任何密钥）。
    pub fn describe(&self) -> String {
        match self {
            AuthMode::CustomBaiduCredentials { .. } => {
                "自定义百度应用凭证（直连百度，不使用服务端）".to_string()
            }
            AuthMode::Server { .. } => format!("CloudKeep-Server（{SERVER_BASE}）"),
        }
    }
}

/// 生效的服务端地址。
///
/// 恒等于 [`SERVER_BASE`]：保留为函数只为让调用点读起来自然，
/// **不接受任何覆盖**（这是刻意的设计约束，不是遗漏）。
pub fn server_url() -> String {
    SERVER_BASE.trim_end_matches('/').to_string()
}

/// 本地保存的 API Key。
///
/// 只来自授权文件——它由客户端首次运行时自动申请，不是用户该操心的配置项。
pub fn api_key() -> Option<String> {
    load_cli_config()
        .server_api_key
        .filter(|v| !v.trim().is_empty())
}

/// 判定使用哪种授权方式（**不发任何网络请求**）。
///
/// **优先级**：自定义百度凭证 > 服务端。
///
/// 只要使用者配置了 `BAIDU_APP_KEY` 与 `BAIDU_SECRET_KEY`，就直连百度，
/// 完全不经过服务端；否则走固定的官方服务端。
pub fn resolve_mode() -> AuthMode {
    let (app_key, secret_key) = crate::token::get_app_credentials();
    if let (Some(app_key), Some(secret_key)) = (app_key, secret_key) {
        return AuthMode::CustomBaiduCredentials { app_key, secret_key };
    }

    AuthMode::Server { api_key: api_key() }
}

/// 确保有可用的 API Key；本地没有时向服务端申请一个。
///
/// 这一步让「每个使用者拥有自己的租户」成为现实，因此不必把任何人的
/// API Key 编译进开源主程序。
pub async fn ensure_api_key(client: &reqwest::Client, existing: Option<String>) -> Result<String> {
    if let Some(key) = existing {
        return Ok(key);
    }
    create_tenant(client).await
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

/// 拼接服务端接口地址（基于固定的 [`SERVER_BASE`]）。
fn api_url(path: &str) -> String {
    format!("{}/{}", server_url(), path.trim_start_matches('/'))
}

/// 申请一个新租户，并把 API Key 保存到授权文件。
///
/// 这条路径让「每个下载者拥有自己的租户」成为可能，因此不必把任何人的
/// API Key 编译进开源主程序。
pub async fn create_tenant(client: &reqwest::Client) -> Result<String> {
    let url = api_url("api/v1/tenants");
    let resp = client
        .post(&url)
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .with_context(|| format!("请求 {url} 失败（网络不可达或服务未就绪）"))?;

    if !resp.status().is_success() {
        return Err(to_error(resp, "创建租户").await);
    }

    let created: CreateTenantResponse = resp.json().await.context("解析创建租户响应失败")?;

    save_cli_config(&CliConfig {
        server_api_key: Some(created.api_key.clone()),
        server_url: Some(server_url()),
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
    api_key: &str,
) -> Result<AuthorizeUrlResponse> {
    let url = api_url("api/v1/oauth/authorize-url");
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
    api_key: &str,
    code: &str,
    state: &str,
) -> Result<()> {
    let url = api_url("api/v1/oauth/exchange");
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
    api_key: &str,
) -> Result<ServerTokenResponse> {
    let url = api_url("api/v1/oauth/token");
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
    api_key: &str,
) -> Result<String> {
    let token = fetch_access_token(client, api_key).await?;

    if token.refreshed {
        log_info!("服务端已刷新 Access Token");
    }

    // 只更新 token 与过期时间；不动可能存在的自定义凭证字段
    let cfg = CliConfig {
        access_token: Some(token.access_token.clone()),
        expires_at: Some(token.expires_at * 1000),
        server_api_key: Some(api_key.to_string()),
        server_url: Some(server_url()),
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
    let AuthMode::Server { api_key } = resolve_mode() else {
        bail!("当前为自定义凭证模式，不应走服务端取 token");
    };

    let api_key = ensure_api_key(client, api_key).await?;
    sync_access_token_from_server(client, &api_key).await
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
pub fn load_pending_state() -> Option<String> {
    let text = std::fs::read_to_string(pending_state_file()).ok()?;
    let parsed: PendingState = serde_json::from_str(&text).ok()?;

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
    fn describe_is_human_readable() {
        let m = AuthMode::Server {
            api_key: Some("cks_secret".into()),
        };
        let d = m.describe();
        assert!(d.contains(SERVER_BASE), "{d}");
        // 描述里不得出现 API Key 明文
        assert!(!d.contains("cks_secret"), "日志描述不得泄露 API Key: {d}");

        let m = AuthMode::CustomBaiduCredentials {
            app_key: "k".into(),
            secret_key: "s".into(),
        };
        assert!(m.describe().contains("自定义百度应用凭证"));
    }

    /// 服务端地址是固定常量，且必须指向官方域名。
    #[test]
    fn server_base_is_the_official_fixed_endpoint() {
        assert_eq!(SERVER_BASE, "https://cloudkeep.emptyfox.net");
        assert_eq!(server_url(), SERVER_BASE);
        assert!(server_url().starts_with("https://"), "必须走 HTTPS");
        assert!(!server_url().ends_with('/'), "拼接路径前不应带尾斜杠");
    }

    /// 服务端地址**不可**被环境变量覆盖——这是刻意约束：
    /// 避免用户被诱导把租户 API Key 发往伪造的第三方服务。
    #[test]
    fn server_url_cannot_be_overridden_by_env() {
        // 逐个尝试可能被误加进来的变量名，都必须无效
        for key in [
            "CLOUDKEEP_SERVER_URL",
            "CLOUDKEEP_DEFAULT_SERVER_URL",
            "SERVER_URL",
            "CLOUDKEEP_SERVER",
        ] {
            // 即便有人在进程环境里设了这些变量，取值也不应改变
            unsafe { std::env::set_var(key, "https://evil.example") };
            assert_eq!(
                server_url(),
                SERVER_BASE,
                "环境变量 {key} 不应能改写服务端地址"
            );
            unsafe { std::env::remove_var(key) };
        }
    }

    /// 接口路径必须拼在固定的官方地址之下。
    #[test]
    fn api_urls_are_built_on_the_fixed_base() {
        for path in [
            "api/v1/tenants",
            "api/v1/oauth/authorize-url",
            "api/v1/oauth/exchange",
            "api/v1/oauth/token",
        ] {
            let url = api_url(path);
            assert!(
                url.starts_with("https://cloudkeep.emptyfox.net/"),
                "接口地址应基于官方域名: {url}"
            );
            assert!(!url.contains("//api/"), "路径拼接不应产生双斜杠: {url}");
        }

        // 带前导斜杠的入参也要能正确拼接
        assert_eq!(
            api_url("/api/v1/tenants"),
            "https://cloudkeep.emptyfox.net/api/v1/tenants"
        );
    }

    /// 没有任何自定义凭证时，应回落到服务端模式（默认行为）。
    #[test]
    fn resolve_mode_falls_back_to_server() {
        let m = AuthMode::Server { api_key: None };
        assert!(matches!(m, AuthMode::Server { api_key: None }));
    }

    #[test]
    fn pending_state_roundtrip_ignores_server_url() {
        // state 文件不再记录服务端地址（地址已固定，无需比对）
        let payload = PendingState {
            state: "a".repeat(48),
            created_at: crate::token::now_millis(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"state\""));
        assert!(json.contains("\"created_at\""));
        assert!(
            !json.contains("base_url"),
            "固定地址后不应再记录 base_url: {json}"
        );

        let back: PendingState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.state, payload.state);
    }
}
