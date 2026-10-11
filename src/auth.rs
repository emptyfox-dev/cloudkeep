//! 无 TTY 授权：生成授权链接、用 code 换 token。

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::token::{
    CliConfig, OPENAPI_URL, USER_AGENT, config_file, get_app_credentials, now_millis, save_cli_config,
};

const DEFAULT_REDIRECT_URI: &str = "oob";
const SCOPE: &str = "basic,netdisk";

/// 生成授权链接。适用于无浏览器 / 无 TTY 的环境（服务器、NAS、ARM 设备）。
pub fn build_auth_url(redirect_uri: &str) -> Result<String> {
    let (app_key, _) = get_app_credentials();
    let Some(app_key) = app_key else {
        bail!("缺少 BAIDU_APP_KEY，请先在 .env 中配置百度开放平台 App Key");
    };

    // 固定顺序，保证每次生成的链接一致、便于用户比对
    let query = [
        ("response_type", "code"),
        ("client_id", app_key.as_str()),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", encode(v)))
    .collect::<Vec<_>>()
    .join("&");

    Ok(format!("{OPENAPI_URL}/oauth/2.0/authorize?{query}"))
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug)]
pub struct LoginResult {
    pub expires_at: i64,
    pub config_file: std::path::PathBuf,
}

/// 用授权码换取 token，并保存到授权文件。
pub async fn login_with_code(client: &reqwest::Client, code: &str) -> Result<LoginResult> {
    let (app_key, secret_key) = get_app_credentials();
    let (Some(app_key), Some(secret_key)) = (app_key, secret_key) else {
        bail!("缺少 BAIDU_APP_KEY / BAIDU_SECRET_KEY，请先在 .env 中配置");
    };

    let auth_code = code.trim();
    if auth_code.is_empty() {
        bail!("授权码不能为空。用法: cloudkeep auth-code <code>");
    }

    let resp = client
        .get(format!("{OPENAPI_URL}/oauth/2.0/token"))
        .query(&[
            ("grant_type", "authorization_code"),
            ("code", auth_code),
            ("client_id", &app_key),
            ("client_secret", &secret_key),
            ("redirect_uri", DEFAULT_REDIRECT_URI),
        ])
        .header("User-Agent", USER_AGENT)
        .send()
        .await
        .context("请求百度授权接口失败")?;

    let status = resp.status();
    let text = resp.text().await.context("读取授权响应失败")?;

    let data: TokenResponse = serde_json::from_str(&text)
        .with_context(|| format!("授权响应无法解析（HTTP {status}）: {}", &text[..text.len().min(200)]))?;

    let Some(access_token) = data.access_token.filter(|v| !v.is_empty()) else {
        let reason = data
            .error_description
            .or(data.error)
            .unwrap_or_else(|| format!("HTTP {status}"));
        bail!("换取 Token 失败: {reason}");
    };

    let expires_at = now_millis() + data.expires_in.unwrap_or(0) * 1000;

    save_cli_config(&CliConfig {
        access_token: Some(access_token),
        refresh_token: data.refresh_token,
        app_key: Some(app_key),
        secret_key: Some(secret_key),
        expires_at: Some(expires_at),
    })?;

    Ok(LoginResult { expires_at, config_file: config_file() })
}

fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这些测试会改动进程级环境变量，必须串行执行。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 在隔离的配置目录 + 指定 BAIDU_APP_KEY 下运行
    fn with_env<T>(app_key: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let dir = tempfile::tempdir().unwrap();
        let prev_dir = std::env::var("CLOUDKEEP_CONFIG_DIR").ok();
        let prev_key = std::env::var("BAIDU_APP_KEY").ok();

        unsafe { std::env::set_var("CLOUDKEEP_CONFIG_DIR", dir.path()) };
        match app_key {
            Some(k) => unsafe { std::env::set_var("BAIDU_APP_KEY", k) },
            None => unsafe { std::env::remove_var("BAIDU_APP_KEY") },
        }

        let out = f();

        match prev_dir {
            Some(v) => unsafe { std::env::set_var("CLOUDKEEP_CONFIG_DIR", v) },
            None => unsafe { std::env::remove_var("CLOUDKEEP_CONFIG_DIR") },
        }
        match prev_key {
            Some(v) => unsafe { std::env::set_var("BAIDU_APP_KEY", v) },
            None => unsafe { std::env::remove_var("BAIDU_APP_KEY") },
        }
        out
    }

    #[test]
    fn encoding_matches_oauth_expectations() {
        assert_eq!(encode("abc123"), "abc123");
        assert_eq!(encode("basic,netdisk"), "basic%2Cnetdisk");
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("a/b+c"), "a%2Fb%2Bc");
        assert_eq!(encode("中国"), "%E4%B8%AD%E5%9B%BD", "多字节应逐字节百分号编码");
    }

    #[test]
    fn build_auth_url_requires_app_key() {
        with_env(None, || {
            let err = build_auth_url("oob").unwrap_err().to_string();
            assert!(err.contains("BAIDU_APP_KEY"), "实际: {err}");
        });
    }

    #[test]
    fn build_auth_url_contains_required_params() {
        with_env(Some("test-app-key"), || {
            let url = build_auth_url("oob").unwrap();

            assert!(url.starts_with("https://openapi.baidu.com/oauth/2.0/authorize?"));
            assert!(url.contains("response_type=code"), "{url}");
            assert!(url.contains("client_id=test-app-key"), "{url}");
            assert!(url.contains("redirect_uri=oob"), "{url}");
            assert!(url.contains("scope=basic%2Cnetdisk"), "{url}");
        });
    }

    #[test]
    fn build_auth_url_is_deterministic() {
        with_env(Some("k"), || {
            assert_eq!(build_auth_url("oob").unwrap(), build_auth_url("oob").unwrap());
        });
    }

    #[tokio::test]
    // 持有 std::sync guard 跨 await 是故意的：这些测试会改进程级环境变量，必须串行
    #[allow(clippy::await_holding_lock)]
    async fn login_with_code_checks_credentials_before_code() {
        // 凭证缺失时的报错应先于空 code 检查
        let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let prev_dir = std::env::var("CLOUDKEEP_CONFIG_DIR").ok();
        let prev_key = std::env::var("BAIDU_APP_KEY").ok();
        let prev_secret = std::env::var("BAIDU_SECRET_KEY").ok();
        unsafe { std::env::set_var("CLOUDKEEP_CONFIG_DIR", dir.path()) };
        unsafe { std::env::set_var("BAIDU_APP_KEY", "k") };
        unsafe { std::env::remove_var("BAIDU_SECRET_KEY") };

        let client = crate::http::build_client().unwrap();
        let err = login_with_code(&client, "   ").await.unwrap_err().to_string();

        unsafe { std::env::set_var("BAIDU_SECRET_KEY", "s") };
        let empty_code_err = login_with_code(&client, "   ").await.unwrap_err().to_string();

        match prev_dir {
            Some(v) => unsafe { std::env::set_var("CLOUDKEEP_CONFIG_DIR", v) },
            None => unsafe { std::env::remove_var("CLOUDKEEP_CONFIG_DIR") },
        }
        match prev_key {
            Some(v) => unsafe { std::env::set_var("BAIDU_APP_KEY", v) },
            None => unsafe { std::env::remove_var("BAIDU_APP_KEY") },
        }
        match prev_secret {
            Some(v) => unsafe { std::env::set_var("BAIDU_SECRET_KEY", v) },
            None => unsafe { std::env::remove_var("BAIDU_SECRET_KEY") },
        }

        assert!(err.contains("BAIDU_SECRET_KEY"), "实际: {err}");
        assert!(empty_code_err.contains("授权码不能为空"), "实际: {empty_code_err}");
    }
}
