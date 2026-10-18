//! 实际执行成功、上游拒绝、参数错误和取消路径，核对终态关联，以及路径、查询串、签名、Host 与
//! 正文都不进入日志。加工部分需要目标 Linux 的 image-worker。
use axum::{Router, body::Body, http::Request, routing::get};
use image::{DynamicImage, ImageFormat};
use image_processor::{config::Config, web};
use std::{
    io::{Cursor, Write},
    sync::{Arc, Mutex},
};
use tower::ServiceExt;
use tracing_subscriber::prelude::*;

#[derive(Clone)]
struct Buffer(Arc<Mutex<Vec<u8>>>);
impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
    type Writer = Self;
    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}
/// 带签名、Host 与 Authorization 的请求；这些值都不允许出现在日志里。
fn request(path: &str, process: &str) -> Request<Body> {
    Request::builder()
        .uri(format!(
            "/bucket/{path}?x-oss-process={process}&X-Amz-Signature=DO_NOT_LOG_SIGNATURE"
        ))
        .header("host", "objects.example.test")
        .header("authorization", "Bearer DO_NOT_LOG_BEARER")
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn success_failure_and_cancellation_have_correlated_safe_terminal_logs() {
    let buffer = Buffer(Arc::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_target(false)
            .with_writer(buffer.clone())
            .with_filter(
                tracing_subscriber::filter::Targets::new()
                    .with_target("image_processor", tracing::Level::INFO),
            ),
    );
    tracing::subscriber::set_global_default(subscriber).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let seen = entered.clone();
    let mut png = Cursor::new(Vec::new());
    DynamicImage::new_rgb8(2, 2)
        .write_to(&mut png, ImageFormat::Png)
        .unwrap();
    let pixels = png.into_inner();
    let server = Router::new()
        .route(
            "/bucket/ok",
            get(move || {
                let pixels = pixels.clone();
                async move { ([("etag", "\"e\"")], pixels) }
            }),
        )
        .route(
            "/bucket/cancel",
            get(move || {
                let seen = seen.clone();
                async move {
                    seen.notify_one();
                    std::future::pending::<()>().await;
                    ""
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server_task = tokio::spawn(async move {
        axum::serve(listener, server).await.unwrap();
    });
    let app = web::app(Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        upstream: format!("http://{address}"),
        max_concurrent: 1,
        fetch_timeout_seconds: 5,
    })
    .unwrap();
    let ok = app
        .clone()
        .oneshot(request("ok", "image/format,png"))
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    // 上游 404 原样转发；参数错误在回源前返回。
    assert_eq!(
        app.clone()
            .oneshot(request("missing", "image/format,png"))
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        app.clone()
            .oneshot(request("ok", "image/resize,w_abc"))
            .await
            .unwrap()
            .status(),
        400
    );
    let cancellation = tokio::spawn(app.clone().oneshot(request("cancel", "image/format,png")));
    entered.notified().await;
    // 名额被占用时取消等待者，须有 queue 阶段终态且不触发额外回源。
    let queued = tokio::spawn(app.oneshot(request("ok", "image/format,png")));
    tokio::task::yield_now().await;
    queued.abort();
    assert!(queued.await.unwrap_err().is_cancelled());
    cancellation.abort();
    assert!(cancellation.await.unwrap_err().is_cancelled());
    server_task.abort();
    let log = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    for secret in [
        "DO_NOT_LOG",
        "X-Amz",
        "objects.example.test",
        "/bucket/",
        "x-oss-process",
        "w_abc",
    ] {
        assert!(!log.contains(secret), "{secret}");
    }
    let entries: Vec<serde_json::Value> = log
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    let starts: Vec<_> = entries
        .iter()
        .filter(|e| e["fields"]["event"] == "request_start")
        .collect();
    let ends: Vec<_> = entries
        .iter()
        .filter(|e| e["fields"]["event"] == "request_end")
        .collect();
    assert_eq!(starts.len(), 5);
    assert_eq!(ends.len(), 5);
    for start in starts {
        assert_eq!(
            ends.iter()
                .filter(|e| e["fields"]["request_id"] == start["fields"]["request_id"])
                .count(),
            1
        );
    }
    assert!(
        ends.iter()
            .any(|e| e["fields"]["status"] == "cancelled" && e["fields"]["stage"] == "queue")
    );
    assert!(ends.iter().any(|e| e["fields"]["status"] == "failed"
        && e["fields"]["code"] == "InvalidArgument"
        && e["fields"]["ec"] == "0040-00000312"));
    assert!(
        ends.iter()
            .any(|e| e["fields"]["status"] == "ok" && e["fields"]["http_status"] == 404)
    );
    for status in ["ok", "failed", "cancelled"] {
        assert!(ends.iter().any(|e| e["fields"]["status"] == status));
    }
}
