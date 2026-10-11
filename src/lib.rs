//! CloudKeep —— 百度网盘定时备份服务。
//!
//! 纯 Rust 实现，单个静态二进制即可运行：
//! 打包用 `tar` + `flate2`，上传直连百度网盘开放平台接口。

pub mod api;
pub mod archive;
pub mod auth;
pub mod backup;
pub mod cli;
pub mod config;
pub mod cron;
pub mod healthcheck;
pub mod http;
pub mod logger;
pub mod remote;
pub mod scheduler;
pub mod server;
pub mod token;
pub mod upload;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
