//! The one URL opener: every click a kuma app hands to the desktop
//! goes through here, and the scheme allowlist is the point. Whatever
//! a program printed, a file span looked like, or a config carried,
//! only a web address leaves: a `file://` is link-shaped text, not a
//! license to hand the opener a local path (a .desktop file would
//! execute).
//!
//! The launch itself is gpui's (`open_uri`): the desktop portal
//! first, the activation token so the browser does not steal focus
//! badly, xdg-open behind it.

use gpui::App;

/// Whether the opener will open this: a web address and nothing else.
/// A separate predicate so the gate is testable without a platform.
pub fn is_web_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Open a URL through the desktop handler, gated. Anything that is
/// not http or https is refused with a log line, never opened.
pub fn url(url: &str, cx: &App) {
    if !is_web_url(url) {
        log::info!("not opening {url}: only http and https leave a kuma app");
        return;
    }
    cx.open_url(url);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_is_web_addresses_only() {
        assert!(is_web_url("https://example.com"));
        assert!(is_web_url("http://example.com/x?y=1"));
        for refused in [
            "file:///etc/passwd",
            "ftp://example.com",
            "javascript:alert(1)",
            "/etc/passwd",
            "",
        ] {
            assert!(!is_web_url(refused), "{refused} must be refused");
        }
    }
}
