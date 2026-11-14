//! LAN web server that viewers open in their browser.
//!
//! Viewers receive ONLY an encrypted WebRTC media stream (pixels + sound) of the app's
//! browser area. Nothing from the website being watched (HTML, JS, cookies, ads, popups,
//! downloads) is ever sent to them.
//!
//! Hardening: LAN-only clients, random 6-digit PIN with lock-out, HttpOnly+SameSite=Strict
//! session cookies, Host-header allow-list (DNS rebinding), Origin check on POST (CSRF),
//! strict CSP and other security headers, small request bodies, viewer cap.

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, DefaultBodyLimit, Request, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rand::{Rng, RngCore};
use serde::Deserialize;
use serde_json::json;

use crate::hub::Hub;

pub const PORT: u16 = 8080;
const MAX_VIEWERS: usize = 12;
const MAX_PIN_FAILS: u32 = 5;
const PIN_LOCK: Duration = Duration::from_secs(5 * 60);
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Network-wide wrong-PIN budget: beyond this many failures in the window the PIN rotates,
/// so spreading guesses over many IP addresses doesn't help an attacker.
const GLOBAL_FAILS: usize = 20;
const GLOBAL_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_CONNECTIONS: usize = 64;

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
media-src 'self' blob: mediastream:; connect-src 'self'; base-uri 'none'; form-action 'self'; \
frame-ancestors 'none'";

pub struct Sessions {
    pin: String,
    /// Long random secret put in the QR code instead of the PIN; changes with the PIN.
    qr_token: String,
    tokens: HashMap<String, (Instant, IpAddr)>, // viewer token -> (created, bound IP)
    fails: HashMap<IpAddr, (u32, Instant)>,
    global_fails: Vec<Instant>,
}

pub struct ServerState {
    pub sessions: Mutex<Sessions>,
    pub hub: Arc<Hub>,
    pub port: u16,
    /// Called when the PIN changes on its own (brute-force protection) so the UI updates.
    pub on_pin_change: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
}

impl ServerState {
    pub fn new(hub: Arc<Hub>, port: u16) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(Sessions {
                pin: new_pin(),
                qr_token: random_hex(16),
                tokens: HashMap::new(),
                fails: HashMap::new(),
                global_fails: Vec::new(),
            }),
            hub,
            port,
            on_pin_change: std::sync::OnceLock::new(),
        })
    }

    pub fn pin(&self) -> String {
        self.sessions.lock().unwrap().pin.clone()
    }

    pub fn qr_token(&self) -> String {
        self.sessions.lock().unwrap().qr_token.clone()
    }

    /// New PIN (and QR secret); every viewer is disconnected and must join again.
    pub fn rotate_pin(&self) -> String {
        let mut s = self.sessions.lock().unwrap();
        Self::rotate_locked(&mut s);
        drop(s);
        self.hub.close_all();
        self.pin()
    }

    fn rotate_locked(s: &mut Sessions) {
        s.pin = new_pin();
        s.qr_token = random_hex(16);
        s.tokens.clear();
        s.global_fails.clear();
    }

    /// Ends every viewer session (cast stopped).
    pub fn end_sessions(&self) {
        self.sessions.lock().unwrap().tokens.clear();
    }

    /// Session token from the cookie, valid only from the IP address it was issued to.
    fn valid_token(&self, headers: &HeaderMap, ip: IpAddr) -> Option<String> {
        let token = cookie(headers, "sc_view")?;
        let mut s = self.sessions.lock().unwrap();
        let now = Instant::now();
        s.tokens.retain(|_, (t, _)| now.duration_since(*t) < SESSION_TTL);
        s.tokens.iter().find(|(k, (_, bound))| ct_eq(k, &token) && *bound == ip).map(|(k, _)| k.clone())
    }
}

fn new_pin() -> String {
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000))
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Constant-time comparison for PINs / tokens.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---------------------------------------------------------------------------------------------
// Network helpers
// ---------------------------------------------------------------------------------------------

pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local(),
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_lan(IpAddr::V4(v4));
            }
            let s0 = v.segments()[0];
            v.is_loopback() || (s0 & 0xfe00) == 0xfc00 || (s0 & 0xffc0) == 0xfe80
        }
    }
}

pub fn lan_ip() -> Option<Ipv4Addr> {
    // Developer override (used by --selftest in restricted environments).
    if let Some(ip) = std::env::var("SC_BIND_IP").ok().and_then(|s| s.parse().ok()) {
        return Some(ip);
    }
    // No packet is sent: connecting a UDP socket only selects the outgoing interface.
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(v) if !v.is_loopback() && !v.is_unspecified() => Some(v),
        _ => None,
    }
}

fn raw_host(headers: &HeaderMap) -> Option<String> {
    Some(headers.get(header::HOST)?.to_str().ok()?.to_ascii_lowercase())
}

fn host_name(headers: &HeaderMap) -> Option<String> {
    let h = raw_host(headers)?;
    if let Some(rest) = h.strip_prefix('[') {
        return Some(rest[..rest.find(']')?].to_string());
    }
    Some(match h.rsplit_once(':') {
        Some((name, _)) => name.to_string(),
        None => h,
    })
}

/// Only IP literals, localhost or mDNS names — blocks DNS-rebinding attacks.
fn host_header_ok(headers: &HeaderMap) -> bool {
    host_name(headers)
        .is_some_and(|n| n == "localhost" || n.parse::<IpAddr>().is_ok() || n.ends_with(".local"))
}

fn origin_matches(headers: &HeaderMap) -> bool {
    let (Some(origin), Some(host)) = (
        headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()),
        raw_host(headers),
    ) else {
        return false;
    };
    let origin = origin.to_ascii_lowercase();
    origin.strip_prefix("http://").is_some_and(|a| a == host)
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.to_string())
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

// ---------------------------------------------------------------------------------------------
// Security middleware
// ---------------------------------------------------------------------------------------------

async fn security(ConnectInfo(addr): ConnectInfo<SocketAddr>, req: Request, next: Next) -> Response {
    if !is_lan(addr.ip()) {
        return err(StatusCode::FORBIDDEN, "local network only");
    }
    if !host_header_ok(req.headers()) {
        return err(StatusCode::MISDIRECTED_REQUEST, "bad host");
    }
    if req.method() != Method::GET && req.method() != Method::HEAD && !origin_matches(req.headers()) {
        return err(StatusCode::FORBIDDEN, "bad origin");
    }
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    for (k, v) in [
        ("content-security-policy", CSP),
        ("x-frame-options", "DENY"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cross-origin-opener-policy", "same-origin"),
        ("cross-origin-resource-policy", "same-origin"),
        ("cache-control", "no-store"),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=(), display-capture=(), usb=(), payment=()",
        ),
    ] {
        h.insert(k, HeaderValue::from_static(v));
    }
    res
}

// ---------------------------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------------------------

fn asset(body: &'static str, ctype: &'static str) -> Response {
    let mut r = body.into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(ctype));
    r
}

async fn viewer_html() -> Response {
    asset(include_str!("../../web/viewer.html"), "text/html; charset=utf-8")
}
async fn viewer_js() -> Response {
    asset(include_str!("../../web/viewer.js"), "text/javascript; charset=utf-8")
}
async fn style_css() -> Response {
    asset(include_str!("../../web/style.css"), "text/css; charset=utf-8")
}

#[derive(Deserialize)]
struct JoinReq {
    #[serde(default)]
    pin: String,
    /// Secret from the QR code (alternative to the PIN).
    #[serde(default)]
    token: String,
}

async fn join(
    State(st): State<Arc<ServerState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<JoinReq>,
) -> Response {
    let ip = addr.ip();
    let now = Instant::now();
    if !st.hub.is_live() {
        return err(StatusCode::CONFLICT, "not casting");
    }
    let mut s = st.sessions.lock().unwrap();

    if let Some((n, since)) = s.fails.get(&ip).copied() {
        if n >= MAX_PIN_FAILS {
            if now.duration_since(since) < PIN_LOCK {
                return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
            }
            s.fails.remove(&ip);
        }
    }
    let ok = (!body.pin.is_empty() && ct_eq(body.pin.trim(), &s.pin))
        || (!body.token.is_empty() && ct_eq(body.token.trim(), &s.qr_token));
    if !ok {
        let e = s.fails.entry(ip).or_insert((0, now));
        e.0 += 1;
        e.1 = now;
        s.global_fails.retain(|t| now.duration_since(*t) < GLOBAL_WINDOW);
        s.global_fails.push(now);
        if s.global_fails.len() >= GLOBAL_FAILS {
            // Too many wrong guesses across the network: make the old PIN worthless.
            ServerState::rotate_locked(&mut s);
            drop(s);
            st.hub.close_all();
            if let Some(f) = st.on_pin_change.get() {
                f();
            }
            return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
        }
        return err(StatusCode::UNAUTHORIZED, "wrong PIN");
    }
    s.fails.remove(&ip);
    s.tokens.retain(|_, (t, _)| now.duration_since(*t) < SESSION_TTL);
    if s.tokens.len() >= MAX_VIEWERS * 4 {
        return err(StatusCode::SERVICE_UNAVAILABLE, "too many sessions");
    }
    let token = random_hex(32);
    s.tokens.insert(token.clone(), (now, ip));
    drop(s);

    let mut r = Json(json!({ "ok": true })).into_response();
    let c = format!("sc_view={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200");
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&c).unwrap());
    r
}

async fn me(
    State(st): State<Arc<ServerState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Response {
    match st.valid_token(&headers, addr.ip()) {
        Some(_) => Json(json!({ "ok": true })).into_response(),
        None => err(StatusCode::UNAUTHORIZED, "not joined"),
    }
}

/// WHEP-style: viewer POSTs its SDP offer, gets the SDP answer back.
async fn watch(
    State(st): State<Arc<ServerState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(token) = st.valid_token(&headers, addr.ip()) else {
        return err(StatusCode::UNAUTHORIZED, "not joined");
    };
    if !st.hub.is_live() {
        return err(StatusCode::CONFLICT, "not casting");
    }
    let Ok(offer) = std::str::from_utf8(&body) else {
        return err(StatusCode::BAD_REQUEST, "bad sdp");
    };
    if !offer.starts_with("v=0") {
        return err(StatusCode::BAD_REQUEST, "bad sdp");
    }
    match st.hub.answer(token, offer.to_string(), MAX_VIEWERS).await {
        Ok(answer) => {
            let mut r = answer.into_response();
            r.headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/sdp"));
            r
        }
        Err(e) => {
            eprintln!("watch failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "could not start stream")
        }
    }
}

pub async fn run(state: Arc<ServerState>) {
    let app = Router::new()
        .route("/", get(viewer_html))
        .route("/viewer.js", get(viewer_js))
        .route("/style.css", get(style_css))
        .route("/api/join", post(join))
        .route("/api/me", get(me))
        .route("/api/watch", post(watch))
        .layer(DefaultBodyLimit::max(32 * 1024))
        .layer(middleware::from_fn(security))
        .with_state(state.clone());

    let listener = match tokio::net::TcpListener::bind(("0.0.0.0", state.port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Could not listen on port {}: {e}", state.port);
            return;
        }
    };
    // Own accept loop: LAN check at accept time, a connection cap, and a header-read timeout
    // (slow-loris protection), which axum::serve doesn't provide.
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok((stream, peer)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        if !is_lan(peer.ip()) {
            continue;
        }
        let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
        let svc = app.clone().layer(axum::Extension(ConnectInfo(peer)));
        tokio::spawn(async move {
            let _permit = permit;
            let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let mut svc = svc.clone();
                async move { tower_service::Service::call(&mut svc, req.map(axum::body::Body::new)).await }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(hyper_util::rt::TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10))
                .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                .await;
        });
    }
}
