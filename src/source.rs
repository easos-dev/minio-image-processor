use crate::{
    config::{Config, Source},
    error::{Failure, Result},
};
use axum::http::StatusCode;
use percent_encoding::percent_decode_str;
use url::Url;

/// URL 仅决定允许桶内的对象和签名参数；实际网络目的地只取可信配置。
/// 保留公开 Host、路径和原查询，避免 S3 SigV4 被内网回源地址改变。
pub fn target(value: &str, sources: &[Source]) -> Result<(Url, String)> {
    if value.len() > 12_288 {
        return Err(Failure::invalid("source_url_too_long"));
    }
    let url = Url::parse(value).map_err(|_| Failure::invalid("invalid_source_url"))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(Failure::invalid("invalid_source_url"));
    }
    let source = sources
        .iter()
        .find(|s| s.origin == url.origin().ascii_serialization())
        .ok_or(Failure(StatusCode::FORBIDDEN, "source_not_allowed"))?;
    let decoded = percent_decode_str(url.path())
        .decode_utf8()
        .map_err(|_| Failure::invalid("invalid_source_path"))?;
    if !url.path().starts_with(&source.path_prefix)
        || !decoded.starts_with(&source.path_prefix)
        || decoded.contains(['\\', '\0', '%'])
        || decoded.split('/').any(|s| s == "." || s == "..")
    {
        return Err(Failure(StatusCode::FORBIDDEN, "source_path_not_allowed"));
    }
    let mut upstream =
        Url::parse(&source.upstream).map_err(|_| Failure::upstream("invalid_configuration"))?;
    upstream.set_path(url.path());
    upstream.set_query(url.query());
    Ok((
        upstream,
        url[url::Position::BeforeHost..url::Position::AfterPort].to_string(),
    ))
}

pub enum Fetched {
    Modified {
        bytes: Vec<u8>,
        etag: Option<String>,
    },
    Unchanged,
}
/// 即使命中处理缓存也让 MinIO 验证当前签名；只在当前授权有效且对象未变时接受 304。
pub async fn fetch(
    client: &reqwest::Client,
    config: &Config,
    source: &str,
    etag: Option<&str>,
) -> Result<Fetched> {
    let (url, host) = target(source, &config.sources)?;
    let mut request = client.get(url).header(reqwest::header::HOST, host);
    if let Some(etag) = etag {
        request = request.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    let mut response = request.send().await.map_err(|e| {
        if e.is_timeout() {
            Failure(StatusCode::GATEWAY_TIMEOUT, "source_timeout")
        } else {
            Failure::upstream("source_unreachable")
        }
    })?;
    if response.status() == StatusCode::NOT_MODIFIED && etag.is_some() {
        return Ok(Fetched::Unchanged);
    }
    if response.status() != StatusCode::OK {
        return Err(Failure::upstream("source_rejected"));
    }
    if response
        .content_length()
        .is_some_and(|n| n > config.max_input_bytes as u64)
    {
        return Err(Failure(StatusCode::PAYLOAD_TOO_LARGE, "source_too_large"));
    }
    // 弱 ETag 不保证字节一致，不能用于变换结果复用。
    let etag = response
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with('"') && v.ends_with('"') && v.len() <= 512)
        .map(str::to_owned);
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| {
        if e.is_timeout() {
            Failure(StatusCode::GATEWAY_TIMEOUT, "source_timeout")
        } else {
            Failure::upstream("source_read_failed")
        }
    })? {
        if chunk.len() > config.max_input_bytes.saturating_sub(bytes.len()) {
            return Err(Failure(StatusCode::PAYLOAD_TOO_LARGE, "source_too_large"));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Err(Failure::upstream("source_empty"));
    }
    Ok(Fetched::Modified { bytes, etag })
}
