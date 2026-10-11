//! 容器健康检查（容器 HEALTHCHECK 调用）。
//!
//! 心跳文件（`WORK_DIR/heartbeat`）超过 `HEALTHCHECK_MAX_AGE_HOURS`
//! （默认 26）小时未刷新即视为不健康——调度进程已死或定时备份长时间未触发。
//! 退出码 0 健康，1 不健康。

use std::path::{Path, PathBuf};

/// 心跳最大允许年龄（小时）。
pub fn max_age_hours() -> f64 {
    std::env::var("HEALTHCHECK_MAX_AGE_HOURS")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(26.0)
}

pub fn heartbeat_file() -> PathBuf {
    let work_dir = std::env::var("WORK_DIR")
        .ok()
        .filter(|w| !w.trim().is_empty())
        .unwrap_or_else(|| crate::config::default_work_dir().to_string_lossy().to_string());
    PathBuf::from(work_dir).join("heartbeat")
}

/// 给定心跳文件路径与阈值，判断是否新鲜。
pub fn heartbeat_is_fresh(path: &Path, max_age_hours: f64) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(age) = meta.modified().map(|m| m.elapsed()) else {
        return false;
    };
    match age {
        Ok(age) => age.as_secs_f64() <= max_age_hours * 3600.0,
        Err(_) => true, // mtime 在未来（时钟回拨），视为健康
    }
}

/// 检查默认位置的心跳是否新鲜。
pub fn is_healthy(max_age_hours: f64) -> bool {
    heartbeat_is_fresh(&heartbeat_file(), max_age_hours)
}

/// 执行健康检查并返回进程退出码。
pub fn run() -> i32 {
    i32::from(!is_healthy(max_age_hours()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_mtime(path: &std::path::Path, time: std::time::SystemTime) {
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(time))
            .expect("设置 mtime 失败");
    }

    #[test]
    fn missing_heartbeat_is_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!heartbeat_is_fresh(&dir.path().join("heartbeat"), 26.0));
    }

    #[test]
    fn fresh_heartbeat_is_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat");
        std::fs::write(&path, "now").unwrap();

        assert!(heartbeat_is_fresh(&path, 26.0));
    }

    #[test]
    fn stale_heartbeat_is_unhealthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat");
        std::fs::write(&path, "old").unwrap();

        set_mtime(&path, std::time::SystemTime::now() - std::time::Duration::from_secs(100 * 3600));

        assert!(!heartbeat_is_fresh(&path, 26.0), "100 小时未刷新应判定为不健康");
    }

    #[test]
    fn threshold_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat");
        std::fs::write(&path, "x").unwrap();
        set_mtime(&path, std::time::SystemTime::now() - std::time::Duration::from_secs(50 * 3600));

        assert!(!heartbeat_is_fresh(&path, 26.0), "50h > 26h 应不健康");
        assert!(heartbeat_is_fresh(&path, 100.0), "50h < 100h 应健康");
    }

    #[test]
    fn boundary_exactly_at_threshold_is_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat");
        std::fs::write(&path, "x").unwrap();

        // 留 5 秒余量，避免调度抖动导致边界不稳定
        set_mtime(&path, std::time::SystemTime::now() - std::time::Duration::from_secs(26 * 3600 - 5));
        assert!(heartbeat_is_fresh(&path, 26.0), "阈值内应视为健康");
    }

    #[test]
    fn future_mtime_is_treated_as_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("heartbeat");
        std::fs::write(&path, "x").unwrap();

        set_mtime(&path, std::time::SystemTime::now() + std::time::Duration::from_secs(3600));
        assert!(heartbeat_is_fresh(&path, 26.0), "时钟回拨不应误判为不健康");
    }

    #[test]
    fn heartbeat_file_is_under_work_dir() {
        unsafe { std::env::set_var("WORK_DIR", "/var/lib/cloudkeep") };
        let path = heartbeat_file();
        unsafe { std::env::remove_var("WORK_DIR") };

        assert!(path.ends_with("heartbeat"), "实际: {}", path.display());
    }

    #[test]
    fn max_age_defaults_and_rejects_invalid() {
        unsafe { std::env::remove_var("HEALTHCHECK_MAX_AGE_HOURS") };
        assert_eq!(max_age_hours(), 26.0, "未设置时应回落到 26");

        unsafe { std::env::set_var("HEALTHCHECK_MAX_AGE_HOURS", "abc") };
        assert_eq!(max_age_hours(), 26.0, "非法值应回落到默认");

        unsafe { std::env::set_var("HEALTHCHECK_MAX_AGE_HOURS", "0") };
        assert_eq!(max_age_hours(), 26.0, "0 或负数应回落到默认");

        unsafe { std::env::set_var("HEALTHCHECK_MAX_AGE_HOURS", "12") };
        assert_eq!(max_age_hours(), 12.0);

        unsafe { std::env::remove_var("HEALTHCHECK_MAX_AGE_HOURS") };
    }
}
