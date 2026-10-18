//! 透传规则：查询串与请求/响应头哪些原样转、哪些必须去掉。
//!
//! 网关是 MinIO 的公开入口：路径、Host、查询串（含 S3 签名参数）、Range、条件头等端到端头逐字转给
//! 可信上游，只去掉 HTTP 规定代理不得转发的逐跳头。网关不签名、不验签、不持凭据；授权完全由上游判断。

use axum::http::{HeaderMap, HeaderName, HeaderValue, header};
use percent_encoding::percent_decode_str;

/// OSS 图片处理参数名。大小写敏感：OSS 实测 `X-Oss-Process` 不生效，网关同样只认小写。
pub const PROCESS: &str = "x-oss-process";

/// S3 查询串鉴权参数（SigV4 预签名与 SigV2 预签名）。不进缓存键，签名不同仍复用同一结果。
const AUTH_PARAMS: &[&str] = &[
    "X-Amz-Algorithm",
    "X-Amz-Credential",
    "X-Amz-Date",
    "X-Amz-Expires",
    "X-Amz-SignedHeaders",
    "X-Amz-Signature",
    "X-Amz-Security-Token",
    "AWSAccessKeyId",
    "Signature",
    "Expires",
];

/// 解码查询片段（只做百分号解码；`+` 保持原样，与 S3 规范化查询一致）。
fn decode(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

/// 查询串的各个片段与其解码后的参数名、值。
fn pieces(query: &str) -> impl Iterator<Item = (&str, String, String)> {
    query.split('&').filter(|p| !p.is_empty()).map(|piece| {
        let (name, value) = piece.split_once('=').unwrap_or((piece, ""));
        (piece, decode(name), decode(value))
    })
}

/// 第一个 `x-oss-process` 的值（OSS 实测重复给出时取第一个）。没有该参数返回 None。
pub fn process_value(query: &str) -> Option<String> {
    pieces(query)
        .find(|(_, name, _)| name == PROCESS)
        .map(|(_, _, value)| value)
}

/// 去掉所有 `x-oss-process` 片段，其余片段逐字节保留原样与顺序。
pub fn strip_process(query: &str) -> String {
    pieces(query)
        .filter(|(_, name, _)| name != PROCESS)
        .map(|(piece, _, _)| piece)
        .collect::<Vec<_>>()
        .join("&")
}

/// 请求是否带 S3 鉴权：查询串签名（SigV4 `X-Amz-Signature` / SigV2 `Signature`）或 `Authorization` 头。
///
/// 带鉴权时 `x-oss-process` 必须原样留在查询串里：SigV4 的规范化查询包含全部参数，签名方（EasOS File
/// 的私有读取地址）把处理串一并签了，网关删掉它就会让 MinIO 验签失败。这与 OSS 一致——OSS 也要求
/// 私有读取把 x-oss-process 签进 URL，签名后再追加处理串会被拒。MinIO 对 GetObject 不认识的查询参数
/// 只参与验签、不影响取对象，所以上游照样返回原图。
pub fn is_signed(query: &str, headers: &HeaderMap) -> bool {
    headers.contains_key(header::AUTHORIZATION)
        || pieces(query).any(|(_, name, _)| name == "X-Amz-Signature" || name == "Signature")
}

/// 缓存键用的非鉴权查询：解码后排序，去掉签名参数与 `x-oss-process`（处理串另行规范化进键）。
/// versionId、response-* 等改变结果的参数保留。
pub fn cache_query(query: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = pieces(query)
        .filter(|(_, name, _)| name != PROCESS && !AUTH_PARAMS.contains(&name.as_str()))
        .map(|(_, name, value)| (name, value))
        .collect();
    pairs.sort();
    pairs
}

/// 逐跳头（RFC 9110 §7.6.1）：只对相邻一跳有效，代理不得转发。另加上 `Connection` 头里点名的头。
fn hop_by_hop(headers: &HeaderMap) -> Vec<HeaderName> {
    let mut names: Vec<HeaderName> = [
        header::CONNECTION,
        HeaderName::from_static("keep-alive"),
        header::PROXY_AUTHENTICATE,
        header::PROXY_AUTHORIZATION,
        header::TE,
        header::TRAILER,
        header::TRANSFER_ENCODING,
        header::UPGRADE,
        HeaderName::from_static("proxy-connection"),
    ]
    .into();
    for value in headers.get_all(header::CONNECTION) {
        if let Ok(text) = value.to_str() {
            names.extend(
                text.split(',')
                    .filter_map(|t| HeaderName::from_bytes(t.trim().as_bytes()).ok()),
            );
        }
    }
    names
}

/// 复制请求头给上游。`Expect` 由本端 HTTP 服务自己应答 100-continue，不再向上游提；SDK 不把它签进
/// SigV4（botocore 明确排除），去掉不影响验签。处理路径另去掉 Range/If-Range（加工要读整个原图，
/// OSS 对处理请求同样忽略 Range）和请求正文相关的头。
pub fn request_headers(from: &HeaderMap, processing: bool) -> HeaderMap {
    let mut skip = hop_by_hop(from);
    skip.push(header::EXPECT);
    if processing {
        skip.extend([
            header::RANGE,
            header::IF_RANGE,
            header::CONTENT_LENGTH,
            header::CONTENT_TYPE,
        ]);
    }
    copy(from, &skip)
}

/// 复制上游响应头给客户端。处理结果另去掉描述原图正文的头（长度、类型、范围、校验和）。
pub fn response_headers(from: &HeaderMap, processed: bool) -> HeaderMap {
    let mut skip = hop_by_hop(from);
    if processed {
        skip.extend([
            header::CONTENT_LENGTH,
            header::CONTENT_TYPE,
            header::CONTENT_RANGE,
            header::CONTENT_ENCODING,
            header::ACCEPT_RANGES,
            HeaderName::from_static("content-md5"),
            HeaderName::from_static("x-amz-mp-parts-count"),
        ]);
    }
    let mut out = copy(from, &skip);
    if processed {
        // 上游给的是原图的校验和，对加工结果不成立。
        let checksums: Vec<HeaderName> = out
            .keys()
            .filter(|k| k.as_str().starts_with("x-amz-checksum-"))
            .cloned()
            .collect();
        for name in checksums {
            out.remove(name);
        }
    }
    out
}

/// 上游 200 应答里描述对象本身的头。上游的 304 不一定带全这些头，缓存命中时用它们补齐。
/// ETag 也在其中：OSS 的处理结果沿用原对象 ETag（OSS FAQ 原文），客户端带它做条件请求时，
/// 网关原样交给上游判断对象是否变化。
pub fn object_meta(from: &HeaderMap) -> Vec<(HeaderName, HeaderValue)> {
    from.iter()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "cache-control"
                    | "content-disposition"
                    | "content-language"
                    | "expires"
                    | "etag"
                    | "last-modified"
                    | "x-amz-version-id"
            ) || name.as_str().starts_with("x-amz-meta-")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// 按名单跳过后逐项复制（同名多值保持顺序）。
fn copy(from: &HeaderMap, skip: &[HeaderName]) -> HeaderMap {
    let mut out = HeaderMap::with_capacity(from.len());
    for (name, value) in from {
        if !skip.contains(name) {
            out.append(name.clone(), value.clone());
        }
    }
    out
}

/// 弱 ETag（`W/` 前缀）不保证字节一致，不能用于复用处理结果；只接受带引号的强 ETag。
pub fn strong_etag(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with('"') && v.ends_with('"') && v.len() >= 2 && v.len() <= 512)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED: &str = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AK%2F20260101%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20260101T000000Z&X-Amz-Expires=300&X-Amz-SignedHeaders=host&x-oss-process=image%2Fresize%2Cw_10&X-Amz-Signature=abc";

    #[test]
    fn process_parameter_is_found_decoded_and_case_sensitive() {
        assert_eq!(process_value(SIGNED).as_deref(), Some("image/resize,w_10"));
        assert_eq!(
            process_value("x-oss-process=image/resize,w_1&x-oss-process=image/format,png")
                .as_deref(),
            Some("image/resize,w_1")
        );
        assert_eq!(process_value("X-Oss-Process=image/resize,w_1"), None);
        assert_eq!(process_value("x-oss-process").as_deref(), Some(""));
        assert_eq!(process_value("versionId=1"), None);
        // 名字也可能被百分号编码。
        assert_eq!(
            process_value("x%2Doss%2Dprocess=image").as_deref(),
            Some("image")
        );
    }

    #[test]
    fn stripping_keeps_every_other_piece_byte_for_byte() {
        assert_eq!(
            strip_process(
                "a=1%2F2&x-oss-process=image%2Fresize%2Cw_1&b=%E4%B8%AD&x-oss-process=x&c"
            ),
            "a=1%2F2&b=%E4%B8%AD&c"
        );
        assert_eq!(strip_process("x-oss-process=image/info"), "");
        assert_eq!(strip_process(""), "");
    }

    #[test]
    fn signatures_are_detected_and_excluded_from_cache_keys() {
        let none = HeaderMap::new();
        assert!(is_signed(SIGNED, &none));
        assert!(is_signed("AWSAccessKeyId=a&Expires=1&Signature=s", &none));
        assert!(!is_signed("x-oss-process=image/info", &none));
        let mut auth = HeaderMap::new();
        auth.insert(
            header::AUTHORIZATION,
            "AWS4-HMAC-SHA256 Credential=x".parse().unwrap(),
        );
        assert!(is_signed("", &auth));
        assert!(cache_query(SIGNED).is_empty());
        assert_eq!(
            cache_query(
                "versionId=7&x-oss-process=image/info&response-content-type=a%2Fb&X-Amz-Signature=s"
            ),
            vec![
                ("response-content-type".to_owned(), "a/b".to_owned()),
                ("versionId".to_owned(), "7".to_owned())
            ]
        );
    }

    #[test]
    fn hop_by_hop_headers_never_cross_the_gateway() {
        let mut headers = HeaderMap::new();
        for (k, v) in [
            ("host", "bucket.example.test"),
            ("authorization", "AWS4-HMAC-SHA256 x"),
            ("x-amz-date", "20260101T000000Z"),
            ("range", "bytes=0-9"),
            ("if-none-match", "\"e\""),
            ("connection", "keep-alive, x-private"),
            ("x-private", "1"),
            ("keep-alive", "timeout=5"),
            ("transfer-encoding", "chunked"),
            ("te", "trailers"),
            ("expect", "100-continue"),
            ("content-length", "10"),
        ] {
            headers.insert(HeaderName::from_static(k), v.parse().unwrap());
        }
        let passed = request_headers(&headers, false);
        for kept in [
            "host",
            "authorization",
            "x-amz-date",
            "range",
            "if-none-match",
            "content-length",
        ] {
            assert!(passed.contains_key(kept), "{kept}");
        }
        for dropped in [
            "connection",
            "x-private",
            "keep-alive",
            "transfer-encoding",
            "te",
            "expect",
        ] {
            assert!(!passed.contains_key(dropped), "{dropped}");
        }
        let processing = request_headers(&headers, true);
        assert!(!processing.contains_key("range"));
        assert!(!processing.contains_key("content-length"));
        assert!(processing.contains_key("if-none-match"));
    }

    #[test]
    fn processed_responses_drop_original_body_headers_but_keep_object_metadata() {
        let mut headers = HeaderMap::new();
        for (k, v) in [
            ("etag", "\"abc\""),
            ("cache-control", "public, max-age=31536000, immutable"),
            ("content-disposition", "attachment"),
            ("content-length", "100"),
            ("content-type", "image/jpeg"),
            ("accept-ranges", "bytes"),
            ("content-md5", "x"),
            ("x-amz-checksum-crc64nvme", "y"),
            ("x-amz-meta-width", "4"),
            ("x-amz-request-id", "r"),
            ("access-control-allow-origin", "https://gui.example.test"),
        ] {
            headers.insert(HeaderName::from_static(k), v.parse().unwrap());
        }
        let processed = response_headers(&headers, true);
        for kept in [
            "etag",
            "cache-control",
            "content-disposition",
            "x-amz-meta-width",
            "x-amz-request-id",
            "access-control-allow-origin",
        ] {
            assert!(processed.contains_key(kept), "{kept}");
        }
        for dropped in [
            "content-length",
            "content-type",
            "accept-ranges",
            "content-md5",
            "x-amz-checksum-crc64nvme",
        ] {
            assert!(!processed.contains_key(dropped), "{dropped}");
        }
        assert_eq!(response_headers(&headers, false).len(), headers.len());
        let meta: Vec<_> = object_meta(&headers)
            .into_iter()
            .map(|(k, _)| k.to_string())
            .collect();
        assert_eq!(
            meta,
            [
                "etag",
                "cache-control",
                "content-disposition",
                "x-amz-meta-width"
            ]
        );
    }

    #[test]
    fn only_strong_etags_allow_reuse() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ETAG, "\"abc\"".parse().unwrap());
        assert_eq!(strong_etag(&headers).as_deref(), Some("\"abc\""));
        headers.insert(header::ETAG, "W/\"abc\"".parse().unwrap());
        assert_eq!(strong_etag(&headers), None);
    }
}
