//! 定时调度与优雅停机。
//!
//! - 单实例互斥：上一次备份未结束时会跳过本次触发。
//! - 停机：收到 SIGTERM / SIGINT 后停止调度，等待进行中的备份自然结束。

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use tokio::task::JoinHandle;

use crate::api::PanClient;
use crate::backup::run_backup;
use crate::config::Config;
use crate::cron::CronSchedule;
use crate::{log_err, log_info, log_warn};

/// 心跳文件：启动与每次备份后刷新，容器 HEALTHCHECK 据此判断存活。
pub struct Heartbeat {
    path: std::path::PathBuf,
}

impl Heartbeat {
    pub fn new(work_dir: &std::path::Path) -> Self {
        Heartbeat { path: work_dir.join("heartbeat") }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn touch(&self) {
        if let Some(dir) = self.path.parent()
            && let Err(err) = std::fs::create_dir_all(dir)
        {
            log_warn!("创建心跳目录失败 {}: {err}", dir.display());
            return;
        }

        if let Err(err) = std::fs::write(&self.path, Utc::now().to_rfc3339()) {
            log_warn!("写入心跳文件失败: {err}");
        }
    }
}

/// 备份触发器：负责单实例互斥与心跳。
pub struct Runner {
    heartbeat: Arc<Heartbeat>,
    running: Arc<tokio::sync::Mutex<bool>>,
}

impl Runner {
    pub fn new(work_dir: &std::path::Path) -> Self {
        Runner {
            heartbeat: Arc::new(Heartbeat::new(work_dir)),
            running: Arc::new(tokio::sync::Mutex::new(false)),
        }
    }

    pub fn heartbeat(&self) -> Arc<Heartbeat> {
        Arc::clone(&self.heartbeat)
    }

    /// 触发一次备份；上一次未结束时跳过本次触发。
    pub async fn trigger(
        self: &Arc<Self>,
        client: PanClient,
        cfg: Arc<Config>,
        reason: &str,
    ) -> bool {
        {
            let mut running = self.running.lock().await;
            if *running {
                log_warn!("{reason}: 上一次备份尚未结束，本次跳过");
                return true;
            }
            *running = true;
        }

        let result = run_backup(&client, &cfg, None).await;
        self.heartbeat.touch();
        *self.running.lock().await = false;

        match result {
            Ok(summary) => {
                log_info!(
                    "{reason}: 备份完成，用时 {:.1}s",
                    summary.duration_ms as f64 / 1000.0
                );
                true
            }
            Err(err) => {
                log_err!("{reason}: 备份失败 - {err:#}");
                false
            }
        }
    }

    /// 是否有备份正在运行。
    pub async fn is_running(&self) -> bool {
        *self.running.lock().await
    }
}

/// 监听 SIGTERM（Unix）。
#[cfg(unix)]
async fn terminate_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut stream) => {
            stream.recv().await;
        }
        Err(_) => std::future::pending::<()>().await,
    }
}

#[cfg(not(unix))]
async fn terminate_signal() {
    std::future::pending::<()>().await
}

/// 启动调度并阻塞运行，直到收到停止信号。
pub async fn serve(client: PanClient, cfg: Config, run_on_start: bool) -> Result<()> {
    let schedule = CronSchedule::parse(&cfg.cron)?;
    let tz: Tz = cfg
        .timezone
        .parse()
        .map_err(|_| anyhow::anyhow!("无法解析时区: {}", cfg.timezone))?;

    let cfg = Arc::new(cfg);
    let runner = Arc::new(Runner::new(&cfg.work_dir));

    runner.heartbeat().touch();

    let mut handles: Vec<JoinHandle<()>> = Vec::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut next_run: Option<DateTime<Tz>> = schedule.next_after(Utc::now().with_timezone(&tz), tz);
    if let Some(at) = next_run {
        log_info!("下次执行时间: {}", at.format("%Y-%m-%d %H:%M:%S %Z"));
    }

    if run_on_start {
        log_info!("RUN_ON_START=true，立即执行一次备份");
        spawn_backup(&mut handles, client.clone(), Arc::clone(&cfg), Arc::clone(&runner), "启动备份");
    }

    loop {
        tokio::select! {
            signal = wait_for_signal() => {
                log_info!("收到 {signal}，停止调度并等待当前任务结束");
                // 不 abort：让进行中的备份自然跑完（最长 10 分钟），避免上传到一半被打断
                drain_running(&runner).await;
                for handle in handles {
                    let _ = handle.await;
                }
                return Ok(());
            }
            _ = ticker.tick() => {
                let now: DateTime<Tz> = Utc::now().with_timezone(&tz);
                let due = next_run.is_some_and(|at| now >= at);

                if due {
                    next_run = schedule.next_after(now, tz);
                    if let Some(next) = next_run {
                        log_info!("下次执行时间: {}", next.format("%Y-%m-%d %H:%M:%S %Z"));
                    }
                    spawn_backup(&mut handles, client.clone(), Arc::clone(&cfg), Arc::clone(&runner), "定时备份");
                }
            }
        }
    }
}

fn spawn_backup(
    handles: &mut Vec<JoinHandle<()>>,
    client: PanClient,
    cfg: Arc<Config>,
    runner: Arc<Runner>,
    reason: &'static str,
) {
    handles.push(tokio::spawn(async move {
        runner.trigger(client, cfg, reason).await;
    }));

    // 回收已结束的句柄，避免长跑后无限增长
    handles.retain(|h| !h.is_finished());
}

/// 等待 SIGTERM / SIGINT。
async fn wait_for_signal() -> &'static str {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => "SIGINT",
        _ = terminate_signal() => "SIGTERM",
    }
}

/// 等待进行中的备份结束（最多 10 分钟，与 compose 的 stop_grace_period 对齐）。
async fn drain_running(runner: &Runner) {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        if !runner.is_running().await {
            return;
        }
        if Instant::now() >= deadline {
            log_warn!("等待当前备份超时，强制退出");
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_path_is_under_work_dir() {
        let hb = Heartbeat::new(std::path::Path::new("/tmp/cloudkeep"));
        assert_eq!(hb.path(), std::path::Path::new("/tmp/cloudkeep/heartbeat"));
    }

    #[test]
    fn heartbeat_touch_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let hb = Heartbeat::new(dir.path());
        hb.touch();
        assert!(hb.path().exists(), "应创建心跳文件");
        assert!(hb.path().metadata().unwrap().len() > 0);
    }

    #[test]
    fn heartbeat_touch_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let hb = Heartbeat::new(dir.path());
        hb.touch();
        hb.touch();
        assert!(hb.path().exists());
    }

    #[tokio::test]
    async fn runner_reports_not_running_initially() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(dir.path());
        assert!(!runner.is_running().await);
    }
}
