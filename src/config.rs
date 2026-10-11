//! 读取并校验环境变量配置。

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

fn to_bool(value: Option<String>, fallback: bool) -> bool {
    match value {
        None => fallback,
        Some(v) if v.trim().is_empty() => fallback,
        Some(v) => {
            let v = v.trim().to_ascii_lowercase();
            matches!(v.as_str(), "1" | "true" | "yes" | "on" | "y")
        }
    }
}

fn to_int(value: Option<String>, fallback: i64) -> i64 {
    match value {
        Some(v) => v.trim().parse::<i64>().unwrap_or(fallback),
        None => fallback,
    }
}

fn to_list(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// 网盘目录统一归一化：去掉结尾斜杠，缺失时默认 `/backup`，并补上开头的 `/`。
fn normalize_remote_dir(value: Option<String>) -> String {
    let dir = value.unwrap_or_default().trim().trim_end_matches('/').to_string();
    if dir.is_empty() {
        return "/backup".to_string();
    }
    if dir.starts_with('/') {
        dir
    } else {
        format!("/{dir}")
    }
}

/// 单个备份源目录对应的网盘目标。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// 容器/主机上的本地路径
    pub source: String,
    /// 网盘子目录名
    pub name: String,
    /// `<remoteDir>/<name>`
    pub remote_dir: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub sources: Vec<String>,
    pub remote_dir: String,
    pub excludes: Vec<String>,
    pub cron: String,
    pub timezone: String,
    pub run_on_start: bool,
    pub prefix: String,
    pub work_dir: PathBuf,
    pub retention: i64,
    pub concurrency: usize,
    pub upload_attempts: usize,
    pub dry_run: bool,
    pub app_key: String,
    pub secret_key: String,
    pub healthcheck_max_age_hours: f64,
}

/// 备份源目录按 basename 生成网盘子目录，重名时用上级目录名消歧。
pub fn build_targets(remote_dir: &str, sources: &[String]) -> Vec<Target> {
    let mut used: Vec<String> = Vec::new();
    let mut targets = Vec::new();

    for source in sources {
        let base = basename_of(source);
        let parent = parent_basename_of(source);
        let mut name = base.clone();

        if used.contains(&name) {
            name = format!("{parent}_{base}");
            let mut index = 2;
            while used.contains(&name) {
                name = format!("{parent}_{base}_{index}");
                index += 1;
            }
        }

        used.push(name.clone());
        // 注意不要覆盖入参 remote_dir
        let target_dir = format!("{remote_dir}/{name}");
        targets.push(Target {
            source: source.clone(),
            name,
            remote_dir: target_dir,
        });
    }

    targets
}

/// 按 `/` 与 `\` 两种分隔符切分路径，并忽略 Windows 盘符（`C:`）。
///
/// 备份源可能是 Windows 路径（`D:\data\nginx`）或 POSIX 路径（`/data/nginx`），
/// 两种写法都要能正确取出目录名，否则会生成含 `:`、`\` 的非法网盘目录名。
fn path_segments(p: &str) -> Vec<&str> {
    p.split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .filter(|s| !is_drive_letter(s))
        .collect()
}

/// 形如 `C:` 的盘符不是目录名。
fn is_drive_letter(s: &str) -> bool {
    let mut chars = s.chars();
    matches!((chars.next(), chars.next(), chars.next()),
        (Some(c), Some(':'), None) if c.is_ascii_alphabetic())
}

/// 取路径最后一段；根路径或空路径返回 `root`。
fn basename_of(p: &str) -> String {
    path_segments(p)
        .last()
        .map(|s| s.to_string())
        .unwrap_or_else(|| "root".to_string())
}

/// 取上级目录名；没有上级时返回 `root`。
fn parent_basename_of(p: &str) -> String {
    let segments = path_segments(p);
    if segments.len() >= 2 {
        segments[segments.len() - 2].to_string()
    } else {
        "root".to_string()
    }
}

impl Config {
    /// 从进程环境变量加载配置。`.env` 文件由调用方（`cli`）先加载。
    pub fn from_env() -> Result<Self> {
        let sources = to_list(env_opt("BACKUP_SOURCE").or_else(|| Some("/data".into())));

        let cfg = Config {
            sources,
            remote_dir: normalize_remote_dir(env_opt("BACKUP_REMOTE_DIR")),
            excludes: to_list(env_opt("BACKUP_EXCLUDE")),
            cron: env_opt("CRON_SCHEDULE").filter(|c| !c.is_empty())
                .unwrap_or_else(|| "0 3 * * *".into()),
            timezone: env_opt("CRON_TIMEZONE")
                .filter(|c| !c.is_empty())
                .or_else(|| env_opt("TZ").filter(|c| !c.is_empty()))
                .unwrap_or_else(|| "Asia/Shanghai".into()),
            run_on_start: to_bool(env_opt("RUN_ON_START"), true),
            prefix: env_opt("BACKUP_NAME_PREFIX")
                .filter(|p| !p.is_empty())
                .unwrap_or_else(|| "backup".into()),
            work_dir: env_opt("WORK_DIR")
                .filter(|w| !w.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(default_work_dir),
            retention: to_int(env_opt("BACKUP_RETENTION"), 7),
            concurrency: to_int(env_opt("UPLOAD_CONCURRENCY"), 3).max(1) as usize,
            upload_attempts: to_int(env_opt("UPLOAD_ATTEMPTS"), 2).max(1) as usize,
            dry_run: to_bool(env_opt("DRY_RUN"), false),
            app_key: env_opt("BAIDU_APP_KEY").unwrap_or_default(),
            secret_key: env_opt("BAIDU_SECRET_KEY").unwrap_or_default(),
            healthcheck_max_age_hours: env_opt("HEALTHCHECK_MAX_AGE_HOURS")
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| *v > 0.0)
                .unwrap_or(26.0),
        };

        cfg.validate()?;
        Ok(cfg)
    }

    /// 校验配置项，返回全部问题（不短路）。
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();

        if self.sources.is_empty() {
            problems.push("BACKUP_SOURCE 不能为空".to_string());
        }
        if self.cron.trim().is_empty() {
            problems.push("CRON_SCHEDULE 不能为空".to_string());
        }
        if !is_usable_work_dir(&self.work_dir) {
            problems.push(format!(
                "WORK_DIR 必须是绝对路径: {}（跨平台建议用平台原生写法，如 Windows 的 D:\\\\cloudkeep）",
                self.work_dir.display()
            ));
        }

        if problems.is_empty() {
            Ok(())
        } else {
            bail!(problems.join("\n"))
        }
    }
}

/// 默认工作目录：跟随系统临时目录（Linux 为 `/tmp`，Windows 为 `%TEMP%`）。
///
/// 用 `temp_dir()` 而不是写死 `/tmp`，否则 Windows 上默认值不是绝对路径，
/// 启动时会直接被配置校验拦下。
pub fn default_work_dir() -> PathBuf {
    std::env::temp_dir().join("cloudkeep")
}

/// 判断工作目录是否可用作绝对路径。
///
/// 除了平台原生绝对路径（`/tmp/x`、`D:\x`），也接受以 `/` 或 `\` 开头的
/// POSIX 风格写法，方便容器里写好的配置直接拿到 Windows 上跑。
fn is_usable_work_dir(p: &Path) -> bool {
    if p.is_absolute() {
        return true;
    }
    matches!(
        p.to_string_lossy().chars().next(),
        Some('/') | Some('\\')
    )
}

fn env_opt(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// 校验 WORK_DIR 可用（供命令行在备份前单独调用）。
pub fn ensure_absolute_work_dir(work_dir: &Path) -> Result<()> {
    if !is_usable_work_dir(work_dir) {
        bail!("WORK_DIR 必须是绝对路径: {}", work_dir.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_dir_normalization() {
        assert_eq!(normalize_remote_dir(None), "/backup");
        assert_eq!(normalize_remote_dir(Some("".into())), "/backup");
        assert_eq!(normalize_remote_dir(Some("/".into())), "/backup");
        assert_eq!(normalize_remote_dir(Some("/backup".into())), "/backup");
        assert_eq!(normalize_remote_dir(Some("/backup/docker/".into())), "/backup/docker");
        // 缺前导斜杠时补上
        assert_eq!(normalize_remote_dir(Some("backup/x".into())), "/backup/x");
    }

    #[test]
    fn to_list_splits_and_trims() {
        let v = to_list(Some(" /a , /b ,, /c ".into()));
        assert_eq!(v, vec!["/a", "/b", "/c"]);
        assert!(to_list(None).is_empty());
    }

    #[test]
    fn to_bool_variants() {
        assert!(to_bool(Some("true".into()), false));
        assert!(to_bool(Some("YES".into()), false));
        assert!(to_bool(Some("1".into()), false));
        assert!(to_bool(Some(" on ".into()), false));
        assert!(!to_bool(Some("false".into()), true));
        assert!(!to_bool(Some("no".into()), true));
        // 空值走默认值
        assert!(to_bool(Some("".into()), true));
        assert!(to_bool(None, true));
    }

    #[test]
    fn to_int_invalid_falls_back() {
        assert_eq!(to_int(Some("5".into()), 7), 5);
        assert_eq!(to_int(Some("abc".into()), 7), 7);
        assert_eq!(to_int(None, 7), 7);
        assert_eq!(to_int(Some(" 3 ".into()), 7), 3);
    }

    #[test]
    fn basename_and_parent() {
        assert_eq!(basename_of("/data/nginx"), "nginx");
        assert_eq!(basename_of("/data/nginx/"), "nginx");
        assert_eq!(basename_of("/"), "root");
        assert_eq!(parent_basename_of("/data/nginx"), "data");
        assert_eq!(parent_basename_of("/nginx"), "root");
        assert_eq!(parent_basename_of("/opt/a/data"), "a");
    }

    #[test]
    fn windows_paths_are_parsed() {
        // 反斜杠分隔
        assert_eq!(basename_of(r"D:\data\nginx"), "nginx");
        assert_eq!(basename_of(r"D:\data\nginx\"), "nginx");
        assert_eq!(parent_basename_of(r"D:\data\nginx"), "data");

        // 盘符根目录应回落到 root，而不是产生非法的 "D:" 目录名
        assert_eq!(basename_of(r"D:\"), "root");
        assert_eq!(basename_of("D:/"), "root");
        assert_eq!(parent_basename_of(r"D:\data"), "root");

        // 混合分隔符（Windows 允许）
        assert_eq!(basename_of(r"D:\data/nginx"), "nginx");
        assert_eq!(parent_basename_of(r"D:\data/nginx"), "data");

        // UNC 路径
        assert_eq!(basename_of(r"\\server\share\nginx"), "nginx");
        assert_eq!(parent_basename_of(r"\\server\share\nginx"), "share");
    }

    #[test]
    fn windows_targets_never_contain_illegal_chars() {
        // 回归用例：早期实现只按 '/' 切分，Windows 路径会整段变成目录名，
        // 结果带上 ':' 与 '\'，上传前才被百度接口以 errno=-7 拒绝。
        let t = build_targets("/backup", &[r"D:\data\nginx".into(), r"E:\data\nginx".into()]);
        assert_eq!(t[0].name, "nginx");
        assert_eq!(t[0].remote_dir, "/backup/nginx");
        assert_eq!(t[1].name, "data_nginx", "同名应使用上级目录名消歧");
        assert_eq!(t[1].remote_dir, "/backup/data_nginx");

        for target in &t {
            assert!(
                crate::remote::validate_segment(&target.name).is_ok(),
                "生成的目录名必须是合法网盘目录名: {}",
                target.name
            );
        }
    }

    #[test]
    fn default_work_dir_is_usable() {
        let dir = default_work_dir();
        assert!(is_usable_work_dir(&dir), "默认工作目录必须可用: {}", dir.display());
        assert!(dir.ends_with("cloudkeep"), "实际: {}", dir.display());
    }

    #[test]
    fn work_dir_validation_accepts_both_path_styles() {
        // POSIX 写法在 Windows 上也应接受，方便复用容器里的配置
        assert!(is_usable_work_dir(Path::new("/tmp/cloudkeep")));
        assert!(is_usable_work_dir(Path::new(r"\cloudkeep")));
        assert!(is_usable_work_dir(Path::new(r"D:\cloudkeep")));
        assert!(is_usable_work_dir(Path::new(r"\\server\share\ck")));

        // 相对路径必须拒绝
        assert!(!is_usable_work_dir(Path::new("tmp/cloudkeep")));
        assert!(!is_usable_work_dir(Path::new("./cloudkeep")));
        assert!(!is_usable_work_dir(Path::new("cloudkeep")));
    }

    #[test]
    fn targets_use_basename() {
        let t = build_targets("/backup", &["/data/nginx".into(), "/data/mysql".into()]);
        assert_eq!(t[0].name, "nginx");
        assert_eq!(t[0].remote_dir, "/backup/nginx");
        assert_eq!(t[1].remote_dir, "/backup/mysql");
    }

    #[test]
    fn duplicate_names_are_disambiguated_by_parent() {
        let t = build_targets("/backup", &["/opt/a/data".into(), "/opt/b/data".into()]);
        assert_eq!(t[0].name, "data");
        assert_eq!(t[1].name, "b_data", "同名目录应使用上级目录名消歧");
        assert_eq!(t[1].remote_dir, "/backup/b_data");
    }

    #[test]
    fn triple_collision_gets_index_suffix() {
        let t = build_targets(
            "/backup",
            &["/x/1/data".into(), "/x/2/data".into(), "/x/3/data".into()],
        );
        assert_eq!(t[0].name, "data");
        assert_eq!(t[1].name, "2_data");
        assert_eq!(t[2].name, "3_data");
    }

    #[test]
    fn validate_rejects_relative_work_dir() {
        let mut cfg = Config {
            sources: vec!["/data".into()],
            remote_dir: "/backup".into(),
            excludes: vec![],
            cron: "0 3 * * *".into(),
            timezone: "Asia/Shanghai".into(),
            run_on_start: true,
            prefix: "backup".into(),
            work_dir: PathBuf::from("tmp/cloudkeep"),
            retention: 7,
            concurrency: 3,
            upload_attempts: 2,
            dry_run: false,
            app_key: String::new(),
            secret_key: String::new(),
            healthcheck_max_age_hours: 26.0,
        };
        assert!(cfg.validate().is_err());

        // POSIX 写法在 Windows 上同样接受（is_usable_work_dir 会放行）
        cfg.work_dir = PathBuf::from("/tmp/cloudkeep");
        assert!(cfg.validate().is_ok(), "绝对路径应通过校验");

        cfg.work_dir = default_work_dir();
        assert!(cfg.validate().is_ok(), "默认工作目录应通过校验");

        cfg.sources.clear();
        assert!(cfg.validate().is_err());
    }
}
