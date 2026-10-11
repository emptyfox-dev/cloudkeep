use std::ffi::OsString;
use std::path::Path;

use clap::Parser;

use cloudkeep::cli::Cli;

/// 可作为 `argv[0]` 直接调用的子命令名（busybox 风格）。
///
/// `docker exec` 不经过镜像 ENTRYPOINT，因此镜像内为每个子命令建了符号链接；
/// 以子命令名调用二进制等价于 `cloudkeep <子命令>`，这样
/// `docker exec cloudkeep backup` 才能正常工作。
const APPLETS: [&str; 8] = [
    "run",
    "backup",
    "ensure-dir",
    "auth",
    "auth-url",
    "auth-code",
    "healthcheck",
    "help",
];

/// 若以子命令名（或指向它的符号链接）被调用，则补上对应的子命令参数。
///
/// 只在「没有显式给出子命令」时推断：即紧随其后的参数不是裸子命令名。
fn inject_applet_subcommand(args: &mut Vec<OsString>) {
    let Some(first) = args.first() else {
        return;
    };

    let stem = Path::new(first)
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase());
    let Some(stem) = stem else {
        return;
    };

    // 只有以子命令名调用时才生效（正常入口 `cloudkeep` 不在名单里）
    if !APPLETS.contains(&stem.as_str()) {
        return;
    }

    // 后面已经跟了子命令时不再注入，避免出现 `backup backup`
    if let Some(second) = args.get(1).and_then(|a| a.to_str())
        && APPLETS.contains(&second)
    {
        return;
    }

    args.insert(1, OsString::from(stem));
}

fn main() {
    let mut args: Vec<OsString> = std::env::args_os().collect();
    inject_applet_subcommand(&mut args);

    let cli = Cli::parse_from(args);
    let code = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("创建 tokio 运行时失败")
        .block_on(cli.run());

    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inject(argv: &[&str]) -> Vec<String> {
        let mut args: Vec<OsString> = argv.iter().map(OsString::from).collect();
        inject_applet_subcommand(&mut args);
        args.iter().map(|a| a.to_string_lossy().to_string()).collect()
    }

    #[test]
    fn normal_entrypoint_is_untouched() {
        assert_eq!(inject(&["cloudkeep"]), vec!["cloudkeep"]);
        assert_eq!(inject(&["cloudkeep", "backup"]), vec!["cloudkeep", "backup"]);
        assert_eq!(
            inject(&["/usr/local/bin/cloudkeep", "run"]),
            vec!["/usr/local/bin/cloudkeep", "run"]
        );
    }

    #[test]
    fn symlinked_command_injects_subcommand() {
        assert_eq!(inject(&["backup"]), vec!["backup", "backup"]);
        assert_eq!(inject(&["/usr/local/bin/backup"]), vec!["/usr/local/bin/backup", "backup"]);
        assert_eq!(inject(&["ensure-dir"]), vec!["ensure-dir", "ensure-dir"]);
        assert_eq!(inject(&["healthcheck"]), vec!["healthcheck", "healthcheck"]);
    }

    #[test]
    fn symlinked_command_keeps_flags() {
        assert_eq!(
            inject(&["backup", "--dry-run"]),
            vec!["backup", "backup", "--dry-run"]
        );
    }

    #[test]
    fn does_not_double_inject() {
        // 已经显式给出子命令时保持原样
        assert_eq!(inject(&["backup", "backup"]), vec!["backup", "backup"]);
    }

    #[test]
    fn windows_exe_suffix_is_handled() {
        assert_eq!(inject(&["backup.exe"]), vec!["backup.exe", "backup"]);
        assert_eq!(inject(&["cloudkeep.exe"]), vec!["cloudkeep.exe"]);
    }

    #[test]
    fn unknown_binary_name_is_untouched() {
        assert_eq!(inject(&["my-backup-tool"]), vec!["my-backup-tool"]);
    }

    #[test]
    fn injected_args_parse_as_the_right_subcommand() {
        // 端到端：注入后 clap 应解析出 backup 子命令
        let args = inject(&["backup"]);
        let cli = Cli::try_parse_from(args).expect("应能解析");
        assert!(format!("{cli:?}").contains("Backup"));
    }
}
