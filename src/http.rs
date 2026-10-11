//! HTTP 客户端构建。
//!
//! 使用 `rustls` 的 `ring` 后端：**必须在创建任何 `reqwest::Client` 之前**
//! 安装全局加密后端，否则 reqwest 会在构造 Client 时 panic。
//! 因此所有 Client 都应通过本模块的 [`build_client`] 创建。

use std::sync::Once;
use std::time::Duration;

use anyhow::{Context, Result};

/// 安装 rustls 的加密后端（ring）。幂等，可重复调用。
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // 已被安装过（如宿主程序自行安装）时返回 Err，属正常情况
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// 构建 HTTP 客户端：复用连接池、跟随代理环境变量、设置合理超时。
pub fn build_client() -> Result<reqwest::Client> {
    install_crypto_provider();

    reqwest::Client::builder()
        .user_agent(crate::token::USER_AGENT)
        .connect_timeout(Duration::from_secs(15))
        // 上传大文件不设整体超时，靠重试与健康检查兜底
        .timeout(Duration::from_secs(1800))
        .pool_idle_timeout(Duration::from_secs(90))
        .build()
        .context("创建 HTTP 客户端失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent() {
        install_crypto_provider();
        install_crypto_provider();
    }

    #[test]
    fn build_client_succeeds() {
        let client = build_client().unwrap();
        let _ = client;
    }

    #[test]
    fn build_client_can_run_without_explicit_install() {
        // 直接调用也应自动安装后端（回归用例：缺失时会 panic）
        let client = build_client().expect("应能创建客户端");
        drop(client);
    }
}
