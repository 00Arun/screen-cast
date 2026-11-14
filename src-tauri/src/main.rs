//! Screen Cast — a sandboxed browser for macOS whose content can be cast to other devices on
//! the local network. Viewers receive only video pixels + audio, never web content.

mod adblock;
mod audio;
mod capture;
mod contentrules;
#[cfg(target_os = "macos")]
mod encoder;
mod hub;
mod netguard;
#[cfg(target_os = "macos")]
mod selftest;
mod server;

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use serde::Serialize;
use tauri::{
    webview::{DownloadEvent, NewWindowResponse, PageLoadEvent, WebviewBuilder},
    window::WindowBuilder,
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, Url, WebviewUrl, WindowEvent,
};

use crate::{adblock::Blocker, hub::Hub, server::ServerState};

pub const TOOLBAR_H: f64 = 56.0;
const HOME: &str = "https://duckduckgo.com/";
const WINDOW_TITLE: &str = "Screen Cast";

pub struct AppState {
    blocker: Arc<Blocker>,
    server: Arc<ServerState>,
    hub: Arc<Hub>,
    capture: Mutex<Option<capture::Capture>>,
    url: Mutex<String>,
    blocked: AtomicU64,
    /// Bumped on every window resize; used to debounce capture restarts.
    resize_gen: AtomicU64,
    /// A video in the page is "fullscreen": window goes fullscreen, toolbar hidden.
    page_fs: std::sync::atomic::AtomicBool,
    page_fs_changed: Mutex<std::time::Instant>,
    /// The user left fullscreen with the window controls: ignore the page's fullscreen
    /// state until it has really left fullscreen itself (no forced re-entry).
    page_fs_suppressed: std::sync::atomic::AtomicBool,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct UiState {
    url: String,
    blocked: u64,
    casting: bool,
    pin: String,
    lan_url: Option<String>,
    viewers: usize,
    platform: &'static str,
    version: &'static str,
}

fn ui_state(app: &AppHandle) -> UiState {
    let st = app.state::<AppState>();
    let state = UiState {
        url: st.url.lock().unwrap().clone(),
        blocked: st.blocked.load(Ordering::Relaxed),
        casting: st.capture.lock().unwrap().is_some(),
        pin: st.server.pin(),
        lan_url: server::lan_ip().map(|ip| format!("http://{ip}:{}", server::PORT)),
        viewers: st.hub.viewer_count(),
        platform: std::env::consts::OS,
        version: env!("CARGO_PKG_VERSION"),
    };
    state
}

fn push_state(app: &AppHandle) {
    let state = ui_state(app);
    let _ = app.emit_to("toolbar", "state", state.clone());
    let _ = app.emit_to("qr", "state", state);
}

/// Link encoded in the QR code. It carries a long random secret (not the PIN) in the URL
/// fragment, which browsers never send over the network; the viewer page reads it once and
/// removes it. The secret stops working when the cast stops or the PIN changes.
fn watch_link(app: &AppHandle) -> Option<String> {
    let ip = server::lan_ip()?;
    let token = app.state::<AppState>().server.qr_token();
    Some(format!("http://{ip}:{}/#t={token}", server::PORT))
}

fn qr_svg(text: &str) -> Option<String> {
    use qrcodegen::{QrCode, QrCodeEcc};
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let border = 3;
    let size = qr.size() + border * 2;
    let mut path = String::new();
    for y in 0..qr.size() {
        for x in 0..qr.size() {
            if qr.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + border, y + border));
            }
        }
    }
    Some(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" shape-rendering="crispEdges"><rect width="{size}" height="{size}" fill="#fff"/><path d="{path}" fill="#000"/></svg>"##
    ))
}

#[derive(Serialize)]
struct QrInfo {
    svg: String,
    url: String,
    pin: String,
}

fn open_qr_window(app: &AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("qr") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(app, "qr", WebviewUrl::App("qr.html".into()))
        .title("Scan to watch")
        .inner_size(340.0, 470.0)
        .resizable(false)
        .minimizable(false)
        .always_on_top(true)
        .devtools(false)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn close_qr_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("qr") {
        let _ = w.close();
    }
}

fn note_blocked(app: &AppHandle, what: &str) {
    app.state::<AppState>().blocked.fetch_add(1, Ordering::Relaxed);
    let _ = app.emit_to("toolbar", "blocked", what.to_string());
    push_state(app);
}

fn is_local_host(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => server::is_lan(ip.into()) || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => server::is_lan(ip.into()) || ip.is_unspecified(),
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local")
        }
        None => true,
    }
}

/// Decide whether the sandboxed browser may load `url` in any frame.
fn navigation_allowed(app: &AppHandle, url: &Url) -> bool {
    match url.scheme() {
        "http" | "https" => {}
        "about" => return matches!(url.as_str(), "about:blank" | "about:srcdoc"),
        "blob" => return true, // in-page media players
        _ => {
            // file:, data:, tauri:, mailto:, itms-apps:, facetime:, custom app schemes...
            note_blocked(app, &format!("{} link", url.scheme()));
            return false;
        }
    }
    if is_local_host(url) {
        note_blocked(app, "local network access");
        return false;
    }
    let st = app.state::<AppState>();
    let source = st.url.lock().unwrap().clone();
    if st.blocker.should_block_navigation(url.as_str(), &source) {
        note_blocked(app, url.host_str().unwrap_or("ad"));
        return false;
    }
    true
}

fn content_webview(app: &AppHandle) -> Result<tauri::Webview, String> {
    app.get_webview("content").ok_or_else(|| "browser not ready".into())
}

fn parse_input(input: &str) -> Option<Url> {
    let s = input.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(u) = Url::parse(s) {
        if matches!(u.scheme(), "http" | "https") {
            return Some(u);
        }
    }
    if !s.contains(char::is_whitespace) && s.contains('.') {
        if let Ok(u) = Url::parse(&format!("https://{s}")) {
            return Some(u);
        }
    }
    let mut u = Url::parse(HOME).ok()?;
    u.query_pairs_mut().append_pair("q", s);
    Some(u)
}

// ---------------------------------------------------------------------------------------------
// Commands (only the local toolbar webview can call these)
// ---------------------------------------------------------------------------------------------

#[tauri::command]
fn get_state(app: AppHandle) -> UiState {
    ui_state(&app)
}

#[tauri::command]
fn navigate(app: AppHandle, input: String) -> Result<(), String> {
    let url = parse_input(&input).ok_or("Enter a website address")?;
    if is_local_host(&url) {
        return Err("Local network addresses are blocked in the sandbox".into());
    }
    content_webview(&app)?.navigate(url).map_err(|e| e.to_string())
}

/// Back/forward through the engine's native API (a page can override `history.back`).
fn history_step(app: &AppHandle, forward: bool) -> Result<(), String> {
    content_webview(app)?
        .with_webview(move |pv| {
            #[cfg(target_os = "macos")]
            unsafe {
                let wv = &*(pv.inner() as *const objc2_web_kit::WKWebView);
                if forward {
                    let _ = wv.goForward();
                } else {
                    let _ = wv.goBack();
                }
            }
            #[cfg(windows)]
            unsafe {
                if let Ok(core) = pv.controller().CoreWebView2() {
                    let _ = if forward { core.GoForward() } else { core.GoBack() };
                }
            }
            #[cfg(target_os = "linux")]
            {
                use webkit2gtk::WebViewExt;
                if forward {
                    pv.inner().go_forward();
                } else {
                    pv.inner().go_back();
                }
            }
        })
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn go_back(app: AppHandle) -> Result<(), String> {
    history_step(&app, false)
}

#[tauri::command]
fn go_forward(app: AppHandle) -> Result<(), String> {
    history_step(&app, true)
}

#[tauri::command]
fn reload(app: AppHandle) -> Result<(), String> {
    content_webview(&app)?.reload().map_err(|e| e.to_string())
}

#[tauri::command]
async fn toggle_cast(app: AppHandle) -> Result<(), String> {
    let st = app.state::<AppState>();
    let running = st.capture.lock().unwrap().take();
    if let Some(cap) = running {
        cap.stop();
        st.hub.set_live(false);
        st.server.end_sessions();
        close_qr_window(&app);
        push_state(&app);
        return Ok(());
    }
    let cap = start_capture(&app).await?;
    // Every cast gets a fresh PIN and QR secret.
    st.server.rotate_pin();
    *st.capture.lock().unwrap() = Some(cap);
    st.hub.set_live(true);
    push_state(&app);
    let _ = open_qr_window(&app);
    Ok(())
}

async fn start_capture(app: &AppHandle) -> Result<capture::Capture, String> {
    let window = app.get_window("main").ok_or("no window")?;
    let scale = window.scale_factor().map_err(|e| e.to_string())?;
    let size = window.inner_size().map_err(|e| e.to_string())?.to_logical::<f64>(scale);
    let target = capture::target(&window, WINDOW_TITLE)?;
    let bar = toolbar_height(app);
    let hub = app.state::<AppState>().hub.clone();
    tauri::async_runtime::spawn_blocking(move || {
        capture::Capture::start(hub, &target, size.width, size.height, bar)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// After the window is resized, enters/leaves full screen or moves to another display,
/// restart capture once things settle so the crop, resolution and window match again.
/// Viewers stay connected; they just get a fresh key frame.
fn schedule_capture_restart(app: &AppHandle) {
    let st = app.state::<AppState>();
    let generation = st.resize_gen.fetch_add(1, Ordering::SeqCst) + 1;
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(700)).await;
        let st = app.state::<AppState>();
        if st.resize_gen.load(Ordering::SeqCst) != generation {
            return;
        }
        if st.capture.lock().unwrap().is_none() {
            reveal_toolbar_if_needed(&app);
            return;
        }
        match start_capture(&app).await {
            Ok(new_cap) => {
                let old = st.capture.lock().unwrap().replace(new_cap);
                if let Some(old) = old {
                    old.stop();
                }
                st.hub.request_keyframe();
                reveal_toolbar_if_needed(&app);
            }
            Err(e) => {
                reveal_toolbar_if_needed(&app);
                let _ = app.emit_to("toolbar", "blocked", format!("Cast restart failed: {e}"));
            }
        }
    });
}

fn reveal_toolbar_if_needed(app: &AppHandle) {
    if !app.state::<AppState>().page_fs.load(Ordering::SeqCst) {
        if let Some(t) = app.get_webview("toolbar") {
            let _ = t.show();
        }
    }
}

#[tauri::command]
fn new_pin(app: AppHandle) {
    app.state::<AppState>().server.rotate_pin();
    push_state(&app);
}

pub const DEVELOPER_SITE: &str = "https://arun-pandey.com.np/";

#[derive(Serialize)]
struct AboutInfo {
    version: &'static str,
}

#[tauri::command]
async fn show_about(app: AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("about") {
        let _ = w.show();
        let _ = w.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(&app, "about", WebviewUrl::App("about.html".into()))
        .title("About Screen Cast")
        .inner_size(360.0, 470.0)
        .resizable(false)
        .minimizable(false)
        .devtools(false)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn about_info() -> AboutInfo {
    AboutInfo { version: env!("CARGO_PKG_VERSION") }
}

/// Opens the developer's website in Screen Cast's own sandboxed browser.
#[tauri::command]
fn open_developer_site(app: AppHandle) -> Result<(), String> {
    let url = Url::parse(DEVELOPER_SITE).map_err(|e| e.to_string())?;
    content_webview(&app)?.navigate(url).map_err(|e| e.to_string())?;
    if let Some(w) = app.get_webview_window("about") {
        let _ = w.close();
    }
    if let Some(w) = app.get_window("main") {
        let _ = w.set_focus();
    }
    Ok(())
}

#[tauri::command]
async fn show_qr(app: AppHandle) -> Result<(), String> {
    open_qr_window(&app)
}

#[tauri::command]
fn qr_info(app: AppHandle) -> Result<QrInfo, String> {
    let url = watch_link(&app).ok_or("Not connected to Wi-Fi / local network")?;
    let svg = qr_svg(&url).ok_or("Could not make QR code")?;
    let pin = app.state::<AppState>().server.pin();
    let shown = url.split('#').next().unwrap_or(&url).trim_end_matches('/').to_string();
    Ok(QrInfo { svg, url: shown, pin })
}

// ---------------------------------------------------------------------------------------------
// Window / webviews
// ---------------------------------------------------------------------------------------------

fn toolbar_height(app: &AppHandle) -> f64 {
    if app.state::<AppState>().page_fs.load(Ordering::SeqCst) {
        0.0
    } else {
        TOOLBAR_H
    }
}

/// Movie went fullscreen in the page (or left it): make the whole window fullscreen with the
/// toolbar hidden so both the Mac and the viewers see only the movie.
fn set_page_fullscreen(app: &AppHandle, on: bool) {
    let st = app.state::<AppState>();
    if st.page_fs.swap(on, Ordering::SeqCst) == on {
        return;
    }
    *st.page_fs_changed.lock().unwrap() = std::time::Instant::now();
    let casting = st.capture.lock().unwrap().is_some();
    if let Some(t) = app.get_webview("toolbar") {
        // When leaving fullscreen while casting, the toolbar (which shows the PIN) is only
        // revealed after the capture crop has been updated (see schedule_capture_restart),
        // so the PIN never appears in the stream.
        if on {
            let _ = t.hide();
        } else if !casting {
            let _ = t.show();
        }
    }
    if let Some(w) = app.get_window("main") {
        let _ = w.set_fullscreen(on);
        layout(&w);
    }
}

/// Watches the page's fullscreen state (read-only; the page gets no access to the app).
fn watch_page_fullscreen(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let Some(content) = app.get_webview("content") else { continue };
            let h = app.clone();
            let _ = content.eval_with_callback("document.fullscreenElement ? 1 : 0", move |r| {
                let page_on = r.trim() == "1";
                let st = h.state::<AppState>();
                let on = st.page_fs.load(Ordering::SeqCst);
                if !page_on {
                    st.page_fs_suppressed.store(false, Ordering::SeqCst);
                }
                if page_on && st.page_fs_suppressed.load(Ordering::SeqCst) {
                    return;
                }
                if page_on != on {
                    // Rate limit: at most one switch per second.
                    if st.page_fs_changed.lock().unwrap().elapsed() < std::time::Duration::from_secs(1) {
                        return;
                    }
                    set_page_fullscreen(&h, page_on);
                    return;
                }
                // The user left full screen with the window controls: leave it in the page too.
                let settled = st.page_fs_changed.lock().unwrap().elapsed() > std::time::Duration::from_millis(1500);
                if on && settled {
                    let fullscreen = h.get_window("main").and_then(|w| w.is_fullscreen().ok()).unwrap_or(true);
                    if !fullscreen {
                        st.page_fs_suppressed.store(true, Ordering::SeqCst);
                        if let Some(c) = h.get_webview("content") {
                            let _ = c.eval("document.exitFullscreen && document.exitFullscreen()");
                        }
                        set_page_fullscreen(&h, false);
                    }
                }
            });
        }
    });
}

fn layout(window: &tauri::Window) {
    let (Ok(scale), Ok(size)) = (window.scale_factor(), window.inner_size()) else { return };
    let s = size.to_logical::<f64>(scale);
    let app = window.app_handle();
    let bar = toolbar_height(app);
    if let Some(t) = app.get_webview("toolbar") {
        let _ = t.set_position(LogicalPosition::new(0.0, 0.0));
        let _ = t.set_size(LogicalSize::new(s.width, TOOLBAR_H));
    }
    if let Some(c) = app.get_webview("content") {
        let _ = c.set_position(LogicalPosition::new(0.0, bar));
        let _ = c.set_size(LogicalSize::new(s.width, (s.height - bar).max(1.0)));
    }
    schedule_capture_restart(app);
}

fn build_content_webview(app: &AppHandle, proxy_port: u16) -> WebviewBuilder<tauri::Wry> {
    let nav_app = app.clone();
    let popup_app = app.clone();
    let dl_app = app.clone();
    let load_app = app.clone();

    // SC_HOME: developer override of the start page (debug builds only).
    let home = if cfg!(debug_assertions) { std::env::var("SC_HOME").ok().and_then(|u| Url::parse(&u).ok()) } else { None }
        .unwrap_or_else(|| Url::parse(HOME).unwrap());
    WebviewBuilder::new("content", WebviewUrl::External(home))
        // Private mode: no cookies, cache or storage survive quitting the app.
        .incognito(true)
        .devtools(false)
        // All website traffic goes through the local filtering proxy (blocks LAN/localhost
        // destinations after DNS resolution).
        .proxy_url(Url::parse(&format!("http://127.0.0.1:{proxy_port}")).unwrap())
        .initialization_script_for_all_frames(include_str!("harden.js"))
        .on_navigation(move |url| navigation_allowed(&nav_app, url))
        .on_new_window(move |url, _features| {
            // Never open popups. A same-site link meant for a new tab opens in place instead.
            let st = popup_app.state::<AppState>();
            let current = st.url.lock().unwrap().clone();
            let same_site = Url::parse(&current)
                .ok()
                .and_then(|c| c.host_str().map(adblock::site_of))
                .zip(url.host_str().map(adblock::site_of))
                .is_some_and(|(a, b)| a == b);
            if same_site
                && matches!(url.scheme(), "http" | "https")
                && !st.blocker.should_block_navigation(url.as_str(), &current)
            {
                if let Some(c) = popup_app.get_webview("content") {
                    let _ = c.navigate(url.clone());
                }
            } else {
                note_blocked(&popup_app, &format!("popup {}", url.host_str().unwrap_or("")));
            }
            NewWindowResponse::Deny
        })
        .on_download(move |_wv, event| {
            if let DownloadEvent::Requested { url, .. } = event {
                note_blocked(&dl_app, &format!("download {}", url.host_str().unwrap_or("")));
            }
            false // downloads are never allowed
        })
        .on_page_load(move |_wv, payload| {
            if payload.event() == PageLoadEvent::Started || payload.event() == PageLoadEvent::Finished {
                let st = load_app.state::<AppState>();
                let new_url = payload.url().to_string();
                let changed = {
                    let mut u = st.url.lock().unwrap();
                    let changed = *u != new_url;
                    *u = new_url;
                    changed
                };
                if changed {
                    st.blocked.store(0, Ordering::Relaxed);
                }
                push_state(&load_app);
            }
        })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    #[cfg(target_os = "macos")]
    if args.get(1).map(String::as_str) == Some("--selftest") {
        let out = args.get(2).cloned().unwrap_or_else(|| "selftest.h264".into());
        let rt = tokio::runtime::Runtime::new().expect("runtime");
        match rt.block_on(selftest::run(out)) {
            Ok(()) => println!("SELFTEST OK"),
            Err(e) => {
                eprintln!("SELFTEST FAILED: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let _ = &args;

    #[cfg(target_os = "linux")]
    {
        // Window capture needs an X11 window; under Wayland this runs via XWayland.
        if std::env::var_os("GDK_BACKEND").is_none() {
            std::env::set_var("GDK_BACKEND", "x11");
        }
        // Run website processes inside WebKitGTK's bubblewrap sandbox when available.
        if std::env::var_os("WEBKIT_FORCE_SANDBOX").is_none() && std::path::Path::new("/usr/bin/bwrap").exists() {
            std::env::set_var("WEBKIT_FORCE_SANDBOX", "1");
        }
        // Avoids blank webviews with some GPU drivers (common WebKitGTK issue).
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }

    let hub = Hub::new();
    let server = ServerState::new(hub.clone(), server::PORT);

    tauri::Builder::default()
        .manage(AppState {
            blocker: Arc::new(Blocker::new()),
            server: server.clone(),
            hub: hub.clone(),
            capture: Mutex::new(None),
            url: Mutex::new(HOME.to_string()),
            blocked: AtomicU64::new(0),
            resize_gen: AtomicU64::new(0),
            page_fs: std::sync::atomic::AtomicBool::new(false),
            page_fs_changed: Mutex::new(std::time::Instant::now()),
            page_fs_suppressed: std::sync::atomic::AtomicBool::new(false),
        })
        .invoke_handler(tauri::generate_handler![
            get_state, navigate, go_back, go_forward, reload, toggle_cast, new_pin, show_qr, qr_info,
            show_about, about_info, open_developer_site
        ])
        .setup(move |app| {
            let handle = app.handle().clone();
            {
                let h = handle.clone();
                hub.set_on_change(Box::new(move || push_state(&h)));
                let h2 = handle.clone();
                let _ = server.on_pin_change.set(Box::new(move || push_state(&h2)));
            }
            tauri::async_runtime::spawn(server::run(server.clone()));

            let window = WindowBuilder::new(app, "main")
                .title(WINDOW_TITLE)
                .inner_size(1280.0, 800.0)
                .min_inner_size(720.0, 420.0);
            // macOS: toolbar sits in the title bar area next to the traffic lights.
            #[cfg(target_os = "macos")]
            let window = window.title_bar_style(tauri::TitleBarStyle::Overlay).hidden_title(true);
            let window = window.build()?;

            let w = 1280.0;
            let h = 800.0;
            window.add_child(
                WebviewBuilder::new("toolbar", WebviewUrl::App("index.html".into())).devtools(false),
                LogicalPosition::new(0.0, 0.0),
                LogicalSize::new(w, TOOLBAR_H),
            )?;
            let proxy_port = tauri::async_runtime::block_on(netguard::start())?;
            let content = window.add_child(
                build_content_webview(&handle, proxy_port),
                LogicalPosition::new(0.0, TOOLBAR_H),
                LogicalSize::new(w, h - TOOLBAR_H),
            )?;
            contentrules::install(&content, handle.state::<AppState>().blocker.clone());

            let win = window.clone();
            window.on_window_event(move |ev| {
                if matches!(ev, WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. }) {
                    layout(&win);
                }
            });
            layout(&window);
            watch_page_fullscreen(handle.clone());

            // Developer testing only: start casting automatically.
            if cfg!(debug_assertions) {
                if let Ok(js) = std::env::var("SC_TEST_JS") {
                    let h = handle.clone();
                    tauri::async_runtime::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
                        if let Some(c) = h.get_webview("content") {
                            let _ = c.eval(js);
                        }
                    });
                }
            }
            if cfg!(debug_assertions) && std::env::var_os("SC_AUTOCAST").is_some() {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    if let Err(e) = toggle_cast(h.clone()).await {
                        eprintln!("autocast failed: {e}");
                    }

                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Screen Cast");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_addresses_are_detected() {
        for u in ["http://localhost:8080/", "http://127.0.0.1/", "http://192.168.1.1/", "http://10.0.0.5/",
                  "http://172.20.1.1/", "http://nas.local/", "http://[::1]/", "http://0.0.0.0/"] {
            assert!(is_local_host(&Url::parse(u).unwrap()), "{u}");
        }
        assert!(!is_local_host(&Url::parse("https://example.com/").unwrap()));
    }

    #[test]
    fn address_bar_input() {
        assert_eq!(parse_input("example.com").unwrap().as_str(), "https://example.com/");
        assert_eq!(parse_input("http://a.com/x").unwrap().as_str(), "http://a.com/x");
        assert!(parse_input("best movies").unwrap().as_str().starts_with("https://duckduckgo.com/?q=best+movies"));
        // Dangerous schemes are never loaded as typed; they become a search.
        assert!(parse_input("file:///etc/passwd").unwrap().as_str().starts_with("https://duckduckgo.com/"));
        assert!(parse_input("javascript:alert(1)").unwrap().as_str().starts_with("https://duckduckgo.com/"));
    }
}

#[cfg(test)]
mod qr_tests {
    #[test]
    fn qr_svg_is_generated() {
        let text = std::env::var("SC_QR_TEXT").unwrap_or_else(|_| "http://192.168.1.20:8080/#pin=482913".into());
        let svg = super::qr_svg(&text).unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("<path d=\"M"));
        if let Ok(p) = std::env::var("SC_QR_OUT") {
            std::fs::write(p, svg).unwrap();
        }
    }
}
