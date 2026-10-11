//! 构建脚本：把服务端地址注入编译期常量。
//!
//! 主程序默认通过 CloudKeep-Server 获取 Access Token，因此需要一个内置的服务端
//! 地址。该地址在**构建时**确定，来源优先级如下：
//!
//! 1. 构建进程环境变量 `CLOUDKEEP_DEFAULT_SERVER_URL`
//! 2. 项目根目录的 `.env` 文件（键名同上）
//! 3. 兜底默认值 `http://127.0.0.1:49494`
//!
//! 之所以走 build.rs 而不是直接 `env!`：Cargo 不会读取 `.env`，只认进程环境变量。
//! 有了这一层，只需在 `.env` 里写一行就能改内置地址，不必在 CI 里额外配置。
//!
//! 使用者仍可用运行时环境变量 `CLOUDKEEP_SERVER_URL` 覆盖内置值。

use std::path::Path;

/// 兜底地址：仅当未配置时使用。
///
/// 指向本机 49494（服务端默认端口）。这样"自建服务端 + 同机客户端"开箱可用；
/// 若要分发给他人使用，请在构建时的 `.env` 里填写真实地址。
const FALLBACK_URL: &str = "http://127.0.0.1:49494";

const KEY: &str = "CLOUDKEEP_DEFAULT_SERVER_URL";

fn main() {
    // 这些文件的改动都应当触发重新构建，保证注入的地址跟着更新
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.env");
    println!("cargo:rerun-if-env-changed={KEY}");

    let url = resolve_url();
    println!("cargo:rustc-env={KEY}={url}");
}

fn resolve_url() -> String {
    // 1) 构建进程环境变量
    if let Some(v) = std::env::var(KEY).ok().and_then(clean) {
        return v;
    }

    // 2) .env 文件（仓库根 = 本脚本所在目录）
    let env_path = Path::new(env!("CARGO_MANIFEST_DIR")).join(".env");
    if let Some(v) = read_env_file(&env_path).and_then(clean) {
        return v;
    }

    FALLBACK_URL.to_string()
}

/// 去掉空白与结尾斜杠；空值视为未设置。
fn clean(raw: String) -> Option<String> {
    let v = raw.trim().trim_end_matches('/').to_string();
    if v.is_empty() { None } else { Some(v) }
}

/// 从 `.env` 文本里取出目标键。解析保持极简：仅支持 `KEY=VALUE` 与 `#` 注释，
/// 与 dotenvy 的行为在本用途上一致，避免为构建脚本再引入依赖。
fn read_env_file(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == KEY {
            let v = v.trim();
            // 兼容 KEY="value" 这类带引号的写法
            let v = v.trim_matches(|c| c == '"' || c == '\'');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}
