//! 打包为 tar.gz（纯 Rust 实现，不依赖系统 tar）。
//!
//! 包内保留目录本身（解压即得 `<目录名>/...`），并按 `BACKUP_EXCLUDE` 排除路径。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use walkdir::WalkDir;

/// 按路径片段匹配排除规则，支持 `*` 通配。
#[derive(Debug, Clone)]
pub struct ExcludeFilter {
    patterns: Vec<String>,
}

impl ExcludeFilter {
    pub fn new(patterns: &[String]) -> Self {
        ExcludeFilter { patterns: patterns.to_vec() }
    }

    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// 判断某个条目路径是否应被排除（相对包根目录的路径）。
    pub fn is_excluded(&self, entry_path: &str) -> bool {
        if self.patterns.is_empty() {
            return false;
        }

        let segments: Vec<&str> = entry_path.split('/').filter(|s| !s.is_empty()).collect();

        self.patterns.iter().any(|pattern| {
            if !pattern.contains('*') {
                return segments.contains(&pattern.as_str());
            }
            match glob_to_regex(pattern) {
                Some(regex) => segments.iter().any(|seg| regex.is_match(seg)),
                None => segments.contains(&pattern.as_str()),
            }
        })
    }
}

/// 把 `*` 通配转成正则（锚定整段，`.` 等字符按字面量处理）。
fn glob_to_regex(pattern: &str) -> Option<regex::Regex> {
    let mut out = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => out.push_str(".*"),
            c if is_regex_special(c) => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push('$');
    regex::Regex::new(&out).ok()
}

fn is_regex_special(c: char) -> bool {
    matches!(c, '.' | '+' | '?' | '^' | '$' | '{' | '}' | '(' | ')' | '[' | ']' | '|' | '\\')
}

/// 把 `source` 目录打成 `archive_path`（tar.gz）。
///
/// 归档根目录为 `source` 的父目录，因此包内路径形如 `<basename>/...`。
pub fn create_archive(archive_path: &Path, source: &Path, filter: &ExcludeFilter) -> Result<()> {
    let cwd = source.parent().unwrap_or(Path::new("/"));
    let name = source
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string());

    if let Some(parent) = archive_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建归档目录失败: {}", parent.display()))?;
    }

    let file = File::create(archive_path)
        .with_context(|| format!("创建归档文件失败: {}", archive_path.display()))?;
    let gz = GzEncoder::new(BufWriter::new(file), Compression::default());
    let mut builder = tar::Builder::new(gz);
    builder.follow_symlinks(false);

    let result = append_dir(&mut builder, cwd, &name, filter);

    // 无论成功失败都要收尾，确保 gzip 流被正确 flush 关闭
    let finish = builder.into_inner().and_then(|gz| gz.finish());

    match (result, finish) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(err), _) => Err(err),
        (_, Err(err)) => Err(err).context("写入归档文件失败"),
    }
}

fn append_dir(
    builder: &mut tar::Builder<GzEncoder<BufWriter<File>>>,
    cwd: &Path,
    name: &str,
    filter: &ExcludeFilter,
) -> Result<()> {
    let root = cwd.join(name);

    // filter_entry 会在命中排除规则时直接剪枝，不再深入该子树
    // （大目录如 cache 因此不必逐个文件遍历）
    let walker = WalkDir::new(&root).follow_links(false).into_iter().filter_entry(|entry| {
        let Ok(rel) = entry.path().strip_prefix(cwd) else {
            return true;
        };
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        !filter.is_excluded(&rel_str)
    });

    for entry in walker {
        let entry = entry.with_context(|| format!("遍历目录失败: {}", root.display()))?;

        let path = entry.path();
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        // tar 内部一律用 `/` 分隔
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if entry.file_type().is_dir() {
            builder
                .append_dir(rel, path)
                .with_context(|| format!("添加目录失败: {rel_str}"))?;
        } else {
            builder
                .append_path_with_name(path, &rel_str)
                .with_context(|| format!("添加文件失败: {rel_str}"))?;
        }
    }

    Ok(())
}

/// 人类可读的字节数。
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;

    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.2} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 确保写完后文件句柄被刷盘。
pub fn flush(file: &mut File) -> Result<()> {
    file.flush().context("刷新文件缓冲区失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filter_excludes_nothing() {
        let f = ExcludeFilter::new(&[]);
        assert!(f.is_empty());
        assert!(!f.is_excluded("cache/a.js"));
        assert!(!f.is_excluded("a/b/c.txt"));
    }

    #[test]
    fn exact_segment_matching() {
        let f = ExcludeFilter::new(&["cache".into(), ".git".into()]);
        assert!(f.is_excluded("cache"));
        assert!(f.is_excluded("src/cache/pkg/index.js"));
        assert!(f.is_excluded(".git"));
        assert!(f.is_excluded("proj/.git/config"));
        assert!(!f.is_excluded("cache_old"), "应为整段匹配而非前缀匹配");
        assert!(!f.is_excluded("my_git_file"));
    }

    #[test]
    fn wildcard_pattern_matches_whole_segment() {
        let f = ExcludeFilter::new(&["*.log".into()]);
        assert!(f.is_excluded("app.log"));
        assert!(f.is_excluded("logs/app.log"));
        assert!(f.is_excluded("a/b/c.log"));
        assert!(!f.is_excluded("app.txt"));
        assert!(!f.is_excluded("log"), "'*' 需要匹配至少零个字符，但整段须完全匹配模式");
    }

    #[test]
    fn wildcard_does_not_cross_segment_boundary() {
        let f = ExcludeFilter::new(&["*.log".into()]);
        assert!(!f.is_excluded("app.log.gz"));
    }

    #[test]
    fn regex_special_chars_are_escaped() {
        let f = ExcludeFilter::new(&["a+b".into(), "v1.0".into()]);
        assert!(f.is_excluded("a+b"), "'+' 应按字面量匹配");
        assert!(f.is_excluded("v1.0"), "'.' 应按字面量匹配");
        assert!(!f.is_excluded("v1x0"), "'.' 不应匹配任意字符");
    }

    #[test]
    fn format_size_thresholds() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.00 KB");
        assert_eq!(format_size(1024 * 1024), "1.00 MB");
        assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GB");
        assert_eq!(format_size(1536), "1.50 KB");
    }

    #[test]
    fn archive_contains_directory_itself() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("data");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("a.txt"), "hello").unwrap();
        std::fs::write(src.join("nested/b.txt"), "world").unwrap();

        let archive = dir.path().join("out.tar.gz");
        create_archive(&archive, &src, &ExcludeFilter::new(&[])).unwrap();

        // 解包验证：路径应保留 data/ 这一层
        let decoder = flate2::read::GzDecoder::new(File::open(&archive).unwrap());
        let mut tar = tar::Archive::new(decoder);
        let names: Vec<String> = tar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().replace('\\', "/"))
            .collect();

        assert!(names.contains(&"data/a.txt".to_string()), "实际: {names:?}");
        assert!(names.contains(&"data/nested/b.txt".to_string()), "实际: {names:?}");
    }

    #[test]
    fn archive_applies_exclude_filter() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("data");
        std::fs::create_dir_all(src.join("cache/pkg")).unwrap();
        std::fs::create_dir_all(src.join("keep")).unwrap();
        std::fs::write(src.join("keep/file.txt"), "ok").unwrap();
        std::fs::write(src.join("cache/pkg/blob.bin"), "skip").unwrap();
        std::fs::write(src.join("app.log"), "skip").unwrap();

        let archive = dir.path().join("out.tar.gz");
        let filter = ExcludeFilter::new(&["cache".into(), "*.log".into()]);
        create_archive(&archive, &src, &filter).unwrap();

        let decoder = flate2::read::GzDecoder::new(File::open(&archive).unwrap());
        let mut tar = tar::Archive::new(decoder);
        let names: Vec<String> = tar
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().replace('\\', "/"))
            .collect();

        assert!(names.contains(&"data/keep/file.txt".to_string()), "实际: {names:?}");
        assert!(!names.iter().any(|n| n.contains("cache")), "实际: {names:?}");
        assert!(!names.iter().any(|n| n.ends_with("app.log")), "实际: {names:?}");
    }

    #[test]
    fn archive_of_empty_dir_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("empty");
        std::fs::create_dir_all(&src).unwrap();

        let archive = dir.path().join("empty.tar.gz");
        create_archive(&archive, &src, &ExcludeFilter::new(&[])).unwrap();

        assert!(archive.metadata().unwrap().len() > 0, "应产出有效的 gzip 归档");
    }

    #[test]
    fn archive_is_valid_gzip() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("data");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("f.bin"), vec![1u8; 100_000]).unwrap();

        let archive = dir.path().join("out.tar.gz");
        create_archive(&archive, &src, &ExcludeFilter::new(&[])).unwrap();

        // magic bytes: 1f 8b
        let bytes = std::fs::read(&archive).unwrap();
        assert_eq!(&bytes[..2], &[0x1f, 0x8b], "应为 gzip 格式");
    }

    /// 跨平台回归：无论在哪个系统上打包，归档内路径必须是 `/` 分隔，
    /// 且条目带可读权限位。
    ///
    /// Windows 上如果直接写入本地分隔符，Linux 解压会得到名为
    /// `data\sub` 的文件；权限位为 0 则会解出无法读取的文件。
    #[test]
    fn archive_is_portable_across_platforms() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("data");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("top.txt"), "a").unwrap();
        std::fs::write(src.join("sub/nested.txt"), "b").unwrap();

        let archive = dir.path().join("out.tar.gz");
        create_archive(&archive, &src, &ExcludeFilter::new(&[])).unwrap();

        let decoder = flate2::read::GzDecoder::new(File::open(&archive).unwrap());
        let mut tar = tar::Archive::new(decoder);

        let mut seen = Vec::new();
        for entry in tar.entries().unwrap() {
            let entry = entry.unwrap();
            let raw = String::from_utf8(entry.path_bytes().to_vec()).unwrap();
            seen.push(raw.clone());

            assert!(
                !raw.contains('\\'),
                "归档内路径不能含反斜杠（否则跨平台解压会出错）: {raw}"
            );
            assert!(!raw.starts_with('/'), "归档内应为相对路径: {raw}");

            let mode = entry.header().mode().expect("条目应带 mode");
            assert!(
                mode & 0o400 != 0,
                "条目至少应对属主可读: {raw} mode={mode:o}"
            );
        }

        assert!(seen.iter().any(|n| n == "data/top.txt"), "实际: {seen:?}");
        assert!(seen.iter().any(|n| n == "data/sub/nested.txt"), "实际: {seen:?}");
    }
}
