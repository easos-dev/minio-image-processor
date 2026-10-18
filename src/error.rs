//! 网关自己产生的错误应答。
//!
//! 处理串、源图与编码错误照搬 OSS：HTTP 状态、`Code`、`Message` 文本与 `EC` 细分码以 OSS 官方文档和
//! 公开示例桶的实测应答为准，正文是同结构的 XML。上游（MinIO）自己的错误应答不经过这里，原样透传。
//! 网关特有的失败（上游不可达、排队满、编码进程异常）没有 OSS 对应项，用 S3/OSS 客户端都认识的
//! `InternalError` / `ServiceUnavailable` 表达，不给 EC。

use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::Response,
};

/// 一条 OSS 风格错误。
///
/// `message` 可能含请求里的原始参数值，只写进响应正文（XML 转义），不进日志；
/// 日志只记 `code`、`ec` 与 `reason` 这些固定文本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OssError {
    /// HTTP 状态码。
    pub status: StatusCode,
    /// OSS 通用错误码（XML 的 `Code` 元素），如 `InvalidArgument`。
    pub code: &'static str,
    /// 详细说明（`Message` 元素）；参数错误的写法照抄 OSS 实测文本，含拼写。
    pub message: String,
    /// OSS 细分错误码（`EC` 元素与 `x-oss-ec` 头）；OSS 自己也不给 EC 的情形为 `None`。
    pub ec: Option<&'static str>,
    /// 只给日志用的固定原因，区分同一 `code` 下的不同网关失败。
    pub reason: &'static str,
}

impl OssError {
    /// 通用构造。
    pub fn new(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        ec: Option<&'static str>,
        reason: &'static str,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            ec,
            reason,
        }
    }

    /// OSS 最常见的参数错误：`The value: <值> of parameter: <参数> is invalid.`
    pub fn invalid_value(value: &str, parameter: &str, ec: &'static str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            format!("The value: {value} of parameter: {parameter} is invalid."),
            Some(ec),
            "invalid_argument",
        )
    }

    /// 带原因的参数错误。OSS 原文在句号后直接接 `reason:`，中间没有空格，这里照抄。
    pub fn invalid_reason(value: &str, parameter: &str, reason: &str, ec: &'static str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "InvalidArgument",
            format!("The value: {value} of parameter: {parameter} is invalid.reason:{reason}"),
            Some(ec),
            "invalid_argument",
        )
    }

    /// 其他固定文本的 400 错误（`MissingArgument`、`BadRequest`、`CutEdgeExceedRange` 等）。
    pub fn bad(code: &'static str, message: &'static str, ec: Option<&'static str>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, code, message, ec, "bad_request")
    }

    /// 源图超过 OSS 的 20 MB 处理上限（OSS EC 0040-00000001）。
    pub fn source_too_large() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "ImageTooLarge",
            "Maximal size of image supported is 20MB.",
            Some("0040-00000001"),
            "source_too_large",
        )
    }

    /// 对象不是网关能解码的图片（OSS EC 0040-00000005，Message 为 OSS FAQ 里的原文）。
    pub fn unsupported_format() -> Self {
        Self::bad(
            "BadRequest",
            "This image format is not supported.",
            Some("0040-00000005"),
        )
    }

    /// 连不上可信上游，或读上游正文中途断开。
    pub fn upstream_unreachable() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "InternalError",
            "The upstream storage is unreachable.",
            None,
            "upstream_unreachable",
        )
    }

    /// 处理路径回源超过 `fetch_timeout_seconds`。
    pub fn upstream_timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "InternalError",
            "The upstream storage timed out.",
            None,
            "upstream_timeout",
        )
    }

    /// 处理请求（含排队、回源、编码）整体超过 55 秒。
    pub fn request_timeout() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "InternalError",
            "Image processing timed out.",
            None,
            "request_timeout",
        )
    }

    /// 排队名额已满，立即拒绝。
    pub fn busy() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "ServiceUnavailable",
            "Too many image processing requests are waiting.",
            None,
            "busy",
        )
    }

    /// 在队列里等满 45 秒仍没拿到编码名额。
    pub fn queue_timeout() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "ServiceUnavailable",
            "Timed out waiting for an image processing slot.",
            None,
            "queue_timeout",
        )
    }

    /// 编码子进程无法启动、异常退出或输出不合约定。
    pub fn worker_failed() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "InternalError",
            "Image processing failed.",
            None,
            "worker_failed",
        )
    }

    /// 组装成 OSS 同结构的 XML 应答。`host` 是请求的 Host，写进 `HostId`，与 OSS 一致。
    pub fn response(&self, request_id: &str, host: &str) -> Response {
        let mut body = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error>\n  <Code>{}</Code>\n  <Message>{}</Message>\n  <RequestId>{}</RequestId>\n  <HostId>{}</HostId>\n",
            self.code,
            xml_escape(&self.message),
            xml_escape(request_id),
            xml_escape(host)
        );
        if let Some(ec) = self.ec {
            body.push_str(&format!("  <EC>{ec}</EC>\n"));
        }
        body.push_str("</Error>\n");
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = self.status;
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml"),
        );
        if let Some(ec) = self.ec {
            headers.insert("x-oss-ec", HeaderValue::from_static(ec));
        }
        response
    }
}

/// XML 元素文本转义；参数值与 Host 来自请求，可能含 `<`、`&` 等字符。
/// 只转义元素文本里必须转义的字符，引号保持原样，与 OSS 原文（如 `cut's`）逐字一致。
fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // XML 1.0 不允许大部分控制字符；直接丢弃，避免客户端解析失败。
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

pub type Result<T> = std::result::Result<T, OssError>;

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn xml_matches_oss_layout_and_escapes_request_values() {
        let error = OssError::invalid_value("<a&b>", "w", "0040-00000312");
        let response = error.response("req-1", "bucket.example.test");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["content-type"], "application/xml");
        assert_eq!(response.headers()["x-oss-ec"], "0040-00000312");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Error>\n  <Code>InvalidArgument</Code>\n  <Message>The value: &lt;a&amp;b&gt; of parameter: w is invalid.</Message>\n  <RequestId>req-1</RequestId>\n  <HostId>bucket.example.test</HostId>\n  <EC>0040-00000312</EC>\n</Error>\n"
        );
    }

    #[tokio::test]
    async fn errors_without_ec_omit_the_element_and_header() {
        let response = OssError::bad(
            "CutEdgeExceedRange",
            "Advance cut's position is out of image.",
            None,
        )
        .response("r", "h");
        assert!(!response.headers().contains_key("x-oss-ec"));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("<Message>Advance cut's position is out of image.</Message>"));
        assert!(!text.contains("<EC>"));
    }

    #[test]
    fn reasons_follow_oss_concatenation() {
        let error = OssError::invalid_reason("", "rotate", "missing agnle", "0040-00000216");
        assert_eq!(
            error.message,
            "The value:  of parameter: rotate is invalid.reason:missing agnle"
        );
    }
}
