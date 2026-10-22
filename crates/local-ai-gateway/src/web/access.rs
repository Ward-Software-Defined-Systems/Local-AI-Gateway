//! The dashboard's gate: one access token (`token.rs`), carried once in the URL (`?token=...`), then by
//! a cookie. API and WebSocket requests may also present it in the query, because a WebSocket client
//! cannot follow a redirect and `curl` checks pass `?token=`. Anything else gets 401 with a notice that
//! never includes the token. Comparison is constant time. Ported from an earlier in-house web console gate.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use super::AppState;

/// The cookie the first visit sets.
pub const COOKIE: &str = "laig_token";
/// What a request without the token is told. Never the token.
pub const REFUSAL: &str = "Local AI Gateway dashboard: open the URL printed at startup, or run `local-ai-gateway --print-token`; it carries the access token.\n";

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// The request goes through.
    Pass,
    /// The entry hit: set the cookie and send the browser to the URL without the token.
    SetCookieAndRedirect {
        location: String,
        set_cookie: String,
    },
    /// 401 with the notice.
    Refuse,
}

/// A matching cookie passes. A matching query token passes an `/api/` or `/ws` request as it is and
/// sends any other request on to the clean URL with the cookie set. Anything else is refused.
pub fn decide(
    path: &str,
    query: Option<&str>,
    query_token: Option<&str>,
    cookie_token: Option<&str>,
    expected: &str,
    secure: bool,
) -> Decision {
    let ok = |candidate: Option<&str>| {
        candidate.is_some_and(|c| constant_time_eq(c.as_bytes(), expected.as_bytes()))
    };
    if ok(cookie_token) {
        return Decision::Pass;
    }
    if ok(query_token) {
        if path == "/ws" || path.starts_with("/api/") {
            return Decision::Pass;
        }
        return Decision::SetCookieAndRedirect {
            location: clean_location(path, query),
            set_cookie: cookie_value(expected, secure),
        };
    }
    Decision::Refuse
}

pub fn cookie_value(token: &str, secure: bool) -> String {
    let mut cookie = format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// Runs over the whole length whatever matches, so timing says nothing about where the first
/// difference is. Differing lengths are visible; the token's length is fixed anyway.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// `token=...` from the query, as typed.
fn query_token(uri: &Uri) -> Option<String> {
    uri.query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("token=").map(str::to_string))
}

/// `laig_token=...` from the `Cookie` headers.
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    headers.get_all(header::COOKIE).iter().find_map(|value| {
        value
            .to_str()
            .ok()?
            .split(';')
            .map(str::trim)
            .find_map(|c| c.strip_prefix(&format!("{COOKIE}=")).map(str::to_string))
    })
}

/// The same path and query without the `token` parameter.
fn clean_location(path: &str, query: Option<&str>) -> String {
    let rest: Vec<&str> = query
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("token="))
        .collect();
    if rest.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{}", rest.join("&"))
    }
}

/// The layer over every route and the fallback. With `dashboard.auth = false` (loopback only) it is a
/// pass-through.
pub async fn require_token(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if !st.auth_required() {
        return next.run(req).await;
    }
    let uri = req.uri().clone();
    let decision = decide(
        uri.path(),
        uri.query(),
        query_token(&uri).as_deref(),
        cookie_token(req.headers()).as_deref(),
        st.token(),
        st.secure_cookies(),
    );
    match decision {
        Decision::Pass => next.run(req).await,
        Decision::SetCookieAndRedirect {
            location,
            set_cookie,
        } => Response::builder()
            .status(StatusCode::SEE_OTHER)
            .header(header::LOCATION, location)
            .header(header::SET_COOKIE, set_cookie)
            .header(header::CACHE_CONTROL, "no-store")
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Decision::Refuse => (
            StatusCode::UNAUTHORIZED,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            REFUSAL,
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_request_without_a_token_is_refused() {
        assert_eq!(decide("/", None, None, None, T, false), Decision::Refuse);
        assert_eq!(decide("/ws", None, None, None, T, false), Decision::Refuse);
        assert_eq!(
            decide("/api/status", None, None, None, T, false),
            Decision::Refuse
        );
        assert_eq!(
            decide("/assets/app.css", None, None, None, T, false),
            Decision::Refuse
        );
    }

    #[test]
    fn a_matching_cookie_passes() {
        assert_eq!(decide("/", None, None, Some(T), T, false), Decision::Pass);
        assert_eq!(decide("/ws", None, None, Some(T), T, true), Decision::Pass);
    }

    #[test]
    fn a_matching_query_token_sets_the_cookie_and_redirects_to_the_clean_url() {
        let q = format!("token={T}");
        assert_eq!(
            decide("/", Some(&q), Some(T), None, T, false),
            Decision::SetCookieAndRedirect {
                location: "/".into(),
                set_cookie: format!("laig_token={T}; Path=/; HttpOnly; SameSite=Strict"),
            }
        );
        let q = format!("token={T}&tab=requests");
        let Decision::SetCookieAndRedirect {
            location,
            set_cookie,
        } = decide("/requests", Some(&q), Some(T), None, T, true)
        else {
            panic!("the entry hit redirects");
        };
        assert_eq!(
            location, "/requests?tab=requests",
            "the other parameters stay"
        );
        assert!(
            set_cookie.ends_with("; Secure"),
            "TLS adds Secure: {set_cookie}"
        );
    }

    #[test]
    fn a_query_token_on_the_socket_or_api_passes_without_a_redirect() {
        let q = format!("token={T}");
        assert_eq!(
            decide("/ws", Some(&q), Some(T), None, T, false),
            Decision::Pass
        );
        assert_eq!(
            decide("/api/status", Some(&q), Some(T), None, T, false),
            Decision::Pass
        );
        assert!(matches!(
            decide("/wsx", Some(&q), Some(T), None, T, false),
            Decision::SetCookieAndRedirect { .. }
        ));
    }

    #[test]
    fn a_wrong_token_is_refused_and_sets_no_cookie() {
        let wrong = "ffffffffffffffffffffffffffffffff";
        assert_eq!(
            decide("/", Some("token=ff"), Some("ff"), None, T, false),
            Decision::Refuse
        );
        assert_eq!(
            decide("/", None, None, Some(wrong), T, false),
            Decision::Refuse
        );
        assert_eq!(
            decide("/", None, None, Some(""), T, false),
            Decision::Refuse
        );
    }

    #[test]
    fn the_comparison_runs_over_the_whole_token() {
        assert!(constant_time_eq(T.as_bytes(), T.as_bytes()));
        let last_byte_off = format!("{}e", &T[..T.len() - 1]);
        assert!(!constant_time_eq(T.as_bytes(), last_byte_off.as_bytes()));
        assert!(!constant_time_eq(T.as_bytes(), &T.as_bytes()[..31]));
        assert!(!constant_time_eq(b"", T.as_bytes()));
    }

    #[test]
    fn the_cookie_is_read_from_the_header_and_the_token_from_the_query() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            format!("theme=dark; laig_token={T}; other=1")
                .parse()
                .unwrap(),
        );
        assert_eq!(cookie_token(&headers).as_deref(), Some(T));
        assert_eq!(cookie_token(&HeaderMap::new()), None);
        let uri: Uri = format!("/?x=1&token={T}").parse().unwrap();
        assert_eq!(query_token(&uri).as_deref(), Some(T));
        assert_eq!(query_token(&"/".parse().unwrap()), None);
    }

    #[test]
    fn the_refusal_points_at_print_token_and_never_contains_a_token() {
        assert!(REFUSAL.contains("--print-token"));
        assert!(!REFUSAL.contains(T));
    }
}
