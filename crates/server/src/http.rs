use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, Response, StatusCode, body::Incoming, header};
use hyper_util::rt::{TokioExecutor, TokioIo};
use rust_embed::Embed;
use std::sync::Arc;

use crate::session_mgr::SharedSessionManager;
use saw_core::crypto::auth::AuthKey;

/// Type alias for the response body type used by all HTTP handlers.
type ResBody = http_body_util::combinators::BoxBody<Bytes, std::convert::Infallible>;

fn full_body(data: impl Into<Bytes>) -> ResBody {
    Full::new(data.into()).boxed()
}

fn asset_body(data: std::borrow::Cow<'static, [u8]>) -> ResBody {
    match data {
        std::borrow::Cow::Borrowed(slice) => Full::new(Bytes::from_static(slice)).boxed(),
        std::borrow::Cow::Owned(vec) => Full::new(Bytes::from(vec)).boxed(),
    }
}

/// Shared application state for HTTP handlers (WebSocket, static files).
pub struct AppState {
    pub auth_key: Option<AuthKey>,
    pub session_mgr: SharedSessionManager,
    pub shared_authorized_keys: crate::ssh::SharedAuthorizedKeys,
    pub webrtc_public_ip: Option<std::net::IpAddr>,
    pub webrtc_mux: std::sync::Arc<crate::webrtc::WebRtcMux>,
    /// When set, serve web frontend from this directory instead of embedded assets.
    pub web_dir: Option<std::path::PathBuf>,
}

/// Embedded static files from the web frontend build output (pre-compressed by build.rs).
#[derive(Embed)]
#[folder = "web-compressed/"]
struct WebAssets;

/// Whether a file extension is worth gzip-compressing (matches build.rs logic).
fn is_compressible(path: &str) -> bool {
    matches!(
        std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str()),
        Some("js" | "css" | "wasm" | "html" | "svg" | "json" | "xml" | "txt")
    )
}

/// Compute FNV-1a ETag from data (same algorithm as build.rs).
fn compute_etag(data: &[u8]) -> String {
    let mut hash: u64 = 14695981039346656037;
    for &byte in data.iter().take(65536) {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("\"{:016x}\"", hash)
}

/// Serve a single HTTP request.
/// Handles static files, WebSocket upgrade, and /api/ping.
fn serve_http_request(req: Request<Incoming>, app_state: Arc<AppState>) -> Response<ResBody> {
    let path = req.uri().path();

    // WebSocket upgrade: /ws
    // Supports both HTTP/1.1 Upgrade and HTTP/2 CONNECT (RFC 8441).
    if path == "/ws" {
        // HTTP/1.1: Upgrade: websocket
        if let Some(upgrade_header) = req.headers().get(header::UPGRADE)
            && upgrade_header.as_bytes().eq_ignore_ascii_case(b"websocket")
        {
            return handle_ws_upgrade_h1(req, app_state);
        }
        // HTTP/2: CONNECT method with :protocol=websocket (RFC 8441)
        if req.method() == Method::CONNECT {
            return handle_ws_upgrade_h2(req, app_state);
        }
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(full_body("Expected WebSocket upgrade"))
            .unwrap();
    }

    // Diagnostic endpoint: /api/ping
    if path == "/api/ping" {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/plain")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(full_body("pong"))
            .unwrap();
    }

    // Only allow GET and HEAD for static files
    if req.method() != Method::GET && req.method() != Method::HEAD {
        return Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(full_body("Method Not Allowed"))
            .unwrap();
    }

    // Normalize path: / -> index.html, strip leading /
    let asset_path = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };

    // Resolve: exact match first, then SPA fallback to index.html
    let (resolved_path, asset_data, from_fs) = if let Some(ref web_dir) = app_state.web_dir {
        // Development mode: serve from filesystem (no gzip pre-compression)
        let file_path = web_dir.join(asset_path);
        if file_path.is_file() {
            match std::fs::read(&file_path) {
                Ok(data) => (asset_path.to_string(), data, true),
                Err(_) => {
                    // SPA fallback
                    let index_path = web_dir.join("index.html");
                    match std::fs::read(&index_path) {
                        Ok(data) => ("index.html".to_string(), data, true),
                        Err(_) => {
                            return Response::builder()
                                .status(StatusCode::NOT_FOUND)
                                .header(header::CONTENT_TYPE, "text/plain")
                                .body(full_body("Not Found"))
                                .unwrap();
                        }
                    }
                }
            }
        } else {
            // SPA fallback
            let index_path = web_dir.join("index.html");
            match std::fs::read(&index_path) {
                Ok(data) => ("index.html".to_string(), data, true),
                Err(_) => {
                    return Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .header(header::CONTENT_TYPE, "text/plain")
                        .body(full_body("Not Found"))
                        .unwrap();
                }
            }
        }
    } else if let Some(a) = WebAssets::get(asset_path) {
        (asset_path.to_string(), a.data.to_vec(), false)
    } else if let Some(a) = WebAssets::get("index.html") {
        ("index.html".to_string(), a.data.to_vec(), false)
    } else {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .header(header::CONTENT_TYPE, "text/plain")
            .body(full_body("Not Found"))
            .unwrap();
    };

    let mime = mime_guess::from_path(&resolved_path).first_or_octet_stream();
    // Filesystem mode: files are not pre-gzip-compressed, so don't set Content-Encoding
    let is_gzipped = !from_fs && is_compressible(&resolved_path);

    // Vite-hashed assets (assets/*) are immutable; index.html needs revalidation.
    let cache_control = if resolved_path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=0, must-revalidate"
    };

    // ETag for non-immutable assets: compute from data
    if !resolved_path.starts_with("assets/") {
        let etag = compute_etag(&asset_data);
        if let Some(inm) = req.headers().get(header::IF_NONE_MATCH)
            && inm.as_bytes() == etag.as_bytes()
        {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, etag)
                .header(header::CACHE_CONTROL, cache_control)
                .header(header::VARY, "Accept-Encoding")
                .body(full_body(Bytes::new()))
                .unwrap();
        }

        if is_gzipped {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime.as_ref())
                .header(header::CONTENT_ENCODING, "gzip")
                .header(header::ETAG, etag)
                .header(header::CACHE_CONTROL, cache_control)
                .header(header::VARY, "Accept-Encoding")
                .body(asset_body(asset_data.into()))
                .unwrap();
        }

        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::VARY, "Accept-Encoding")
            .body(asset_body(asset_data.into()))
            .unwrap();
    }

    // Immutable assets (assets/*): no ETag needed, content-hash in filename
    if is_gzipped {
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::CONTENT_ENCODING, "gzip")
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::VARY, "Accept-Encoding")
            .body(asset_body(asset_data.into()))
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime.as_ref())
        .header(header::CACHE_CONTROL, cache_control)
        .header(header::VARY, "Accept-Encoding")
        .body(asset_body(asset_data.into()))
        .unwrap()
}

/// Handle WebSocket upgrade via HTTP/1.1 (Upgrade: websocket).
/// Creates a native WebSocket connection for browser clients using
/// type-prefixed binary messages (0x00=Control, 0x01=TerminalIO output, 0x02=TerminalIO input).
fn handle_ws_upgrade_h1(req: Request<Incoming>, app_state: Arc<AppState>) -> Response<ResBody> {
    let key = req
        .headers()
        .get("Sec-WebSocket-Key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let accept_key = compute_ws_accept_key(key);

    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let io = TokioIo::new(upgraded);
                let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                    io,
                    tokio_tungstenite::tungstenite::protocol::Role::Server,
                    None,
                )
                .await;

                if let Err(e) = crate::listener::handle_native_ws(ws, app_state).await {
                    log::debug!("native WS handler error: {}", e);
                }
            }
            Err(e) => {
                log::debug!("WebSocket upgrade failed: {}", e);
            }
        }
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header("Sec-WebSocket-Accept", accept_key)
        .body(full_body(Bytes::new()))
        .unwrap()
}

/// Handle WebSocket upgrade via HTTP/2 CONNECT (RFC 8441).
/// The browser sends CONNECT with :protocol=websocket pseudo-header.
/// We respond with 200 OK and the upgraded stream becomes a raw WebSocket.
fn handle_ws_upgrade_h2(req: Request<Incoming>, app_state: Arc<AppState>) -> Response<ResBody> {
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let io = TokioIo::new(upgraded);
                let ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                    io,
                    tokio_tungstenite::tungstenite::protocol::Role::Server,
                    None,
                )
                .await;

                if let Err(e) = crate::listener::handle_native_ws(ws, app_state).await {
                    log::debug!("native WS handler error: {}", e);
                }
            }
            Err(e) => {
                log::debug!("WebSocket h2 upgrade failed: {}", e);
            }
        }
    });

    // RFC 8441: respond with 200 OK (not 101 Switching Protocols)
    Response::builder()
        .status(StatusCode::OK)
        .body(full_body(Bytes::new()))
        .unwrap()
}

/// Compute Sec-WebSocket-Accept value from Sec-WebSocket-Key.
/// RFC 6455: SHA-1(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"), base64 encoded.
fn compute_ws_accept_key(key: &str) -> String {
    use sha1::{Digest as _, Sha1};
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    BASE64.encode(hasher.finalize())
}

/// Handle an HTTPS connection on a TLS stream detected as HTTP traffic.
/// Auto-detects HTTP/1.1 or HTTP/2 based on ALPN negotiation.
/// Supports WebSocket upgrades.
pub async fn handle_http_stream<S>(
    stream: S,
    app_state: Arc<AppState>,
    local_addr: std::net::SocketAddr,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = TokioIo::new(stream);
    let app_state = app_state.clone();
    let service = hyper::service::service_fn(move |req| {
        let app_state = app_state.clone();
        let local_addr = local_addr.ip();
        async move {
            // Async endpoints (need body reading)
            if req.uri().path() == "/api/webrtc/offer" && req.method() == Method::POST {
                let resp = handle_webrtc_offer(req, app_state, local_addr).await;
                return Ok::<_, std::convert::Infallible>(resp);
            }
            let resp = serve_http_request(req, app_state);
            Ok::<_, std::convert::Infallible>(resp)
        }
    });

    // Auto-detect HTTP/1.1 or HTTP/2 based on ALPN, with upgrade support
    // Enable h2 extended CONNECT protocol (RFC 8441) for WebSocket over HTTP/2.
    let mut builder = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
    builder.http2().enable_connect_protocol();
    if let Err(e) = builder.serve_connection_with_upgrades(io, service).await {
        log::debug!("HTTP connection error: {}", e);
    }
}

/// Handle a plain HTTP connection by redirecting to HTTPS.
/// Uses 301 redirect + HTML meta refresh + anchor tag for maximum compatibility.
pub async fn handle_http_redirect<S>(stream: S, listen_addr: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let listen_addr = listen_addr.to_string();
    let io = TokioIo::new(stream);
    let service = hyper::service::service_fn(move |req| {
        let listen_addr = listen_addr.clone();
        async move {
            // Build HTTPS URL from the request
            let host = req
                .headers()
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or(&listen_addr);
            let uri = req.uri();
            let path_and_query = uri.path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
            let https_url = format!("https://{}{}", host, path_and_query);

            // HTML body with meta refresh + anchor tag as fallback
            let html = format!(
                r#"<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<meta http-equiv="refresh" content="0;url={url}">
<title>Redirecting to HTTPS</title>
</head><body>
<noscript><p>Redirecting to <a href="{url}">{url}</a></p></noscript>
<script>window.location.replace({url_quoted});</script>
</body></html>"#,
                url = html_escape(&https_url),
                url_quoted = json_escape(&https_url),
            );

            Ok::<_, std::convert::Infallible>(
                Response::builder()
                    .status(StatusCode::MOVED_PERMANENTLY)
                    .header(header::LOCATION, &https_url)
                    .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                    .body(full_body(html))
                    .unwrap(),
            )
        }
    });

    if let Err(e) = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new())
        .serve_connection(io, service)
        .await
    {
        log::debug!("HTTP redirect connection error: {}", e);
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('"', "&quot;")
}

fn json_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Handle POST /api/webrtc/offer — SDP signaling for WebRTC Data Channel.
async fn handle_webrtc_offer(
    req: Request<Incoming>,
    app_state: Arc<AppState>,
    local_addr: std::net::IpAddr,
) -> Response<ResBody> {
    // Determine the ICE candidate IP from the Host header the browser used.
    // Priority: config override > Host IP > Host domain DNS resolve > auto-detect
    let candidate_ip = resolve_candidate_ip(&req, &app_state, local_addr);

    // Read request body
    let body = match req.into_body().collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(full_body("Failed to read request body"))
                .unwrap();
        }
    };

    // Parse JSON { sdp: "..." }
    let offer_req: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(full_body("Invalid JSON"))
                .unwrap();
        }
    };

    let offer_sdp = match offer_req.get("sdp").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(full_body("Missing 'sdp' field"))
                .unwrap();
        }
    };

    // Handle offer via webrtc module
    match crate::webrtc::handle_offer(
        offer_sdp,
        candidate_ip,
        &app_state.webrtc_mux,
        app_state.session_mgr.clone(),
        app_state.auth_key.clone(),
    )
    .await
    {
        Ok(answer_sdp) => {
            let answer_json = serde_json::json!({
                "sdp": answer_sdp,
                "iceServers": []
            });
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .body(full_body(serde_json::to_vec(&answer_json).unwrap()))
                .unwrap()
        }
        Err(e) => {
            log::warn!("WebRTC offer error: {}", e);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "text/plain")
                .body(full_body(format!("WebRTC error: {}", e)))
                .unwrap()
        }
    }
}

/// Resolve the ICE candidate IP for WebRTC.
/// Priority: config override > Host header IP > connection local addr > auto-detect (8.8.8.8 probe)
fn resolve_candidate_ip(
    req: &Request<Incoming>,
    app_state: &AppState,
    local_addr: std::net::IpAddr,
) -> Option<std::net::IpAddr> {
    // 1. Config override (for NAT scenarios)
    if let Some(ip) = app_state.webrtc_public_ip {
        return Some(ip);
    }

    // 2. Host header is an IP — browser connected directly, use it
    if let Some(ip) = extract_host_ip(req) {
        return Some(ip);
    }

    // 3. Connection local address (the IP the browser actually connected to)
    if !local_addr.is_unspecified() {
        return Some(local_addr);
    }

    // 4. Auto-detect: UDP probe to 8.8.8.8 reveals the outbound interface IP
    detect_local_ip()
}

/// Extract an IP address from the Host header (if it is one, not a domain).
fn extract_host_ip(req: &Request<Incoming>) -> Option<std::net::IpAddr> {
    let host_str = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let host = if host_str.starts_with('[') {
        // IPv6: [::1]:port → ::1
        host_str
            .trim_start_matches('[')
            .split(']')
            .next()
            .unwrap_or(host_str)
    } else {
        // IPv4: 192.168.1.5:port → 192.168.1.5
        host_str.split(':').next().unwrap_or(host_str)
    };

    host.parse().ok()
}

/// Detect the local IP address by "connecting" a UDP socket to a public address.
/// This doesn't send any data — the OS picks the outbound interface and we read its IP.
fn detect_local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}
