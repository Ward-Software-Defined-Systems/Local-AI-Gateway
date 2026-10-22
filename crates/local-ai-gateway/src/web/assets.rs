//! The embedded dashboard. `build.rs` stages `frontend/dist` (or a placeholder page) into `$OUT_DIR/ui`
//! and rust-embed compiles it in, so the binary carries its own UI. Any path that is not an API route or
//! an existing file gets `index.html`, which is how the single-page app's client-side routes deep-link.
//! Hashed Vite assets under `assets/` are immutable; everything else is `no-store` so a rebuilt binary
//! never serves a stale page.

use std::borrow::Cow;

use axum::body::Body;
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::{EmbeddedFile, RustEmbed};
use serde_json::json;

#[derive(RustEmbed)]
#[folder = "$OUT_DIR/ui"]
struct Assets;

#[derive(Debug, PartialEq, Eq)]
pub enum Route<'a> {
    /// The single-page app.
    App,
    /// An embedded file at this path, falling back to the app when it does not exist.
    File(&'a str),
    /// An API path nobody registered: a JSON 404, never the app's HTML.
    ApiNotFound,
}

pub fn route(path: &str) -> Route<'_> {
    let path = path.trim_start_matches('/');
    if path.is_empty() || path == "index.html" {
        Route::App
    } else if path == "api" || path.starts_with("api/") {
        Route::ApiNotFound
    } else {
        Route::File(path)
    }
}

pub async fn static_handler(uri: Uri) -> Response {
    match route(uri.path()) {
        Route::App => index(),
        Route::ApiNotFound => (
            StatusCode::NOT_FOUND,
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(json!({"error": {"code": "not_found", "message": "no such API route"}})),
        )
            .into_response(),
        Route::File(path) => match Assets::get(path) {
            Some(file) => serve(path, file),
            None => index(),
        },
    }
}

fn index() -> Response {
    match Assets::get("index.html") {
        Some(file) => serve("index.html", file),
        None => (StatusCode::NOT_FOUND, "dashboard not embedded").into_response(),
    }
}

fn serve(path: &str, file: EmbeddedFile) -> Response {
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    let body = match file.data {
        Cow::Borrowed(bytes) => Body::from(bytes),
        Cow::Owned(bytes) => Body::from(bytes),
    };
    (
        [
            (header::CONTENT_TYPE, content_type(path)),
            (header::CACHE_CONTROL, cache),
        ],
        body,
    )
        .into_response()
}

pub fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("txt") => "text/plain; charset=utf-8",
        Some("webmanifest") => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_route_to_the_app_a_file_or_a_json_404() {
        assert_eq!(route("/"), Route::App);
        assert_eq!(route("/index.html"), Route::App);
        assert_eq!(
            route("/requests/abc"),
            Route::File("requests/abc"),
            "unknown files fall back to the app at serve time"
        );
        assert_eq!(
            route("/assets/index-abc123.js"),
            Route::File("assets/index-abc123.js")
        );
        assert_eq!(route("/api/nope"), Route::ApiNotFound);
        assert_eq!(route("/api"), Route::ApiNotFound);
        assert_eq!(route("/apix"), Route::File("apix"));
    }

    #[test]
    fn content_types_cover_what_vite_emits() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type("assets/index-abc.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(
            content_type("assets/index-abc.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(content_type("favicon.svg"), "image/svg+xml");
        assert_eq!(content_type("font.woff2"), "font/woff2");
        assert_eq!(content_type("blob"), "application/octet-stream");
    }

    #[test]
    fn the_embedded_index_exists() {
        assert!(
            Assets::get("index.html").is_some(),
            "build.rs stages index.html"
        );
    }
}
