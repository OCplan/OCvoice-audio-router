use axum::http::header;
use axum::response::{IntoResponse, Response};

fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::X_FRAME_OPTIONS, "DENY"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY,
             "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
        ],
        body,
    ).into_response()
}

pub(crate) async fn page() -> Response {
    asset("text/html; charset=utf-8", include_str!("../resources/setup/index.html"))
}

pub(crate) async fn script() -> Response {
    asset("text/javascript; charset=utf-8", include_str!("../resources/setup/setup.js"))
}

pub(crate) async fn style() -> Response {
    asset("text/css; charset=utf-8", include_str!("../resources/setup/setup.css"))
}
