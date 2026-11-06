//! The console's static files, compiled into the binary by `build.rs`, so an install gets it with
//! no extra pod. It's a single-page app: any path that isn't a file gets `index.html`, and the app
//! routes from there.

use axum::http::{HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};

include!(concat!(env!("OUT_DIR"), "/console_assets.rs"));

/// Radix sets inline styles, so styles need `unsafe-inline`. Scripts don't.
const POLICY: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
                      img-src 'self' data:; font-src 'self'; connect-src 'self'; \
                      base-uri 'none'; form-action 'self'; frame-ancestors 'none'";
const NOT_BUILT: &str = "The console wasn't built into this binary. Run `npm ci && npm run build` \
                         in console/, then build mmx again.\n";

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("woff2") => "font/woff2",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn find(path: &str) -> Option<(&'static str, &'static [u8])> {
    ASSETS
        .iter()
        .find(|(served, _)| *served == path)
        .map(|(served, bytes)| (*served, *bytes))
}

fn file(path: &str, bytes: &'static [u8]) -> Response {
    // Vite puts a hash in every name under `assets/`, so those can be kept forever. `index.html`
    // has to be fetched each time, or an upgrade wouldn't reach open browsers.
    let cache = if path.starts_with("/assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(path)),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

pub async fn serve(method: Method, uri: Uri) -> Response {
    let path = uri.path();
    if method != Method::GET && method != Method::HEAD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    // A mistyped API path or missing asset is a real 404, not a page that loads as if it worked.
    if path.starts_with("/admin/") || path.starts_with("/assets/") {
        return match find(path) {
            Some((served, bytes)) => file(served, bytes),
            None => StatusCode::NOT_FOUND.into_response(),
        };
    }
    let found = if path == "/" { None } else { find(path) };
    match found.or_else(|| find("/index.html")) {
        Some((served, bytes)) => file(served, bytes),
        None => (StatusCode::SERVICE_UNAVAILABLE, NOT_BUILT).into_response(),
    }
}
