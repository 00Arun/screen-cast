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

const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
media-src 'self' blob: mediastream:; connect-src 'self'; base-uri 'none'; form-action 'self'; \
frame-ancestors 'none'";

pub struct Sessions {
    pin: String,
    tokens: HashMap<String, Instant>, // viewer token -> created
    fails: HashMap<IpAddr, (u32, Instant)>,
}

pub struct ServerState {
    pub sessions: Mutex<Sessions>,
    pub hub: Arc<Hub>,
    pub port: u16,
}

impl ServerState {
    pub fn new(hub: Arc<Hub>, port: u16) -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(Sessions {
                pin: new_pin(),
                tokens: HashMap::new(),
                fails: HashMap::new(),
            }),
            hub,
            port,
        })
    }

    pub fn pin(&self) -> String {
        self.sessions.lock().unwrap().pin.clone()
    }

    /// New PIN and every viewer is disconnected and must re-enter it.
    pub fn rotate_pin(&self) -> String {
        let mut s = self.sessions.lock().unwrap();
        s.pin = new_pin();
        s.tokens.clear();
        drop(s);
        self.hub.close_all();
        self.pin()
    }

    fn valid_token(&self, headers: &HeaderMap) -> Option<String> {
        let token = cookie(headers, "sc_view")?;
        let mut s = self.sessions.lock().unwrap();
        let now = Instant::now();
        s.tokens.retain(|_, t| now.duration_since(*t) < SESSION_TTL);
        s.tokens.keys().find(|k| ct_eq(k, &token)).cloned()
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
    pin: String,
}

async fn join(
    State(st): State<Arc<ServerState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(body): Json<JoinReq>,
) -> Response {
    let ip = addr.ip();
    let now = Instant::now();
    let mut s = st.sessions.lock().unwrap();

    if let Some((n, since)) = s.fails.get(&ip).copied() {
        if n >= MAX_PIN_FAILS {
            if now.duration_since(since) < PIN_LOCK {
                return err(StatusCode::TOO_MANY_REQUESTS, "too many attempts");
            }
            s.fails.remove(&ip);
        }
    }
    if !ct_eq(body.pin.trim(), &s.pin) {
        let e = s.fails.entry(ip).or_insert((0, now));
        e.0 += 1;
        e.1 = now;
        return err(StatusCode::UNAUTHORIZED, "wrong PIN");
    }
    s.fails.remove(&ip);
    s.tokens.retain(|_, t| now.duration_since(*t) < SESSION_TTL);
    if s.tokens.len() >= MAX_VIEWERS * 4 {
        return err(StatusCode::SERVICE_UNAVAILABLE, "too many sessions");
    }
    let token = random_hex(32);
    s.tokens.insert(token.clone(), now);
    drop(s);

    let mut r = Json(json!({ "ok": true })).into_response();
    let c = format!("sc_view={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=43200");
    r.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&c).unwrap());
    r
}

async fn me(State(st): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    match st.valid_token(&headers) {
        Some(_) => Json(json!({ "ok": true })).into_response(),
        None => err(StatusCode::UNAUTHORIZED, "not joined"),
    }
}

/// WHEP-style: viewer POSTs its SDP offer, gets the SDP answer back.
async fn watch(State(st): State<Arc<ServerState>>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(token) = st.valid_token(&headers) else {
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
    if st.hub.viewer_count() >= MAX_VIEWERS && !st.hub.has_viewer(&token) {
        return err(StatusCode::SERVICE_UNAVAILABLE, "too many viewers");
    }
    match st.hub.answer(token, offer.to_string()).await {
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
    let _ = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await;
}
