//! Installs native WebKit content-blocking rules (ads, trackers, malware, local-network
//! access) into the sandboxed browser. Compiled once, then cached by WebKit.

use block2::RcBlock;
use objc2::rc::Retained;
use objc2_foundation::{MainThreadMarker, NSError, NSString};
use objc2_web_kit::{WKContentRuleList, WKContentRuleListStore, WKUserContentController};

use crate::adblock::{content_rule_chunks, RULES_VERSION};

fn rule_id(i: usize) -> Retained<NSString> {
    NSString::from_str(&format!("screencast-{RULES_VERSION}-{i}"))
}

pub fn install(webview: &tauri::Webview) {
    let wv = webview.clone();
    let _ = webview.with_webview(move |pv| unsafe {
        let Some(ctrl) = Retained::retain(pv.controller() as *mut WKUserContentController) else {
            return;
        };
        let Some(mtm) = MainThreadMarker::new() else { return };
        let Some(store) = WKContentRuleListStore::defaultStore(mtm) else { return };
        load_cached(store, ctrl, 0, wv);
    });
}

/// Add cached lists screencast-vN-0, -1, ... until one is missing.
unsafe fn load_cached(
    store: Retained<WKContentRuleListStore>,
    ctrl: Retained<WKUserContentController>,
    i: usize,
    wv: tauri::Webview,
) {
    let next_store = store.clone();
    let block = RcBlock::new(move |list: *mut WKContentRuleList, _err: *mut NSError| {
        if let Some(list) = list.as_ref() {
            ctrl.addContentRuleList(list);
            load_cached(next_store.clone(), ctrl.clone(), i + 1, wv.clone());
        } else if i == 0 {
            compile_all(wv.clone());
        }
    });
    store.lookUpContentRuleListForIdentifier_completionHandler(Some(&rule_id(i)), Some(&block));
}

/// First run (or new rules version): convert the filter lists off the main thread, then
/// compile and install them.
fn compile_all(wv: tauri::Webview) {
    std::thread::spawn(move || {
        let chunks = content_rule_chunks();
        let wv2 = wv.clone();
        let _ = wv.with_webview(move |pv| unsafe {
            let Some(ctrl) = Retained::retain(pv.controller() as *mut WKUserContentController) else {
                return;
            };
            let Some(mtm) = MainThreadMarker::new() else { return };
            let Some(store) = WKContentRuleListStore::defaultStore(mtm) else { return };
            let total = chunks.len();
            for (i, json) in chunks.into_iter().enumerate() {
                let ctrl = ctrl.clone();
                let wv = wv2.clone();
                let block = RcBlock::new(move |list: *mut WKContentRuleList, err: *mut NSError| {
                    if let Some(list) = list.as_ref() {
                        ctrl.addContentRuleList(list);
                    } else if let Some(err) = err.as_ref() {
                        eprintln!("content rules {i} failed: {}", err.localizedDescription());
                    }
                    if i + 1 == total {
                        let _ = wv.reload(); // apply to the page that loaded before rules were ready
                    }
                });
                store.compileContentRuleListForIdentifier_encodedContentRuleList_completionHandler(
                    Some(&rule_id(i)),
                    Some(&NSString::from_str(&json)),
                    Some(&block),
                );
            }
        });
    });
}
