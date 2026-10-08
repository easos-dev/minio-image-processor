//! 实际执行成功、失败和取消路径，核对终态关联以及签名/正文不进入日志。
use axum::{Router, body::Body, http::Request, routing::get};
use image::{DynamicImage, ImageFormat};
use image_processor::{
    config::{Config, Source},
    web,
};
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
fn request(path: &str) -> Request<Body> {
    let url =
        format!("https://objects.example.test/bucket/{path}?X-Amz-Signature=DO_NOT_LOG_SIGNATURE");
    let query = serde_urlencoded::to_string([("url", url)]).unwrap();
    Request::builder()
        .uri(format!("/?{query}&f=png"))
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
                async move { pixels }
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
        allowed_origins: vec!["http://127.0.0.1:18080".into()],
        sources: vec![Source {
            origin: "https://objects.example.test".into(),
            upstream: format!("http://{address}"),
            path_prefix: "/bucket/".into(),
        }],
        max_input_bytes: 10000,
        max_input_pixels: 10000,
        max_concurrent: 1,
        fetch_timeout_seconds: 5,
    })
    .unwrap();
    assert_eq!(
        app.clone().oneshot(request("ok")).await.unwrap().status(),
        200
    );
    assert_eq!(
        app.clone()
            .oneshot(request("missing"))
            .await
            .unwrap()
            .status(),
        502
    );
    let cancellation = tokio::spawn(app.oneshot(request("cancel")));
    entered.notified().await;
    cancellation.abort();
    assert!(cancellation.await.unwrap_err().is_cancelled());
    server_task.abort();
    let log = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
    for secret in ["DO_NOT_LOG", "X-Amz", "objects.example.test", "/bucket/"] {
        assert!(!log.contains(secret));
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
    assert_eq!(starts.len(), 3);
    assert_eq!(ends.len(), 3);
    for start in starts {
        assert_eq!(
            ends.iter()
                .filter(|e| e["fields"]["request_id"] == start["fields"]["request_id"])
                .count(),
            1
        );
    }
    for status in ["ok", "failed", "cancelled"] {
        assert!(ends.iter().any(|e| e["fields"]["status"] == status));
    }
}
