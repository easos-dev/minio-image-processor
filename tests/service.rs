//! 网关的 HTTP 行为：用 loopback 上的 MinIO 桩核对透传（路径、查询串与签名逐字、Range、HEAD、304、
//! 上传流式转发）与加工路径（OSS 处理串、缓存命中、对象头、上游错误透传、OSS 错误码）。
//! 加工部分需要目标 Linux 的 image-worker（见 README）。
use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use image_processor::{config::Config, web};
use std::{
    collections::HashMap,
    io::Cursor,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

/// 桩里的一个对象。
#[derive(Clone)]
struct Object {
    /// 对象正文。
    body: Vec<u8>,
    /// 带引号的 ETag。
    etag: String,
    /// 额外的对象头（Content-Type、Cache-Control 等）。
    headers: Vec<(&'static str, String)>,
    /// 预签名请求必须逐字等于这条查询串（模拟 SigV4 覆盖整个查询串）。
    signed_query: Option<String>,
    /// 私有对象：匿名请求返回 403。
    private: bool,
}

/// 桩收到的一次请求。
#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    /// 原始请求目标（路径 + 查询串），逐字比较。
    uri: String,
    headers: HeaderMap,
    body: Vec<u8>,
}

/// 模拟 MinIO 的对象读写、签名核对、条件请求与 Range。
#[derive(Clone, Default)]
struct Stub {
    objects: Arc<Mutex<HashMap<String, Object>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
    /// 每个请求先等这么久（模拟慢上游）。
    delay: Arc<Mutex<Option<Duration>>>,
    /// 当前在处理的请求数与峰值，用于核对并发上限。
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Stub {
    fn put(&self, path: &str, object: Object) {
        self.objects.lock().unwrap().insert(path.into(), object);
    }
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    fn last(&self) -> Seen {
        self.seen().pop().expect("upstream request")
    }
}

/// S3 风格错误正文。
fn s3_error(status: StatusCode, code: &str) -> Response {
    (
        status,
        [("content-type", "application/xml"), ("x-amz-request-id", "STUB")],
        format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>{code}</Code><Message>stub</Message></Error>"),
    )
        .into_response()
}

async fn stub_handler(State(stub): State<Stub>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = body.collect().await.unwrap().to_bytes().to_vec();
    let uri = parts
        .uri
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();
    stub.seen.lock().unwrap().push(Seen {
        method: parts.method.clone(),
        uri,
        headers: parts.headers.clone(),
        body: body.clone(),
    });
    let now = stub.active.fetch_add(1, Ordering::SeqCst) + 1;
    stub.peak.fetch_max(now, Ordering::SeqCst);
    let delay = *stub.delay.lock().unwrap();
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    let response = respond(&stub, &parts, body);
    stub.active.fetch_sub(1, Ordering::SeqCst);
    response
}

fn respond(stub: &Stub, parts: &axum::http::request::Parts, body: Vec<u8>) -> Response {
    let path = parts.uri.path().to_owned();
    let query = parts.uri.query().unwrap_or("").to_owned();
    if parts.method == Method::PUT {
        let etag = format!("\"put-{}\"", body.len());
        stub.put(
            &path,
            Object {
                body,
                etag: etag.clone(),
                headers: vec![],
                signed_query: None,
                private: false,
            },
        );
        return ([("etag", etag)], "").into_response();
    }
    if !matches!(parts.method, Method::GET | Method::HEAD) {
        return (StatusCode::OK, "other").into_response();
    }
    let Some(object) = stub.objects.lock().unwrap().get(&path).cloned() else {
        return s3_error(StatusCode::NOT_FOUND, "NoSuchKey");
    };
    if query.contains("X-Amz-Signature=") {
        if object.signed_query.as_deref() != Some(query.as_str()) {
            return s3_error(StatusCode::FORBIDDEN, "SignatureDoesNotMatch");
        }
    } else if object.private && !parts.headers.contains_key("authorization") {
        return s3_error(StatusCode::FORBIDDEN, "AccessDenied");
    }
    let header = |name: &str| {
        parts
            .headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    if header("if-match").is_some_and(|m| m != object.etag) {
        return s3_error(StatusCode::PRECONDITION_FAILED, "PreconditionFailed");
    }
    let last_modified = "Wed, 16 Nov 2022 08:08:39 GMT";
    // 与 MinIO 一样，304 只带 ETag 与 Last-Modified，不带 Cache-Control、Content-Disposition 等对象头。
    if header("if-none-match").is_some_and(|m| m == object.etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                ("etag", object.etag.clone()),
                ("last-modified", last_modified.into()),
            ],
        )
            .into_response();
    }
    let mut response = Response::builder()
        .header("etag", &object.etag)
        .header("last-modified", last_modified)
        .header("accept-ranges", "bytes")
        .header("content-md5", "c3R1Yg==")
        .header("x-amz-request-id", "STUB");
    for (name, value) in &object.headers {
        response = response.header(*name, value);
    }
    if let Some(range) = header("range").and_then(|r| r.strip_prefix("bytes=").map(str::to_owned)) {
        let (start, end) = range.split_once('-').unwrap();
        let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
        return response
            .status(StatusCode::PARTIAL_CONTENT)
            .header(
                "content-range",
                format!("bytes {start}-{end}/{}", object.body.len()),
            )
            .body(Body::from(object.body[start..=end].to_vec()))
            .unwrap();
    }
    response.body(Body::from(object.body)).unwrap()
}

/// 测试结束时停掉的 loopback 服务。
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

/// 一套桩 + 网关。
struct Rig {
    stub: Stub,
    upstream: Server,
    gateway: Server,
    client: reqwest::Client,
}
async fn rig(max_concurrent: usize, fetch_timeout_seconds: u64) -> Rig {
    let stub = Stub::default();
    let upstream = serve(
        Router::new()
            .fallback(stub_handler)
            .with_state(stub.clone()),
    )
    .await;
    let gateway = serve(
        web::app(Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            upstream: upstream.origin.clone(),
            max_concurrent,
            fetch_timeout_seconds,
        })
        .unwrap(),
    )
    .await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    Rig {
        stub,
        upstream,
        gateway,
        client,
    }
}
impl Rig {
    fn get(&self, target: &str) -> reqwest::RequestBuilder {
        self.client
            .get(format!("{}{target}", self.gateway.origin))
            .header("host", "bucket.example.test")
    }
}

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = RgbaImage::from_fn(width, height, |x, y| {
        Rgba([(x * 7) as u8, (y * 11) as u8, 80, 255])
    });
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut bytes, ImageFormat::Png)
        .unwrap();
    bytes.into_inner()
}
fn image_object(bytes: Vec<u8>, etag: &str) -> Object {
    Object {
        body: bytes,
        etag: etag.into(),
        headers: vec![
            ("content-type", "image/png".into()),
            (
                "cache-control",
                "public, max-age=31536000, immutable".into(),
            ),
            (
                "content-disposition",
                "attachment; filename=\"download\"".into(),
            ),
            ("x-amz-meta-width", "800".into()),
        ],
        signed_query: None,
        private: false,
    }
}
fn dimensions(bytes: &[u8]) -> (u32, u32) {
    let image = image::load_from_memory(bytes).unwrap();
    (image.width(), image.height())
}
const LFIT_256: &str =
    "image/auto-orient,1/resize,m_lfit,w_256,h_256,limit_1/quality,q_90/format,webp";
const FILL_64: &str =
    "image/auto-orient,1/resize,m_fill,w_64,h_64,limit_1/quality,q_90/format,webp";
fn process(value: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("x-oss-process", value)
        .finish()
}

#[tokio::test]
async fn passthrough_keeps_path_query_signature_headers_and_bytes() {
    let r = rig(2, 2).await;
    let path = "/bucket/a%20b/%E4%B8%AD.png";
    let query = "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AK%2F20261018%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20261018T000000Z&X-Amz-Expires=300&X-Amz-SignedHeaders=host&response-content-disposition=attachment%3B%20filename%2A%3DUTF-8%27%27a.png&X-Amz-Signature=0123abcd";
    let mut object = image_object(png(30, 20), "\"e1\"");
    object.signed_query = Some(query.into());
    r.stub.put(path, object.clone());

    let target = format!("{path}?{query}");
    let response = r
        .get(&target)
        .header("connection", "x-hop")
        .header("x-hop", "1")
        .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let headers = response.headers().clone();
    for (name, value) in [
        ("etag", "\"e1\""),
        ("content-type", "image/png"),
        ("accept-ranges", "bytes"),
        ("content-md5", "c3R1Yg=="),
        ("cache-control", "public, max-age=31536000, immutable"),
        ("content-disposition", "attachment; filename=\"download\""),
        ("x-amz-meta-width", "800"),
        ("last-modified", "Wed, 16 Nov 2022 08:08:39 GMT"),
    ] {
        assert_eq!(headers[name], value, "{name}");
    }
    assert_eq!(headers["content-length"], object.body.len().to_string());
    assert!(!headers.contains_key("x-image-cache"));
    assert!(uuid::Uuid::parse_str(headers["x-request-id"].to_str().unwrap()).is_ok());
    let through_gateway = response.bytes().await.unwrap();
    // 与直接向上游取逐字节一致。
    let direct = r
        .client
        .get(format!("{}{target}", r.upstream.origin))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(through_gateway, direct);
    assert_eq!(through_gateway.as_ref(), object.body.as_slice());
    let seen = &r.stub.seen()[0];
    assert_eq!(seen.uri, target);
    assert_eq!(seen.headers["host"], "bucket.example.test");
    assert_eq!(seen.headers["x-amz-content-sha256"], "UNSIGNED-PAYLOAD");
    assert!(!seen.headers.contains_key("x-hop"));

    // Range → 206 与 Content-Range 原样回给客户端。
    let partial = r
        .get(&target)
        .header("range", "bytes=2-9")
        .send()
        .await
        .unwrap();
    assert_eq!(partial.status(), 206);
    assert_eq!(
        partial.headers()["content-range"],
        format!("bytes 2-9/{}", object.body.len())
    );
    assert_eq!(partial.bytes().await.unwrap().as_ref(), &object.body[2..=9]);
    assert_eq!(r.stub.last().headers["range"], "bytes=2-9");

    // HEAD：头与 GET 相同、没有正文。
    let head = r
        .client
        .head(format!("{}{target}", r.gateway.origin))
        .header("host", "bucket.example.test")
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(
        head.headers()["content-length"],
        object.body.len().to_string()
    );
    assert_eq!(head.headers()["etag"], "\"e1\"");
    assert!(head.bytes().await.unwrap().is_empty());

    // 条件请求：304 原样转回。
    let not_modified = r
        .get(&target)
        .header("if-none-match", "\"e1\"")
        .send()
        .await
        .unwrap();
    assert_eq!(not_modified.status(), 304);
    assert_eq!(not_modified.headers()["etag"], "\"e1\"");

    // 签名不对：上游的 403 与正文原样转回，网关不做任何判断。
    let tampered = target.replace("0123abcd", "ffffffff");
    let denied = r.get(&tampered).send().await.unwrap();
    assert_eq!(denied.status(), 403);
    assert!(
        String::from_utf8_lossy(&denied.bytes().await.unwrap()).contains("SignatureDoesNotMatch")
    );
}

#[tokio::test]
async fn uploads_and_other_methods_stream_through_unchanged() {
    let r = rig(2, 2).await;
    let payload: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let query = "X-Amz-Signature=abc&x-oss-process=image%2Fresize%2Cw_1";
    let response = r
        .client
        .put(format!("{}/bucket/upload?{query}", r.gateway.origin))
        .header("host", "bucket.example.test")
        .header("content-md5", "md5")
        .header("if-none-match", "*")
        .body(payload.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["etag"],
        format!("\"put-{}\"", payload.len())
    );
    let seen = r.stub.last();
    assert_eq!(seen.method, Method::PUT);
    // 非 GET/HEAD 的查询串一律不动。
    assert_eq!(seen.uri, format!("/bucket/upload?{query}"));
    assert_eq!(seen.body, payload);
    assert_eq!(seen.headers["content-length"], payload.len().to_string());
    assert!(!seen.headers.contains_key("transfer-encoding"));
    assert_eq!(seen.headers["if-none-match"], "*");
    assert_eq!(seen.headers["content-md5"], "md5");

    // 客户端分块上传（没有 Content-Length）：上游同样收到分块正文。用原始 HTTP/1.1 报文发送。
    let address = r.gateway.origin.trim_start_matches("http://").to_owned();
    let raw = tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream
            .write_all(b"PUT /bucket/chunked HTTP/1.1\r\nHost: bucket.example.test\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n4\r\ndefg\r\n0\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    })
    .await
    .unwrap();
    assert!(raw.starts_with("HTTP/1.1 200"), "{raw}");
    let seen = r.stub.last();
    assert_eq!(seen.body, b"abcdefg");
    assert_eq!(seen.headers["transfer-encoding"], "chunked");
    assert!(!seen.headers.contains_key("content-length"));

    // 其他方法（分片上传等）同样透传。
    let response = r
        .client
        .post(format!("{}/bucket/key?uploads", r.gateway.origin))
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), "other");
    assert_eq!(r.stub.last().uri, "/bucket/key?uploads");
}

#[tokio::test]
async fn anonymous_reads_drop_the_process_parameter_and_signed_reads_keep_it() {
    let r = rig(2, 2).await;
    r.stub
        .put("/bucket/photo", image_object(png(80, 40), "\"p1\""));

    // 匿名加工：转给上游前去掉 x-oss-process，其他参数逐字保留。
    let response = r
        .get(&format!(
            "/bucket/photo?versionId=3&{}",
            process("image/resize,w_10")
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(dimensions(&response.bytes().await.unwrap()), (10, 5));
    assert_eq!(r.stub.last().uri, "/bucket/photo?versionId=3");

    // HEAD 与 OSS 一致不加工，返回原对象的头；匿名时同样去掉处理串。
    let head = r
        .client
        .head(format!(
            "{}/bucket/photo?{}",
            r.gateway.origin,
            process("image/resize,w_10")
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status(), 200);
    assert_eq!(
        head.headers()["content-length"],
        png(80, 40).len().to_string()
    );
    assert_eq!(head.headers()["content-type"], "image/png");
    assert!(!head.headers().contains_key("x-image-cache"));
    assert_eq!(r.stub.last().uri, "/bucket/photo");

    // 单独的 image、未知前缀：原样返回原图（OSS 实测同样如此）。
    for value in ["image", "foo/bar"] {
        let original = r
            .get(&format!("/bucket/photo?{}", process(value)))
            .send()
            .await
            .unwrap();
        assert!(!original.headers().contains_key("x-image-cache"));
        assert_eq!(
            original.bytes().await.unwrap().as_ref(),
            png(80, 40).as_slice()
        );
        assert_eq!(r.stub.last().uri, "/bucket/photo");
    }

    // 预签名里签进了 x-oss-process：整条查询串原样交给上游验签，再加工。
    let signed = format!("{}&X-Amz-Signature=sig", process("image/resize,w_20"));
    let mut private = image_object(png(80, 40), "\"s1\"");
    private.private = true;
    private.signed_query = Some(signed.clone());
    r.stub.put("/bucket/private", private);
    let response = r
        .get(&format!("/bucket/private?{signed}"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(dimensions(&response.bytes().await.unwrap()), (20, 10));
    assert_eq!(r.stub.last().uri, format!("/bucket/private?{signed}"));

    // 签名后再追加处理串：与 OSS 一样验签失败，上游的 403 原样回给客户端。
    let mut unsigned_process = image_object(png(80, 40), "\"s2\"");
    unsigned_process.private = true;
    unsigned_process.signed_query = Some("X-Amz-Signature=sig".into());
    r.stub.put("/bucket/private2", unsigned_process);
    let denied = r
        .get(&format!(
            "/bucket/private2?X-Amz-Signature=sig&{}",
            process("image/resize,w_20")
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    assert!(
        String::from_utf8_lossy(&denied.bytes().await.unwrap()).contains("SignatureDoesNotMatch")
    );
}

#[tokio::test]
async fn easos_previews_are_processed_cached_and_keep_object_headers() {
    let r = rig(2, 2).await;
    r.stub
        .put("/bucket/easos/obj", image_object(png(800, 600), "\"v1\""));
    let lfit = format!("/bucket/easos/obj?{}", process(LFIT_256));

    let first = r.get(&lfit).send().await.unwrap();
    assert_eq!(first.status(), 200);
    let headers = first.headers().clone();
    assert_eq!(headers["content-type"], "image/webp");
    assert_eq!(headers["x-image-cache"], "MISS");
    // OSS：处理结果沿用原对象的 ETag 与对象头；描述原图正文的头不再出现。
    assert_eq!(headers["etag"], "\"v1\"");
    assert_eq!(
        headers["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"download\""
    );
    assert_eq!(headers["x-amz-meta-width"], "800");
    assert!(!headers.contains_key("accept-ranges"));
    assert!(!headers.contains_key("content-md5"));
    let bytes = first.bytes().await.unwrap();
    assert_eq!(headers["content-length"], bytes.len().to_string());
    assert_eq!(dimensions(&bytes), (256, 192));
    assert!(!r.stub.last().headers.contains_key("if-none-match"));

    // 第二次：网关带缓存的 ETag 做条件 GET，上游 304 即命中；304 缺的对象头由缓存补齐。
    let second = r.get(&lfit).send().await.unwrap();
    assert_eq!(second.headers()["x-image-cache"], "HIT");
    assert_eq!(
        second.headers()["cache-control"],
        "public, max-age=31536000, immutable"
    );
    assert_eq!(
        second.headers()["content-disposition"],
        "attachment; filename=\"download\""
    );
    assert_eq!(second.headers()["etag"], "\"v1\"");
    assert_eq!(second.bytes().await.unwrap(), bytes);
    assert_eq!(r.stub.last().headers["if-none-match"], "\"v1\"");

    // 写法不同、语义相同的处理串命中同一缓存。
    let reordered = format!(
        "/bucket/easos/obj?{}",
        process("image/format,webp/resize,h_256,w_256,m_lfit/quality,q_90")
    );
    assert_eq!(
        r.get(&reordered).send().await.unwrap().headers()["x-image-cache"],
        "HIT"
    );

    // 另一档位：短边缩到 64 后居中裁成 64×64。
    let fill = r
        .get(&format!("/bucket/easos/obj?{}", process(FILL_64)))
        .send()
        .await
        .unwrap();
    assert_eq!(fill.headers()["x-image-cache"], "MISS");
    assert_eq!(dimensions(&fill.bytes().await.unwrap()), (64, 64));

    // 处理请求忽略 Range（OSS 同样返回完整结果），上游也收不到 Range。
    let ranged = r
        .get(&lfit)
        .header("range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(ranged.status(), 200);
    assert_eq!(ranged.bytes().await.unwrap(), bytes);
    assert!(!r.stub.last().headers.contains_key("range"));

    // 对象更新后 ETag 变化：重新编码。
    r.stub
        .put("/bucket/easos/obj", image_object(png(400, 300), "\"v2\""));
    let changed = r.get(&lfit).send().await.unwrap();
    assert_eq!(changed.headers()["x-image-cache"], "MISS");
    assert_eq!(dimensions(&changed.bytes().await.unwrap()), (256, 192));

    // 弱 ETag 不能保证字节一致：不缓存。
    r.stub
        .put("/bucket/weak", image_object(png(40, 30), "W/\"w\""));
    let weak = r
        .get(&format!("/bucket/weak?{}", process(LFIT_256)))
        .send()
        .await
        .unwrap();
    assert_eq!(weak.headers()["x-image-cache"], "BYPASS");
}

#[tokio::test]
async fn client_conditionals_on_processed_urls_are_decided_by_upstream() {
    let r = rig(2, 2).await;
    r.stub.put("/bucket/c", image_object(png(80, 60), "\"c1\""));
    let target = format!("/bucket/c?{}", process(LFIT_256));
    assert_eq!(
        r.get(&target).send().await.unwrap().headers()["x-image-cache"],
        "MISS"
    );

    // 带原对象 ETag 的条件请求：上游 304 原样回给客户端。
    let not_modified = r
        .get(&target)
        .header("if-none-match", "\"c1\"")
        .send()
        .await
        .unwrap();
    assert_eq!(not_modified.status(), 304);
    assert!(!not_modified.headers().contains_key("x-image-cache"));
    assert_eq!(r.stub.last().headers["if-none-match"], "\"c1\"");

    // If-Match 不符：412 原样回给客户端。
    let failed = r
        .get(&target)
        .header("if-match", "\"other\"")
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), 412);

    // 客户端拿着过期 ETag：上游 200，但 ETag 与缓存相同，直接用缓存。
    let stale = r
        .get(&target)
        .header("if-none-match", "\"old\"")
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 200);
    assert_eq!(stale.headers()["x-image-cache"], "HIT");
}

#[tokio::test]
async fn upstream_errors_on_the_processing_path_pass_through_without_retry() {
    let r = rig(2, 2).await;
    let mut private = image_object(png(80, 60), "\"x\"");
    private.private = true;
    r.stub.put("/bucket/private", private);
    for (path, status, code) in [
        ("/bucket/missing", 404, "NoSuchKey"),
        ("/bucket/private", 403, "AccessDenied"),
    ] {
        let before = r.stub.seen().len();
        let response = r
            .get(&format!("{path}?{}", process(LFIT_256)))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert_eq!(response.headers()["x-amz-request-id"], "STUB");
        assert!(!response.headers().contains_key("x-image-cache"));
        assert!(String::from_utf8_lossy(&response.bytes().await.unwrap()).contains(code));
        assert_eq!(r.stub.seen().len(), before + 1, "{path}");
    }
}

/// 读出网关错误：状态、XML 里的 Code / EC 与 x-oss-ec 头。
async fn oss_error(response: reqwest::Response) -> (u16, String, Option<String>, String) {
    let status = response.status().as_u16();
    let header = response
        .headers()
        .get("x-oss-ec")
        .map(|v| v.to_str().unwrap().to_owned());
    assert_eq!(response.headers()["content-type"], "application/xml");
    let id = response.headers()["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.text().await.unwrap();
    let tag = |name: &str| {
        body.split(&format!("<{name}>"))
            .nth(1)
            .and_then(|s| s.split(&format!("</{name}>")).next())
            .map(str::to_owned)
    };
    assert_eq!(tag("RequestId").as_deref(), Some(id.as_str()));
    assert_eq!(tag("EC"), header);
    (status, tag("Code").unwrap(), header, tag("HostId").unwrap())
}

#[tokio::test]
async fn invalid_process_strings_fail_like_oss_before_contacting_upstream() {
    let r = rig(2, 2).await;
    r.stub
        .put("/bucket/photo", image_object(png(80, 40), "\"p\""));
    for (value, status, code, ec) in [
        (
            "image/resize,w_abc",
            400,
            "InvalidArgument",
            Some("0040-00000312"),
        ),
        (
            "image/slim,1",
            400,
            "InvalidArgument",
            Some("0040-00000213"),
        ),
        ("image/", 400, "MissingArgument", Some("0040-00000212")),
        (
            "image/resize",
            400,
            "MissingArgument",
            Some("0040-00000322"),
        ),
        ("image/crop,w_16385", 400, "CutEdgeExceedRange", None),
        ("image/average-hue/resize,w_10", 400, "BadRequest", None),
        ("style/thumb", 404, "NoSuchStyle", None),
    ] {
        let response = r
            .get(&format!("/bucket/photo?{}", process(value)))
            .send()
            .await
            .unwrap();
        let (got_status, got_code, got_ec, host) = oss_error(response).await;
        assert_eq!(
            (got_status, got_code.as_str(), got_ec.as_deref()),
            (status, code, ec),
            "{value}"
        );
        assert_eq!(host, "bucket.example.test");
    }
    assert!(r.stub.seen().is_empty());
}

#[tokio::test]
async fn non_images_and_oversized_sources_fail_like_oss() {
    let r = rig(2, 2).await;
    let mut text = image_object(b"<svg onload='alert(1)'/>".to_vec(), "\"t\"");
    text.headers[0].1 = "image/svg+xml".into();
    r.stub.put("/bucket/svg", text);
    let mut big = b"\x89PNG\r\n\x1a\n".to_vec();
    big.resize(20 * 1024 * 1024 + 1, 0);
    r.stub.put("/bucket/big", image_object(big, "\"b\""));
    for (path, code, ec) in [
        ("/bucket/svg", "BadRequest", "0040-00000005"),
        ("/bucket/big", "ImageTooLarge", "0040-00000001"),
    ] {
        let response = r
            .get(&format!("{path}?{}", process(LFIT_256)))
            .send()
            .await
            .unwrap();
        let (status, got_code, got_ec, _) = oss_error(response).await;
        assert_eq!(
            (status, got_code.as_str(), got_ec.as_deref()),
            (400, code, Some(ec)),
            "{path}"
        );
    }
}

#[tokio::test]
async fn slow_or_missing_upstreams_return_gateway_errors() {
    let r = rig(1, 1).await;
    r.stub
        .put("/bucket/slow", image_object(png(10, 10), "\"s\""));
    *r.stub.delay.lock().unwrap() = Some(Duration::from_secs(3));
    let (status, code, ec, _) = oss_error(
        r.get(&format!("/bucket/slow?{}", process(LFIT_256)))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, code.as_str(), ec), (504, "InternalError", None));

    let dead = serve(
        web::app(Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            upstream: "http://127.0.0.1:1".into(),
            max_concurrent: 1,
            fetch_timeout_seconds: 1,
        })
        .unwrap(),
    )
    .await;
    let response = r
        .client
        .get(format!("{}/bucket/x", dead.origin))
        .send()
        .await
        .unwrap();
    let (status, code, _, _) = oss_error(response).await;
    assert_eq!((status, code.as_str()), (502, "InternalError"));
}

#[tokio::test]
async fn queued_and_cancelled_requests_release_their_slots() {
    let r = rig(1, 5).await;
    r.stub
        .put("/bucket/slow", image_object(png(10, 10), "\"s\""));
    r.stub
        .put("/bucket/fast", image_object(png(10, 10), "\"f\""));
    *r.stub.delay.lock().unwrap() = Some(Duration::from_secs(30));
    let first = tokio::spawn(r.get(&format!("/bucket/slow?{}", process(LFIT_256))).send());
    while r.stub.seen().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // 名额被占用时第二个请求只排队、不回源；取消后队列与名额都要释放。
    let queued = tokio::spawn(r.get(&format!("/bucket/fast?{}", process(LFIT_256))).send());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(r.stub.seen().len(), 1);
    queued.abort();
    first.abort();
    let _ = (queued.await, first.await);
    *r.stub.delay.lock().unwrap() = None;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        r.get(&format!("/bucket/fast?{}", process(LFIT_256))).send(),
    )
    .await
    .expect("slot released")
    .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn page_bursts_wait_within_the_worker_budget_and_never_retry() {
    let r = rig(2, 5).await;
    for i in 0..24 {
        r.stub.put(
            &format!("/bucket/{i}"),
            image_object(png(160, 120), &format!("\"{i}\"")),
        );
    }
    *r.stub.delay.lock().unwrap() = Some(Duration::from_millis(40));
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..24 {
        let request = r.get(&format!("/bucket/{i}?{}", process(FILL_64)));
        tasks.spawn(async move {
            let response = request.send().await.unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(dimensions(&response.bytes().await.unwrap()), (64, 64));
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    assert_eq!(r.stub.seen().len(), 24);
    assert!(r.stub.peak.load(Ordering::SeqCst) <= 2);
}
