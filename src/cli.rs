//! 命令行入口：子命令解析与各命令分发。
//!
//! 命令既可用子命令（`backup` / `auth` 这类），也兼容旧的 `--flag` 写法；
//! 无法识别的参数直接报错，不会静默进入调度模式。

use anyhow::Result;
use clap::{Parser, Subcommand};
use reqwest::Client;

use crate::api::PanClient;
use crate::auth::{build_auth_url, login_with_code};
use crate::backup::run_backup;
use crate::config::Config;
use crate::healthcheck;
use crate::remote::ensure_remote_dir;
use crate::scheduler;
use crate::token::ensure_access_token;
use crate::{log_err, log_info};

/// 备份服务名（用于日志）
const SERVICE: &str = "cloudkeep";

#[derive(Parser, Debug)]
#[command(
    name = "cloudkeep",
    version,
    about = "CloudKeep - 百度网盘定时备份服务",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// 兼容旧的 flag 写法：`--once` 等价于 `backup`
    #[arg(long = "once", hide = true)]
    once: bool,

    #[arg(long = "mkdir", hide = true)]
    mkdir: bool,

    #[arg(long = "auth", hide = true)]
    auth_flag: bool,

    #[arg(long = "auth-url", hide = true)]
    auth_url_flag: bool,

    /// 兼容旧的 `--auth-code <code>` 写法
    #[arg(long = "auth-code", value_name = "CODE", hide = true)]
    auth_code_flag: Option<String>,

    #[arg(long = "healthcheck", hide = true)]
    healthcheck_flag: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 启动定时调度（默认）
    Run,
    /// 备份一次后退出
    Backup {
        /// 只打包不上传
        #[arg(long)]
        dry_run: bool,
    },
    /// 确保网盘目标目录存在
    EnsureDir,
    /// 打印授权链接（无 TTY 授权第 1 步）
    AuthUrl,
    /// 用授权码换取 token（无 TTY 授权第 2 步）
    AuthCode {
        /// 浏览器授权后拿到的 code，也可用环境变量 BAIDU_AUTH_CODE
        code: Option<String>,
    },
    /// 显示授权指引（同 auth-url）
    Auth,
    /// 健康检查（供容器 HEALTHCHECK 调用）
    Healthcheck,
    /// 显示帮助
    Help,
}

impl Cli {
    /// 执行命令，返回进程退出码。
    pub async fn run(self) -> i32 {
        match self.dispatch().await {
            Ok(code) => code,
            Err(err) => {
                log_err!("{err:#}");
                1
            }
        }
    }

    async fn dispatch(self) -> Result<i32> {
        // .env 只在存在时加载，缺失不报错（纯环境变量部署同样可用）
        let _ = dotenvy::dotenv();

        let command = self.normalize_command();
        let Some(command) = command else {
            return Ok(0);
        };

        let http = build_http_client()?;

        match command {
            Command::Help => {
                print_help();
                Ok(0)
            }

            Command::AuthUrl | Command::Auth => {
                // 授权命令不要求 BACKUP_SOURCE 等备份配置
                auth_url(&http).await
            }

            Command::AuthCode { code } => {
                let code = code
                    .or_else(|| std::env::var("BAIDU_AUTH_CODE").ok())
                    .unwrap_or_default();

                auth_code(&http, &code).await
            }

            Command::Backup { dry_run } => match do_backup(&http, dry_run).await {
                Ok(()) => Ok(0),
                Err(err) => {
                    log_err!("备份失败: {err:#}");
                    Ok(1)
                }
            },

            Command::EnsureDir => {
                let cfg = Config::from_env()?;
                let client = PanClient::new(http);
                let token = ensure_access_token(client.http()).await?;

                match ensure_remote_dir(&client, &token, &cfg.remote_dir).await {
                    Ok(dir) => {
                        log_info!("网盘目录已就绪: {dir}");
                        Ok(0)
                    }
                    Err(err) => {
                        log_err!("创建网盘目录失败: {err:#}");
                        Ok(1)
                    }
                }
            }

            Command::Healthcheck => {
                let max_age = healthcheck::max_age_hours();
                Ok(i32::from(!healthcheck::is_healthy(max_age)))
            }

            Command::Run => do_run(http).await,
        }
    }

    /// 把子命令与兼容 flag 归一化。
    fn normalize_command(self) -> Option<Command> {
        match self.command {
            Some(cmd) => Some(cmd),
            None => {
                if self.once {
                    Some(Command::Backup { dry_run: false })
                } else if self.mkdir {
                    Some(Command::EnsureDir)
                } else if self.auth_flag {
                    Some(Command::Auth)
                } else if self.auth_url_flag {
                    Some(Command::AuthUrl)
                } else if let Some(code) = self.auth_code_flag {
                    Some(Command::AuthCode { code: Some(code) })
                } else if self.healthcheck_flag {
                    Some(Command::Healthcheck)
                } else {
                    Some(Command::Run)
                }
            }
        }
    }
}

/// 单次备份：加载配置 → 执行。
async fn do_backup(http: &Client, dry_run: bool) -> Result<()> {
    let cfg = Config::from_env()?;
    let client = PanClient::new(http.clone());
    run_backup(&client, &cfg, Some(dry_run)).await?;
    Ok(())
}

/// 授权第 1 步：打印授权链接。
///
/// 两种模式：
/// - 自定义百度凭证 → 本地生成链接（原有行为）；
/// - 服务端模式 → 向服务端索取链接，并把返回的 `state` 暂存下来，
///   供 `auth-code` 第 2 步一起提交（服务端的 state 是一次性的，且绑定租户）。
async fn auth_url(http: &Client) -> Result<i32> {
    use crate::server::{self, AuthMode};

    match server::resolve_mode() {
        AuthMode::CustomBaiduCredentials { .. } => match build_auth_url("oob") {
            Ok(url) => {
                println!("① 在浏览器打开下面的链接，登录百度账号并授权：\n   {url}\n");
                println!("② 授权后页面会显示 code，执行：\n   cloudkeep auth-code <code>");
                Ok(0)
            }
            Err(err) => {
                log_err!("生成授权链接失败: {err:#}");
                Ok(1)
            }
        },
        AuthMode::Server { base_url, api_key } => {
            let result = async {
                let api_key = server::ensure_api_key(http, &base_url, api_key).await?;
                server::fetch_authorize_url(http, &base_url, &api_key).await
            }
            .await;

            match result {
                Ok(resp) => {
                    // 暂存 state：服务端要求 exchange 时同时提交 code 与 state
                    if let Err(e) = server::save_pending_state(&resp.state) {
                        log_err!("保存授权 state 失败: {e:#}");
                        return Ok(1);
                    }
                    println!("① 在浏览器打开下面的链接，登录百度账号并授权：\n   {}\n", resp.url);
                    println!("② 授权后页面会显示 code，执行：\n   cloudkeep auth-code <code>");
                    println!("   （state 已自动保存，无需手动填写）");
                    Ok(0)
                }
                Err(err) => {
                    log_err!("生成授权链接失败: {err:#}");
                    Ok(1)
                }
            }
        }
    }
}

/// 授权第 2 步：用 code 换 token。
async fn auth_code(http: &Client, code: &str) -> Result<i32> {
    use crate::server::{self, AuthMode};

    match server::resolve_mode() {
        AuthMode::CustomBaiduCredentials { .. } => match login_with_code(http, code).await {
            Ok(result) => {
                log_info!("授权成功，Token 已保存到 {}", result.config_file.display());
                let expires_at = chrono::DateTime::from_timestamp_millis(result.expires_at)
                    .unwrap_or_else(chrono::Utc::now);
                log_info!(
                    "Access Token 有效期至 {}（过期后会自动刷新）",
                    expires_at.to_rfc3339()
                );
                Ok(0)
            }
            Err(err) => {
                log_err!("授权失败: {err:#}");
                Ok(1)
            }
        },
        AuthMode::Server { base_url, api_key } => {
            let Some(state) = server::load_pending_state() else {
                log_err!(
                    "本地没有待用的授权 state。请先执行 `cloudkeep auth-url` 获取授权链接\
                     （state 与链接是配套的，且有时效）"
                );
                return Ok(1);
            };

            let api_key = match server::ensure_api_key(http, &base_url, api_key).await {
                Ok(k) => k,
                Err(err) => {
                    log_err!("获取 API Key 失败: {err:#}");
                    return Ok(1);
                }
            };

            match server::exchange_code(http, &base_url, &api_key, code, &state).await {
                Ok(()) => {
                    // state 已被服务端消费，本地副本即可清掉
                    let _ = server::clear_pending_state();
                    log_info!("授权成功：服务端已保存该租户的百度授权");
                    log_info!("之后备份会自动取用 token，过期由服务端刷新");
                    Ok(0)
                }
                Err(err) => {
                    log_err!("授权失败: {err:#}");
                    Ok(1)
                }
            }
        }
    }
}

/// 启动定时调度。
async fn do_run(http: Client) -> Result<i32> {
    let cfg = Config::from_env()?;

    log_info!("百度网盘备份服务启动（{SERVICE} v{}）", crate::VERSION);
    log_info!("定时规则: {}（时区 {}）", cfg.cron, cfg.timezone);
    log_info!("备份源: {}", cfg.sources.join(", "));
    log_info!("网盘目录: {}/<目录名>/（每个目录单独打包）", cfg.remote_dir);
    log_info!(
        "保留份数: {} | 上传并发: {} | DRY_RUN: {}",
        if cfg.retention > 0 {
            cfg.retention.to_string()
        } else {
            "不清理".to_string()
        },
        cfg.concurrency,
        cfg.dry_run
    );

    let run_on_start = cfg.run_on_start;
    let client = PanClient::new(http);
    scheduler::serve(client, cfg, run_on_start).await?;
    Ok(0)
}

/// 构建 HTTP 客户端（统一走 `http` 模块，确保加密后端已安装）。
fn build_http_client() -> Result<Client> {
    crate::http::build_client()
}

fn print_help() {
    println!(
        "\
CloudKeep - 百度网盘定时备份服务

用法:
  cloudkeep [命令]

命令:
  run               启动调度（默认）
  backup            备份一次后退出
  ensure-dir        确保网盘目标目录存在
  auth-url          打印授权链接（无 TTY 授权第 1 步）
  auth-code <code>  用授权码换 token（第 2 步）
  auth              显示授权指引（等同于 auth-url）
  healthcheck       健康检查（供容器使用）
  help              显示本帮助

选项:
  --dry-run         只打包不上传（配合 backup 子命令）
  -V, --version     显示版本
  -h, --help        显示帮助

授权（推荐两步走，无需 TTY）:
  cloudkeep auth-url                    # ① 在浏览器打开输出的链接并授权
  cloudkeep auth-code <code>            # ② 把页面上的 code 填进来

直接运行二进制:
  cloudkeep run                         # 定时备份（需配置 CRON_SCHEDULE）
  cloudkeep backup                      # 手动备份一次

Docker:
  docker compose up -d                  # 启动定时备份
  docker compose logs -f                # 查看日志
  docker exec cloudkeep backup          # 对运行中的容器手动备份一次
  docker exec cloudkeep auth-url        # 打印授权链接

环境变量:
  配置项见 .env.example。"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_subcommands() {
        assert!(matches!(
            Cli::parse_from(["cloudkeep", "run"]).command,
            Some(Command::Run)
        ));
        assert!(matches!(
            Cli::parse_from(["cloudkeep", "backup"]).command,
            Some(Command::Backup { dry_run: false })
        ));
        assert!(matches!(
            Cli::parse_from(["cloudkeep", "backup", "--dry-run"]).command,
            Some(Command::Backup { dry_run: true })
        ));
        assert!(matches!(
            Cli::parse_from(["cloudkeep", "ensure-dir"]).command,
            Some(Command::EnsureDir)
        ));
        assert!(matches!(
            Cli::parse_from(["cloudkeep", "auth-url"]).command,
            Some(Command::AuthUrl)
        ));
        assert!(matches!(
            Cli::parse_from(["cloudkeep", "help"]).command,
            Some(Command::Help)
        ));
    }

    #[test]
    fn parses_auth_code_with_value() {
        let cli = Cli::parse_from(["cloudkeep", "auth-code", "abc123"]);
        match cli.command {
            Some(Command::AuthCode { code }) => assert_eq!(code.as_deref(), Some("abc123")),
            other => panic!("解析失败: {other:?}"),
        }
    }

    #[test]
    fn auth_code_accepts_env_var() {
        let cli = Cli::parse_from(["cloudkeep", "auth-code"]);
        match cli.command {
            Some(Command::AuthCode { code }) => {
                assert!(code.is_none(), "未传参数时应回落到环境变量")
            }
            other => panic!("解析失败: {other:?}"),
        }
    }

    #[test]
    fn legacy_flags_map_to_commands() {
        let cli = Cli::parse_from(["cloudkeep", "--once"]);
        assert!(cli.once);
        assert!(matches!(
            cli.normalize_command(),
            Some(Command::Backup { dry_run: false })
        ));

        let cli = Cli::parse_from(["cloudkeep", "--mkdir"]);
        assert!(matches!(cli.normalize_command(), Some(Command::EnsureDir)));

        let cli = Cli::parse_from(["cloudkeep", "--auth-url"]);
        assert!(matches!(cli.normalize_command(), Some(Command::AuthUrl)));

        let cli = Cli::parse_from(["cloudkeep", "--auth"]);
        assert!(matches!(cli.normalize_command(), Some(Command::Auth)));

        let cli = Cli::parse_from(["cloudkeep", "--healthcheck"]);
        assert!(matches!(cli.normalize_command(), Some(Command::Healthcheck)));
    }

    #[test]
    fn legacy_auth_code_flag_carries_value() {
        let cli = Cli::parse_from(["cloudkeep", "--auth-code", "abc123"]);
        match cli.normalize_command() {
            Some(Command::AuthCode { code }) => assert_eq!(code.as_deref(), Some("abc123")),
            other => panic!("解析失败: {other:?}"),
        }

        // 也支持 --auth-code=abc123
        let cli = Cli::parse_from(["cloudkeep", "--auth-code=xyz"]);
        match cli.normalize_command() {
            Some(Command::AuthCode { code }) => assert_eq!(code.as_deref(), Some("xyz")),
            other => panic!("解析失败: {other:?}"),
        }
    }

    #[test]
    fn no_args_defaults_to_run() {
        let cli = Cli::parse_from(["cloudkeep"]);
        assert!(cli.command.is_none());
        assert!(matches!(cli.normalize_command(), Some(Command::Run)));
    }

    #[test]
    fn unknown_command_is_rejected() {
        // 命令拼错（如 bakcup）必须报错，不能静默进入调度模式
        assert!(Cli::try_parse_from(["cloudkeep", "bakcup"]).is_err());
        assert!(Cli::try_parse_from(["cloudkeep", "mkdi"]).is_err());
    }

    #[test]
    fn help_text_lists_all_commands() {
        let help = Cli::command().render_long_help().to_string();
        for cmd in [
            "run", "backup", "ensure-dir", "auth-url", "auth-code", "auth", "healthcheck", "help",
        ] {
            assert!(help.contains(cmd), "帮助应包含命令 {cmd}");
        }
    }
}
