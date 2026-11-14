//! Ad / tracker / malware blocking for the sandboxed browser.
//!
//! Two layers:
//! 1. `Blocker::should_block_navigation` — Brave's adblock engine checks every frame
//!    navigation (main page + iframes) before WebKit is allowed to load it.
//! 2. WebKit content rule lists (compiled from the same EasyList / EasyPrivacy / uBlock
//!    badware lists) block scripts, images, XHR, iframes and hide ad elements natively inside
//!    WebKit, plus our own rules that stop websites from talking to the local network.

use adblock::{lists::ParseOptions, request::Request, Engine, FilterSet};
use serde_json::{json, Value};

const LISTS: [&str; 3] = [
    include_str!("../filters/easylist.txt"),
    include_str!("../filters/easyprivacy.txt"),
    include_str!("../filters/ublock-badware.txt"),
];

/// Bump when the lists or extra rules change so WebKit recompiles them.
pub const RULES_VERSION: &str = "v1";
/// WebKit's per-list limit is 150k; stay well under it.
const RULES_PER_LIST: usize = 40_000;

pub struct Blocker {
    engine: Engine,
}

impl Blocker {
    pub fn new() -> Self {
        let mut set = FilterSet::new(false);
        for list in LISTS {
            set.add_filter_list(list.to_string(), ParseOptions::default());
        }
        Self { engine: Engine::new_with_filter_set(set) }
    }

    /// `true` if a sub-resource request (`script`, `image`, `xmlhttprequest`, ...) should be blocked.
    #[allow(dead_code)]
    pub fn should_block_request(&self, url: &str, source: &str, request_type: &str) -> bool {
        Request::new(url, source, request_type, "GET")
            .map(|r| self.engine.check_network_request(&r).should_block())
            .unwrap_or(false)
    }

    /// `true` if loading `url` (as a page or iframe inside `source`) should be blocked.
    pub fn should_block_navigation(&self, url: &str, source: &str) -> bool {
        ["document", "subdocument"].iter().any(|ty| {
            Request::new(url, source, ty, "GET")
                .map(|r| self.engine.check_network_request(&r).should_block())
                .unwrap_or(false)
        })
    }
}

/// `true` for URLs pointing at this computer or the local network (router, NAS, the cast
/// server itself...). Web pages are never allowed to reach those.
#[allow(dead_code)]
pub fn is_local_url(url: &str) -> bool {
    let Ok(u) = url::Url::parse(url) else { return false };
    if !matches!(u.scheme(), "http" | "https" | "ws" | "wss") {
        return false;
    }
    match u.host() {
        Some(url::Host::Ipv4(ip)) => crate::server::is_lan(ip.into()) || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => crate::server::is_lan(ip.into()) || ip.is_unspecified(),
        Some(url::Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            d == "localhost" || d.ends_with(".localhost") || d.ends_with(".local")
        }
        None => false,
    }
}

/// Rules that protect the computer and the LAN: web pages may not reach localhost, private
/// networks (routers, NAS, this app's own cast server) or mDNS hosts.
fn local_network_rules() -> Vec<Value> {
    [
        r"^[a-z]+://localhost[:/]",
        r"^[a-z]+://127\.",
        r"^[a-z]+://0\.0\.0\.0",
        r"^[a-z]+://10\.",
        r"^[a-z]+://192\.168\.",
        r"^[a-z]+://169\.254\.",
        r"^[a-z]+://172\.1[6-9]\.",
        r"^[a-z]+://172\.2[0-9]\.",
        r"^[a-z]+://172\.3[01]\.",
        r"^[a-z]+://\[",
        r"^[a-z]+://[^/:]+\.local[:/]",
        r"^[a-z]+://[^/:]+\.local$",
    ]
    .iter()
    .map(|f| json!({ "trigger": { "url-filter": f }, "action": { "type": "block" } }))
    .collect()
}

/// Converts the filter lists into WebKit content-blocker JSON, split into chunks.
/// This takes a few seconds, so it is only run when WebKit has no compiled copy cached.
pub fn content_rule_chunks() -> Vec<String> {
    let mut set = FilterSet::new(true);
    for list in LISTS {
        set.add_filter_list(list.to_string(), ParseOptions::default());
    }
    let rules: Vec<Value> = match set.into_content_blocking() {
        Ok((rules, _)) => rules
            .into_iter()
            .filter_map(|r| serde_json::to_value(r).ok())
            // WebKit rejects the whole list if any rule contains non-ASCII text.
            .filter(|v| v.to_string().is_ascii())
            .collect(),
        Err(_) => Vec::new(),
    };

    let mut chunks: Vec<String> = rules
        .chunks(RULES_PER_LIST)
        .map(|c| serde_json::to_string(c).unwrap_or_else(|_| "[]".into()))
        .collect();
    // Local-network protection goes last so ad-list "ignore-previous-rules" exceptions
    // can never cancel it.
    chunks.push(serde_json::to_string(&local_network_rules()).unwrap());
    chunks
}

/// Registrable-ish domain used for popup same-site checks ("www.example.com.np" ->
/// "example.com.np"). Deliberately simple; only used to decide whether to *allow*.
pub fn site_of(host: &str) -> String {
    let labels: Vec<&str> = host.trim_end_matches('.').split('.').collect();
    let n = labels.len();
    if n <= 2 {
        return host.to_string();
    }
    let second = labels[n - 2];
    let take = if labels[n - 1].len() == 2
        && matches!(second, "co" | "com" | "net" | "org" | "gov" | "edu" | "ac" | "or" | "ne")
    {
        3
    } else {
        2
    };
    labels[n.saturating_sub(take)..].join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_ad_and_malware_navigations() {
        let b = Blocker::new();
        let page = "https://example-movies.com/watch/1";
        assert!(b.should_block_navigation("https://securepubads.g.doubleclick.net/gampad/ads", page));
        assert!(b.should_block_navigation("https://www.googletagmanager.com/gtm.js?id=X", page));
        assert!(!b.should_block_navigation("https://example-movies.com/watch/2", page));
        assert!(!b.should_block_navigation("https://www.youtube.com/embed/abc", page));
    }

    #[test]
    fn content_rules_are_valid_json() {
        let chunks = content_rule_chunks();
        let mut total = 0;
        for c in &chunks {
            let v: Vec<Value> = serde_json::from_str(c).unwrap();
            assert!(v.len() <= RULES_PER_LIST);
            total += v.len();
        }
        assert!(total > 10_000, "only {total} rules");
        assert!(chunks.last().unwrap().contains("192\\\\.168"));
    }

    #[test]
    fn site_of_handles_country_domains() {
        assert_eq!(site_of("www.example.com"), "example.com");
        assert_eq!(site_of("a.b.example.com.np"), "example.com.np");
        assert_eq!(site_of("player.site.co.uk"), "site.co.uk");
    }
}
