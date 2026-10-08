use axum::{
    Router,
    body::{Body, Bytes},
    http::{Request, StatusCode},
    routing::get,
};
use http_body_util::BodyExt;
use image::{DynamicImage, ImageFormat};
use image_processor::{
    config::{Config, Source},
    source,
    transform::{self, Parameters},
    web,
};
use std::{
    io::Cursor,
    sync::{Arc, Mutex},
};
use tower::ServiceExt;

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([(x * 7) as u8, (y * 11) as u8, 80, 255])
    });
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}
fn parameters(extra: &str) -> Parameters {
    serde_urlencoded::from_str(&format!(
        "url=https%3A%2F%2Fobjects.example.test%2Fbucket%2Fimage&{extra}"
    ))
    .unwrap()
}
fn config(upstream: &str) -> Config {
    Config {
        listen: "127.0.0.1:0".parse().unwrap(),
        allowed_origins: vec!["http://127.0.0.1:18080".into()],
        sources: vec![Source {
            origin: "https://objects.example.test".into(),
            upstream: upstream.into(),
            path_prefix: "/bucket/".into(),
        }],
        max_input_bytes: 100 * 1024 * 1024,
        max_input_pixels: 24_000_000,
        max_concurrent: 2,
        fetch_timeout_seconds: 1,
    }
}
struct Server {
    origin: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn serve(router: Router) -> Server {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    Server { origin, task }
}
async fn request(app: Router, source: &str, extra: &str) -> axum::response::Response {
    let query = serde_urlencoded::to_string([("url", source)]).unwrap();
    app.oneshot(
        Request::builder()
            .uri(format!("/?{query}&{extra}"))
            .header("Origin", "http://127.0.0.1:18080")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
}
async fn body(response: axum::response::Response) -> Bytes {
    response.into_body().collect().await.unwrap().to_bytes()
}

#[test]
fn shape_format_quality_alpha_and_defaults() {
    let input = png(120, 60);
    for (fit, size) in [
        ("contain", (40, 20)),
        ("fill", (40, 40)),
        ("cover", (40, 40)),
    ] {
        for (format, mime) in [
            ("jpg", "image/jpeg"),
            ("jpeg", "image/jpeg"),
            ("png", "image/png"),
            ("webp", "image/webp"),
        ] {
            let result = transform::transform(
                &input,
                &parameters(&format!("width=40&height=40&fit={fit}&format={format}")),
                100_000,
            )
            .unwrap();
            assert_eq!((result.width, result.height), size);
            assert_eq!(result.content_type, mime);
            assert_eq!(
                image::load_from_memory(&result.bytes).unwrap().width(),
                size.0
            );
        }
    }
    let default = transform::transform(&input, &parameters(""), 100_000).unwrap();
    assert_eq!((default.width, default.height), (120, 60));
    assert_eq!(default.content_type, "image/png");
    for f in ["jpg", "webp"] {
        let a = transform::transform(
            &input,
            &parameters(&format!("format={f}&quality=20")),
            100_000,
        )
        .unwrap();
        let b = transform::transform(
            &input,
            &parameters(&format!("format={f}&quality=95")),
            100_000,
        )
        .unwrap();
        assert_ne!(a.bytes, b.bytes);
    }
    let mut transparent = Cursor::new(Vec::new());
    DynamicImage::new_rgba8(2, 2)
        .write_to(&mut transparent, ImageFormat::Png)
        .unwrap();
    let result =
        transform::transform(transparent.get_ref(), &parameters("format=jpg"), 100).unwrap();
    let decoded = image::load_from_memory(&result.bytes).unwrap().to_rgb8();
    assert!(decoded.get_pixel(0, 0).0.iter().all(|x| *x > 245));
    let result =
        transform::transform(transparent.get_ref(), &parameters("format=png"), 100).unwrap();
    assert_eq!(
        image::load_from_memory(&result.bytes)
            .unwrap()
            .to_rgba8()
            .get_pixel(0, 0)[3],
        0
    );
}
#[test]
fn exif_orientation_is_applied_before_resize() {
    let mut jpeg = Cursor::new(Vec::new());
    DynamicImage::new_rgb8(3, 2)
        .write_to(&mut jpeg, ImageFormat::Jpeg)
        .unwrap();
    let jpeg = jpeg.into_inner();
    // APP1 内嵌最小 little-endian TIFF，Orientation=6（顺时针90度）。
    let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
    let mut source = jpeg[..2].to_vec();
    source.extend([0xff, 0xe1]);
    source.extend(((exif.len() + 2) as u16).to_be_bytes());
    source.extend(exif);
    source.extend(&jpeg[2..]);
    let result = transform::transform(&source, &parameters("width=3&height=3"), 100).unwrap();
    assert_eq!((result.width, result.height), (2, 3));
    assert!(!result.bytes.windows(6).any(|w| w == b"Exif\0\0"));
}
#[test]
fn invalid_images_parameters_and_pixel_bombs_fail() {
    for query in ["width=0", "height=1921", "quality=0", "quality=101"] {
        assert!(parameters(query).validate().is_err());
    }
    assert!(serde_urlencoded::from_str::<Parameters>("url=x&unknown=y").is_err());
    assert!(serde_urlencoded::from_str::<Parameters>("url=x&width=10&width=20").is_err());
    assert!(transform::transform(b"<svg onload='alert(1)'/>", &parameters(""), 100).is_err());
    assert_eq!(
        transform::transform(&png(20, 20), &parameters(""), 399)
            .err()
            .unwrap()
            .1,
        "source_pixel_limit"
    );
    let mut broken = png(20, 20);
    broken.truncate(20);
    assert!(transform::transform(&broken, &parameters(""), 1000).is_err());
}
#[test]
fn fixed_source_host_preserves_signature_and_rejects_ssrf_paths() {
    let sources = config("http://minio:9000").sources;
    let (target, host) = source::target(
        "https://objects.example.test/bucket/a%20b?X-Amz-Signature=secret&x=1%2F2",
        &sources,
    )
    .unwrap();
    assert_eq!(
        target.as_str(),
        "http://minio:9000/bucket/a%20b?X-Amz-Signature=secret&x=1%2F2"
    );
    assert_eq!(host, "objects.example.test");
    for value in [
        "http://169.254.169.254/latest/meta-data/",
        "http://127.0.0.1/admin",
        "https://objects.example.test.evil.test/bucket/a",
        "https://user@objects.example.test/bucket/a",
        "https://objects.example.test/bucket/../admin",
        "https://objects.example.test/bucket/%2f..%2fadmin",
        "https://objects.example.test/other/a",
        "https://objects.example.test/bucket/a#fragment",
        "https://objects.example.test/bucket/%252f..%252fadmin",
    ] {
        assert!(source::target(value, &sources).is_err(), "{value}");
    }
}
#[test]
fn configuration_is_explicit_and_rejects_credentials() {
    let mut c = config("http://minio:9000");
    assert!(c.validate().is_ok());
    c.sources[0].upstream = "http://admin:secret@minio:9000".into();
    assert!(c.validate().is_err());
    c = config("http://minio:9000");
    c.sources[0].path_prefix = "/bucket/../".into();
    assert!(c.validate().is_err());
    c = config("http://minio:9000");
    c.max_concurrent = 20;
    assert!(c.validate().is_err());
    c = config("http://minio:9000");
    c.allowed_origins = vec!["*".into()];
    assert!(c.validate().is_err());
}
#[tokio::test]
async fn http_pipeline_preserves_signed_host_cors_and_pixels_without_forwarding_credentials() {
    let captured = Arc::new(Mutex::new(None));
    let record = captured.clone();
    let input = png(80, 40);
    let server = serve(Router::new().fallback(get(move |r: Request<Body>| {
        let record = record.clone();
        let input = input.clone();
        async move {
            *record.lock().unwrap() = Some((r.uri().to_string(), r.headers().clone()));
            input
        }
    })))
    .await;
    let app = web::app(config(&server.origin)).unwrap();
    let query = serde_urlencoded::to_string([(
        "url",
        "https://objects.example.test/bucket/image?X-Amz-Signature=fixture",
    )])
    .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/?{query}&width=20&height=20&format=webp"))
                .header("Origin", "http://127.0.0.1:18080")
                .header("Authorization", "Bearer must-not-forward")
                .header("Cookie", "must-not-forward")
                .header("Referer", "must-not-forward")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "http://127.0.0.1:18080"
    );
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["content-type"], "image/webp");
    let image = image::load_from_memory(&body(response).await).unwrap();
    assert_eq!((image.width(), image.height()), (20, 10));
    let capture = captured.lock().unwrap();
    let (uri, headers) = capture.as_ref().unwrap();
    assert_eq!(uri, "/bucket/image?X-Amz-Signature=fixture");
    assert_eq!(headers["host"], "objects.example.test");
    for name in ["authorization", "cookie", "referer", "x-trace-id"] {
        assert!(!headers.contains_key(name));
    }
}
#[tokio::test]
async fn redirects_upstream_denial_and_size_limit_are_not_retried() {
    for status in [
        StatusCode::FOUND,
        StatusCode::FORBIDDEN,
        StatusCode::NOT_FOUND,
        StatusCode::INTERNAL_SERVER_ERROR,
    ] {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        let server = serve(Router::new().fallback(get(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                (
                    status,
                    [("Location", "http://169.254.169.254/")],
                    "private-upstream-error",
                )
            }
        })))
        .await;
        let response = request(
            web::app(config(&server.origin)).unwrap(),
            "https://objects.example.test/bucket/a?secret=must-not-leak",
            "",
        )
        .await;
        assert_eq!(response.status(), 502);
        let body = body(response).await;
        assert!(!String::from_utf8_lossy(&body).contains("private"));
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    let server = serve(Router::new().fallback(get(|| async { png(80, 40) }))).await;
    let mut c = config(&server.origin);
    c.max_input_bytes = 10;
    assert_eq!(
        request(
            web::app(c).unwrap(),
            "https://objects.example.test/bucket/a",
            ""
        )
        .await
        .status(),
        413
    );
}
#[tokio::test]
async fn timeout_busy_and_cancel_release_async_download_permit() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let seen = entered.clone();
    let server = serve(Router::new().fallback(get(move || {
        let seen = seen.clone();
        async move {
            seen.notify_one();
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            png(2, 2)
        }
    })))
    .await;
    let mut c = config(&server.origin);
    c.max_concurrent = 1;
    let app = web::app(c).unwrap();
    let first = tokio::spawn(request(
        app.clone(),
        "https://objects.example.test/bucket/a",
        "",
    ));
    entered.notified().await;
    assert_eq!(
        request(app.clone(), "https://objects.example.test/bucket/b", "")
            .await
            .status(),
        503
    );
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert_eq!(
        request(app, "https://objects.example.test/bucket/c", "")
            .await
            .status(),
        504
    );
}

#[test]
fn cover_crops_extreme_aspect_ratio_before_resampling() {
    for (w, h) in [(1, 32768), (32768, 1)] {
        let result = transform::transform(
            &png(w, h),
            &parameters("width=64&height=64&fit=cover&format=png"),
            40000,
        )
        .unwrap();
        assert_eq!((result.width, result.height), (64, 64));
    }
}

#[test]
fn animations_and_oversized_resampling_buffers_are_rejected() {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_animated(2, 0).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[255; 16]).unwrap();
        writer.write_image_data(&[0; 16]).unwrap();
    }
    assert_eq!(
        transform::transform(&bytes, &parameters(""), 1000)
            .err()
            .unwrap()
            .1,
        "animated_image_not_supported"
    );
    assert_eq!(
        transform::transform(
            &png(100, 1),
            &parameters("width=20&height=20&fit=fill"),
            1000
        )
        .err()
        .unwrap()
        .1,
        "intermediate_pixel_limit"
    );
}

#[tokio::test]
async fn cors_and_input_errors_have_stable_responses() {
    let app = web::app(config("http://127.0.0.1:1")).unwrap();
    for (origin, allowed) in [
        ("http://127.0.0.1:18080", true),
        ("https://unlisted.example.test", false),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .method("OPTIONS")
                    .header("Origin", origin)
                    .header("Access-Control-Request-Method", "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response
                .headers()
                .contains_key("access-control-allow-origin"),
            allowed
        );
    }
    let response = request(app, "https://objects.example.test/bucket/a", "unexpected=x").await;
    assert_eq!(response.status(), 400);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let id = response.headers()["x-request-id"].to_str().unwrap();
    assert!(uuid::Uuid::parse_str(id).is_ok());
    let json: serde_json::Value = serde_json::from_slice(&body(response).await).unwrap();
    assert_eq!(json["error"]["code"], "invalid_parameters");
}
