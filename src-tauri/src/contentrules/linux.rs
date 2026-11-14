//! WebKitGTK: compile the same content-blocker rules as macOS into user content filters.

use std::ffi::{c_void, CString};

use gtk::glib::translate::ToGlibPtr;
use webkit2gtk::WebViewExt;
use webkit2gtk_sys as wk;

use crate::adblock::{content_rule_chunks, RULES_VERSION};

unsafe extern "C" fn saved(source: *mut gobject_sys::GObject, result: *mut gio_sys::GAsyncResult, data: glib_sys::gpointer) {
    let mut err: *mut glib_sys::GError = std::ptr::null_mut();
    let filter = wk::webkit_user_content_filter_store_save_finish(source as *mut _, result, &mut err);
    let manager = data as *mut wk::WebKitUserContentManager;
    if !filter.is_null() {
        wk::webkit_user_content_manager_add_filter(manager, filter);
        wk::webkit_user_content_filter_unref(filter);
    } else if !err.is_null() {
        eprintln!("content filter failed: {:?}", std::ffi::CStr::from_ptr((*err).message));
        glib_sys::g_error_free(err);
    }
    gobject_sys::g_object_unref(manager as *mut _);
}

pub fn install(webview: &tauri::Webview) {
    let wv = webview.clone();
    std::thread::spawn(move || {
        let chunks = content_rule_chunks();
        let dir = std::env::temp_dir().join(format!("screen-cast-filters-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = wv.with_webview(move |pv| unsafe {
            let view = pv.inner();
            let Some(manager) = view.user_content_manager() else { return };
            let manager: *mut wk::WebKitUserContentManager = manager.to_glib_none().0;
            let Ok(path) = CString::new(dir.to_string_lossy().as_bytes()) else { return };
            // Lives for the rest of the process (callbacks may arrive later).
            let store = wk::webkit_user_content_filter_store_new(path.as_ptr());
            for (i, json) in chunks.iter().enumerate() {
                let Ok(id) = CString::new(format!("screencast-{RULES_VERSION}-{i}")) else { continue };
                let bytes = glib_sys::g_bytes_new(json.as_ptr() as *const c_void, json.len());
                gobject_sys::g_object_ref(manager as *mut _);
                wk::webkit_user_content_filter_store_save(
                    store,
                    id.as_ptr(),
                    bytes,
                    std::ptr::null_mut(),
                    Some(saved),
                    manager as glib_sys::gpointer,
                );
                glib_sys::g_bytes_unref(bytes);
            }
        });
    });
}
