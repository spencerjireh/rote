//! The browser front end, compiled in.
//!
//! Served by the daemon itself at `/`, same-origin, which is the whole reason
//! there are no CORS headers anywhere in `daemon.rs`: a page served from
//! `http://127.0.0.1:<port>` passes `is_loopback_origin` for free, and a page
//! served from anywhere else has no business talking to a server that hands out
//! your source code.
//!
//! One self-contained file, `include_str!`'d. No build step and no asset
//! pipeline: a build script to concatenate three files would be a build system
//! for forty kilobytes of text, and `include_str!` already recompiles on
//! change.

use tiny_http::{Header, Response, ResponseBox};

pub const INDEX_HTML: &str = include_str!("web/index.html");

/// What the page is allowed to do.
///
/// `'unsafe-inline'` is forced by a self-contained page, so this buys nothing
/// against injected script — the actual defence is that the page sets text with
/// `textContent` and never touches `innerHTML`, which `the_page_never_assigns_inner_html`
/// checks mechanically. What it does buy is that *if* an injection ever landed,
/// `default-src 'none'` and `connect-src 'self'` mean it could neither load
/// anything nor send anything anywhere. This page renders the agent's source
/// code into the DOM, which makes it the most plausible injection surface in
/// the whole tool.
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; \
                   style-src 'unsafe-inline'; img-src data:; connect-src 'self'";

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header")
}

pub fn page() -> ResponseBox {
    Response::from_data(INDEX_HTML.as_bytes().to_vec())
        // tiny_http switches to `Transfer-Encoding: chunked` past 32 KiB. The
        // page is under that today, so this is not load-bearing yet — it is
        // here so that growing the page cannot silently break the invariant
        // that the daemon chunk-encodes nothing but the event stream, which the
        // hand-rolled client refuses chunked outright on the strength of.
        .with_chunked_threshold(usize::MAX)
        .with_header(header("Content-Type", "text/html; charset=utf-8"))
        .with_header(header("Cache-Control", "no-store"))
        // The token is in the URL, so this is what stops it leaking through a
        // `Referer` if the page ever grows an off-origin reference.
        .with_header(header("Referrer-Policy", "no-referrer"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Content-Security-Policy", CSP))
        .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_never_assigns_inner_html() {
        // The XSS guard, as a test rather than as a comment. The page renders
        // the agent's source code, so this is the line that matters most.
        assert!(
            !INDEX_HTML.contains("innerHTML"),
            "the page must set text with textContent"
        );
        assert!(!INDEX_HTML.contains("outerHTML"));
        assert!(!INDEX_HTML.contains("insertAdjacentHTML"));
        assert!(!INDEX_HTML.contains("document.write"));
    }

    #[test]
    fn the_page_loads_nothing_from_another_origin() {
        // It has to be self-contained: the CSP forbids it, and the daemon
        // serves exactly one file.
        for needle in [
            "https://",
            "http://cdn",
            "<script src",
            "@import",
            "<link rel=\"style",
        ] {
            assert!(
                !INDEX_HTML.contains(needle),
                "the page must be self-contained, found {needle}"
            );
        }
    }

    #[test]
    fn the_page_does_not_decide_terminality_from_ready_state() {
        // `EventSource` retries on its own and stays in CONNECTING while it does,
        // so a daemon that restarted on a new port never reaches CLOSED. Reading
        // `readyState` to tell "gone" from "blip" therefore said "blip" forever,
        // against a dead address — the failure DESIGN §9.20 exists to prevent.
        assert!(
            !INDEX_HTML.contains("EventSource.CLOSED"),
            "terminality comes from asking the daemon, not from readyState"
        );
    }

    #[test]
    fn the_page_re_probes_health_before_reconnecting() {
        // `handshake` checks the status and the wire version, which is what turns
        // a new token into a 401 and a new port into a network failure — both
        // terminal. Reconnecting without it is what makes the loop unbounded.
        let handler = INDEX_HTML
            .split("addEventListener(\"error\"")
            .nth(1)
            .expect("an error handler on the stream");
        assert!(
            handler.contains("handshake()"),
            "the error path must re-probe /health: {handler:.400}"
        );
        assert!(
            handler.contains("stream.close()"),
            "and must close rather than let the browser retry: {handler:.400}"
        );
        assert!(
            INDEX_HTML.contains("RECONNECT_LIMIT"),
            "a daemon that answers /health but keeps dropping /events is bounded too"
        );
    }

    #[test]
    fn the_page_pins_the_wire_version_the_daemon_speaks() {
        assert!(
            INDEX_HTML.contains(&format!(
                "const WIRE_VERSION = {}",
                crate::state::WIRE_VERSION
            )),
            "the page must pin wire version {}",
            crate::state::WIRE_VERSION
        );
    }
}
