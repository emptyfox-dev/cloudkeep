//! 百度网盘开放平台接口封装。
//!
//! 覆盖本项目需要的操作：列目录、建目录、分块上传、删除。
//! 分块上传沿用开放平台的 `precreate` → `superfile2` → `create` 三段式流程，
//! 并支持秒传（`return_type=2`）。

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use reqwest::multipart::{Form, Part};
use serde::Deserialize;

use crate::token::{PAN_URL, USER_AGENT};

/// 分块大小 4 MB（开放平台建议值）
pub const CHUNK_SIZE: u64 = 4 * 1024 * 1024;

/// 分块上传地址（注意与 pan.baidu.com 不同域）
pub const SUPERFILE_URL: &str = "https://d.pcs.baidu.com/rest/2.0/pcs/superfile2";

/// 百度接口 errno
pub mod errno {
    pub const OK: i64 = 0;
    pub const NAME_INVALID: i64 = -7;
    pub const EXISTS: i64 = -8;
    pub const NOT_FOUND: i64 = -9;
}

/// 开放平台返回的错误体。
///
/// 不同接口的错误字段不一致（errno / error_code / error_msg / error），
/// 这里统一成一个结构体，便于判断 token 过期并生成可读信息。
#[derive(Debug, Default, Clone, Deserialize, PartialEq)]
pub struct ApiError {
    #[serde(default)]
    pub errno: Option<i64>,
    #[serde(default)]
    pub error_code: Option<i64>,
    #[serde(default)]
    pub error_msg: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl ApiError {
    /// errno 无 -6 / 111，或 error_code 110 / 111，表示 token 过期。
    pub fn is_token_expired(&self) -> bool {
        matches!(self.errno, Some(e) if crate::token::is_token_expired_errno(e))
            || matches!(self.error_code, Some(110) | Some(111))
    }

    /// 是否是真实错误（errno 非 0 / 存在错误描述）。
    pub fn is_error(&self) -> bool {
        self.errno.is_some_and(|e| e != errno::OK)
            || self.error_code.is_some_and(|c| c != 0)
            || self.error_msg.as_deref().is_some_and(|s| !s.is_empty())
    }

    /// 人类可读的描述，便于写进错误信息。
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(v) = self.errno {
            parts.push(format!("errno={v}"));
        }
        if let Some(v) = self.error_code {
            parts.push(format!("error_code={v}"));
        }
        if let Some(v) = self.error_msg.as_deref().filter(|s| !s.is_empty()) {
            parts.push(format!("error_msg={v}"));
        }
        if let Some(v) = self.error.as_deref().filter(|s| !s.is_empty()) {
            parts.push(format!("error={v}"));
        }
        if parts.is_empty() {
            "无错误详情".to_string()
        } else {
            parts.join(", ")
        }
    }
}

/// 网盘目录/文件条目。
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteEntry {
    #[serde(alias = "server_filename", default)]
    pub name: String,
    /// 百度接口用 0/1 整数表示，不是布尔
    #[serde(default, deserialize_with = "de_isdir")]
    pub isdir: Option<bool>,
    #[serde(default)]
    pub size: u64,
}

/// 兼容 `isdir: 0|1`、`true|false` 以及字段缺失三种情况。
fn de_isdir<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Flag {
        Num(i64),
        Bool(bool),
    }

    Ok(Option::<Flag>::deserialize(deserializer).ok().flatten().map(|v| match v {
        Flag::Num(n) => n != 0,
        Flag::Bool(b) => b,
    }))
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    list: Option<Vec<RemoteEntry>>,
    #[serde(flatten)]
    error: ApiError,
}

#[derive(Debug, Deserialize)]
struct PrecreateResponse {
    #[serde(default)]
    uploadid: Option<String>,
    #[serde(default)]
    block_list: Option<Vec<u64>>,
    /// 2 表示秒传命中，无需上传任何分块
    #[serde(default)]
    return_type: Option<i64>,
    #[serde(flatten)]
    error: ApiError,
}

#[derive(Debug, Deserialize)]
struct ChunkResponse {
    #[serde(default)]
    md5: Option<String>,
    #[serde(flatten)]
    error: ApiError,
}

/// 预创建结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PrecreateResult {
    pub upload_id: Option<String>,
    pub blocks_to_upload: Vec<u64>,
    pub quick_upload: bool,
    /// 调用方本地计算的全量分块 MD5，用作提交时的基准值
    pub md5_list: Vec<String>,
}

/// 百度网盘 API 客户端（内部持有可复用的 HTTP 连接池）。
#[derive(Debug, Clone)]
pub struct PanClient {
    http: reqwest::Client,
}

impl PanClient {
    pub fn new(http: reqwest::Client) -> Self {
        PanClient { http }
    }

    /// 借用底层 HTTP 客户端（token 刷新等需要复���同一连接池）。
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// 列出目录下的条目（单次上限 1000）。
    pub async fn list(&self, token: &str, dir: &str) -> Result<Vec<RemoteEntry>> {
        let text = self
            .http
            .get(format!("{PAN_URL}/rest/2.0/xpan/file"))
            .query(&[
                ("method", "list"),
                ("dir", dir),
                ("order", "name"),
                ("desc", "0"),
                ("start", "0"),
                ("limit", "1000"),
                ("web", "1"),
                ("access_token", token),
            ])
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .with_context(|| format!("调用百度网盘接口失败（list {dir}）"))?
            .text()
            .await
            .context("读取 list 响应失败")?;

        let parsed: ListResponse = parse_json(&text, "list")?;

        if parsed.error.is_error() {
            bail!("列举目录失败: {}", parsed.error.describe());
        }

        Ok(parsed.list.unwrap_or_default())
    }

    /// 创建目录（`isdir=1`）。目录已存在时接口返回 errno=-8。
    pub async fn create_dir(&self, token: &str, dir: &str) -> Result<ApiError> {
        let form = [
            ("path", dir.to_string()),
            ("size", "0".to_string()),
            ("isdir", "1".to_string()),
            ("rtype", "3".to_string()),
            ("block_list", "[]".to_string()),
        ];

        let text = self
            .http
            .post(format!("{PAN_URL}/rest/2.0/xpan/file"))
            .query(&[("method", "create"), ("access_token", token)])
            .header("User-Agent", USER_AGENT)
            .form(&form)
            .send()
            .await
            .with_context(|| format!("创建网盘目录失败（请求 {dir}）"))?
            .text()
            .await
            .context("读取创建目录响应失败")?;

        let err: ApiError = parse_json(&text, "创建目录")?;
        Ok(err)
    }

    /// 分块上传完成后提交文件（`create`）。
    pub async fn create_file(
        &self,
        token: &str,
        path: &str,
        size: u64,
        upload_id: &str,
        md5_list: &[String],
    ) -> Result<ApiError> {
        let block_list = serde_json::to_string(md5_list).unwrap_or_else(|_| "[]".into());
        let form = [
            ("path", path.to_string()),
            ("size", size.to_string()),
            ("isdir", "0".to_string()),
            ("uploadid", upload_id.to_string()),
            ("block_list", block_list),
            ("rtype", "3".to_string()),
        ];

        let text = self
            .http
            .post(format!("{PAN_URL}/rest/2.0/xpan/file"))
            .query(&[("method", "create"), ("access_token", token)])
            .header("User-Agent", USER_AGENT)
            .form(&form)
            .send()
            .await
            .with_context(|| format!("创建上传文件失败（{path}）"))?
            .text()
            .await
            .context("读取创建文件响应失败")?;

        let err: ApiError = parse_json(&text, "创建文件")?;
        Ok(err)
    }

    /// 预创建：返回 uploadid 与需要上传的分块序号列表。
    pub async fn precreate(
        &self,
        token: &str,
        path: &str,
        size: u64,
        md5_list: &[String],
    ) -> Result<PrecreateResult> {
        let block_list = serde_json::to_string(md5_list).unwrap_or_else(|_| "[]".into());
        let form = [
            ("path", path.to_string()),
            ("size", size.to_string()),
            ("isdir", "0".to_string()),
            ("autoinit", "1".to_string()),
            ("block_list", block_list),
            ("rtype", "3".to_string()),
        ];

        let text = self
            .http
            .post(format!("{PAN_URL}/rest/2.0/xpan/file"))
            .query(&[("method", "precreate"), ("access_token", token)])
            .header("User-Agent", USER_AGENT)
            .form(&form)
            .send()
            .await
            .with_context(|| format!("预创建失败（{path}）"))?
            .text()
            .await
            .context("读取预创建响应失败")?;

        let parsed: PrecreateResponse = parse_json(&text, "预创建")?;

        if parsed.error.is_error() {
            bail!("预创建失败: {}", parsed.error.describe());
        }

        Ok(PrecreateResult {
            upload_id: parsed.uploadid,
            blocks_to_upload: parsed.block_list.unwrap_or_default(),
            quick_upload: parsed.return_type == Some(2),
            md5_list: md5_list.to_vec(),
        })
    }

    /// 上传单个分块，返回服务端回传的该分块 MD5。
    pub async fn upload_chunk(
        &self,
        token: &str,
        path: &str,
        upload_id: &str,
        partseq: u64,
        data: Vec<u8>,
    ) -> Result<String> {
        let part = Part::bytes(data)
            .file_name("chunk")
            .mime_str("application/octet-stream")?;
        let form = Form::new().part("file", part);

        let text = self
            .http
            .post(SUPERFILE_URL)
            .query(&[
                ("method", "upload"),
                ("access_token", token),
                ("type", "tmpfile"),
                ("path", path),
                ("uploadid", upload_id),
                ("partseq", &partseq.to_string()),
            ])
            .header("User-Agent", USER_AGENT)
            .multipart(form)
            .send()
            .await
            .with_context(|| format!("上传分块 {partseq} 失败"))?
            .text()
            .await
            .context("读取分块响应失败")?;

        let parsed: ChunkResponse = parse_json(&text, "上传分块")?;

        if parsed.error.is_error() {
            bail!("上传分块 {partseq} 失败: {}", parsed.error.describe());
        }

        parsed
            .md5
            .filter(|m| !m.is_empty())
            .ok_or_else(|| anyhow::anyhow!("上传分块 {partseq} 响应缺少 md5"))
    }

    /// 批量删除文件（`filemanager` + `opera=remove`）。
    pub async fn delete_files(&self, token: &str, paths: &[String]) -> Result<ApiError> {
        if paths.is_empty() {
            return Ok(ApiError::default());
        }

        let filelist: Vec<HashMap<&str, &str>> =
            paths.iter().map(|p| HashMap::from([("path", p.as_str())])).collect();
        let body = serde_json::to_string(&filelist).unwrap_or_else(|_| "[]".into());

        let text = self
            .http
            .post(format!("{PAN_URL}/rest/2.0/xpan/file"))
            .query(&[
                ("method", "filemanager"),
                ("opera", "remove"),
                ("access_token", token),
            ])
            .header("User-Agent", USER_AGENT)
            .form(&[("async", "0"), ("filelist", &body)])
            .send()
            .await
            .context("调用删除接口失败")?
            .text()
            .await
            .context("读取删除响应失败")?;

        let err: ApiError = parse_json(&text, "删除")?;
        Ok(err)
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T> {
    let trimmed = text.trim();
    // 开放平台偶尔返回 HTML 错误页，这里截断后放进报错信息便于排查
    let preview = trimmed.chars().take(200).collect::<String>();
    serde_json::from_str(trimmed).with_context(|| format!("{what}响应无法解析: {preview}"))
}

/// 计算分块的 MD5。
pub fn md5_hex(data: &[u8]) -> String {
    let digest = md5::compute(data);
    hex::encode(digest.as_ref())
}

/// 计算分块总数（向上取整；空文件为 0）。
pub fn total_chunks(file_size: u64) -> u64 {
    file_size.div_ceil(CHUNK_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_count_is_ceil_division() {
        assert_eq!(total_chunks(0), 0);
        assert_eq!(total_chunks(1), 1);
        assert_eq!(total_chunks(CHUNK_SIZE), 1);
        assert_eq!(total_chunks(CHUNK_SIZE + 1), 2);
        assert_eq!(total_chunks(CHUNK_SIZE * 2), 2);
        assert_eq!(total_chunks(CHUNK_SIZE * 3 + 5), 4);
    }

    #[test]
    fn md5_matches_known_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn api_error_describe_includes_all_fields() {
        let err = ApiError {
            errno: Some(-7),
            error_code: Some(31066),
            error_msg: Some("目录名不合法".into()),
            error: None,
        };
        let d = err.describe();
        assert!(d.contains("errno=-7"), "{d}");
        assert!(d.contains("error_code=31066"), "{d}");
        assert!(d.contains("error_msg=目录名不合法"), "{d}");
    }

    #[test]
    fn api_error_describe_empty() {
        assert_eq!(ApiError::default().describe(), "无错误详情");
    }

    #[test]
    fn api_error_detects_token_expiry() {
        assert!(ApiError { errno: Some(-6), ..Default::default() }.is_token_expired());
        assert!(ApiError { errno: Some(111), ..Default::default() }.is_token_expired());
        assert!(ApiError { error_code: Some(110), ..Default::default() }.is_token_expired());
        assert!(ApiError { error_code: Some(111), ..Default::default() }.is_token_expired());
        assert!(!ApiError { errno: Some(0), ..Default::default() }.is_token_expired());
        assert!(!ApiError::default().is_token_expired());
    }

    #[test]
    fn api_error_is_error_detection() {
        assert!(ApiError { errno: Some(-8), ..Default::default() }.is_error());
        assert!(ApiError { error_code: Some(31066), ..Default::default() }.is_error());
        assert!(ApiError { error_msg: Some("x".into()), ..Default::default() }.is_error());
        // errno=0 是成功，不算错误
        assert!(!ApiError { errno: Some(0), ..Default::default() }.is_error());
        assert!(!ApiError::default().is_error());
    }

    #[test]
    fn list_response_parses_entries() {
        let json = r#"{"list":[{"server_filename":"backup_1.tar.gz","isdir":0,"size":123},{"server_filename":"sub","isdir":1,"size":0}],"errno":0}"#;
        let parsed: ListResponse = serde_json::from_str(json).unwrap();
        assert!(!parsed.error.is_error());
        let list = parsed.list.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "backup_1.tar.gz");
        assert_eq!(list[0].size, 123);
        assert_eq!(list[0].isdir, Some(false));
        assert_eq!(list[1].name, "sub");
        assert_eq!(list[1].isdir, Some(true));
    }

    #[test]
    fn list_response_surfaces_errors() {
        let json = r#"{"errno":-6,"error_msg":"expired"}"#;
        let parsed: ListResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.error.is_error());
        assert!(parsed.error.is_token_expired());
    }

    #[test]
    fn precreate_parses_blocks_and_quick_upload() {
        let json = r#"{"uploadid":"UP123","block_list":[0,2],"return_type":0,"errno":0}"#;
        let parsed: PrecreateResponse = serde_json::from_str(json).unwrap();
        assert!(!parsed.error.is_error());
        assert_eq!(parsed.uploadid.as_deref(), Some("UP123"));
        assert_eq!(parsed.block_list.unwrap(), vec![0, 2]);
        assert_ne!(parsed.return_type, Some(2));
    }

    #[test]
    fn precreate_detects_instant_upload() {
        let json = r#"{"return_type":2,"errno":0}"#;
        let parsed: PrecreateResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.return_type, Some(2), "return_type=2 表示秒传");
    }

    #[test]
    fn chunk_response_parses_md5() {
        let json = r#"{"md5":"900150983cd24fb0d6963f7d28e17f72","errno":0}"#;
        let parsed: ChunkResponse = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.md5.as_deref(), Some("900150983cd24fb0d6963f7d28e17f72"));
        assert!(!parsed.error.is_error());
    }

    #[test]
    fn html_error_page_is_reported() {
        let result: Result<ChunkResponse> = parse_json("<html>502</html>", "上传分块");
        assert!(result.is_err());
        assert!(
            format!("{:#}", result.unwrap_err()).contains("html"),
            "应把响应内容带入错误信息"
        );
    }
}
