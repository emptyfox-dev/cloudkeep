//! 带时间戳的分级日志，输出到 stdout / stderr。

use std::io::Write;

use chrono::{SecondsFormat, Utc};

fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn emit(level: &str, args: std::fmt::Arguments<'_>) {
    let line = format!("[{}] [{}] {}\n", timestamp(), level, args);

    if level == "ERROR" || level == "WARN" {
        let mut out = std::io::stderr().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    } else {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::logger::emit("INFO", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::logger::emit("WARN", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_err {
    ($($arg:tt)*) => { $crate::logger::emit("ERROR", format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_iso8601_with_millis() {
        let ts = timestamp();
        assert!(ts.ends_with('Z'), "应为 UTC ISO8601: {ts}");
        assert_eq!(ts.len(), "2024-01-02T03:04:05.678Z".len(), "时间戳格式: {ts}");
        assert!(chrono::DateTime::parse_from_rfc3339(&ts).is_ok(), "时间戳可解析: {ts}");
    }

    #[test]
    fn emit_does_not_panic() {
        emit("INFO", format_args!("普通日志 {}", 1));
        emit("WARN", format_args!("警告日志"));
        emit("ERROR", format_args!("错误日志 {}", "原因"));
    }
}
