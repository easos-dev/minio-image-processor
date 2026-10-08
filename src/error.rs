use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

/// 错误只带固定原因码；上游 URL 内含访问签名，不能进入响应、日志或 Debug 字段。
#[derive(Clone, Copy, Debug)]
pub struct Failure(pub StatusCode, pub &'static str);
impl Failure {
    pub fn invalid(code: &'static str) -> Self {
        Self(StatusCode::BAD_REQUEST, code)
    }
    pub fn upstream(code: &'static str) -> Self {
        Self(StatusCode::BAD_GATEWAY, code)
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": {"code": self.1}}))).into_response()
    }
}
pub type Result<T> = std::result::Result<T, Failure>;
