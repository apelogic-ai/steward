use axum::extract::Request;
use axum::http::{HeaderName, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::Serialize;

use crate::AdminContext;

const ADMIN_API_VERSION: &str = "steward.admin/v1";
const ADMIN_HTML: &str = include_str!("../assets/admin/index.html");
const ADMIN_CSS: &str = include_str!("../assets/admin/admin.css");
const ADMIN_JS: &str = include_str!("../assets/admin/admin.js");
const ADMIN_ICON: &str = include_str!("../assets/admin/icon.svg");
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; base-uri 'none'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; img-src 'self'; object-src 'none'; script-src 'self'; style-src 'self'";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AdminSurface {
    Approvals,
    Envelope,
    Fleet,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AdminBootstrapResponse {
    api_version: &'static str,
    actor: String,
    surfaces: [AdminSurface; 3],
}

pub(crate) fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let browser_api = Router::<S>::new()
        .route("/admin/api/v1/bootstrap", get(bootstrap))
        .route_layer(middleware::from_fn(enforce_browser_mutation_boundary));
    Router::<S>::new()
        .route("/admin", get(shell))
        .route("/admin/", get(shell))
        .merge(asset_router())
        .merge(browser_api)
}

pub(crate) fn asset_router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::<S>::new()
        .route("/admin/assets/admin.css", get(stylesheet))
        .route("/admin/assets/admin.js", get(script))
        .route("/admin/assets/icon.svg", get(icon))
}

async fn shell() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

async fn stylesheet() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        ADMIN_CSS,
    )
        .into_response()
}

async fn script() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        ADMIN_JS,
    )
        .into_response()
}

async fn icon() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/svg+xml")],
        ADMIN_ICON,
    )
        .into_response()
}

#[utoipa::path(
    get,
    path = "/admin/api/v1/bootstrap",
    security(("adminBearer" = [])),
    responses(
        (status = 200, description = "Authenticated administrator UI contract", body = AdminBootstrapResponse),
        (status = 401, description = "Missing or invalid authentication"),
        (status = 403, description = "Administrator authority required")
    )
)]
pub(crate) async fn bootstrap(
    Extension(admin): Extension<AdminContext>,
) -> Json<AdminBootstrapResponse> {
    Json(AdminBootstrapResponse {
        api_version: ADMIN_API_VERSION,
        actor: admin.actor,
        surfaces: [
            AdminSurface::Approvals,
            AdminSurface::Envelope,
            AdminSurface::Fleet,
        ],
    })
}

pub(crate) async fn add_browser_security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), geolocation=(), microphone=(), payment=(), usb=()"),
    );
    response
}

pub(crate) async fn enforce_browser_mutation_boundary(request: Request, next: Next) -> Response {
    if !matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    ) {
        return next.run(request).await;
    }

    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    let same_origin = host
        .map(|host| format!("https://{host}"))
        .is_some_and(|expected| origin == Some(expected.as_str()));
    let same_origin_fetch = headers
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        == Some("same-origin");
    let explicit_csrf = headers
        .get("x-steward-csrf")
        .and_then(|value| value.to_str().ok())
        == Some("1");
    let json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"));
    if !(same_origin && same_origin_fetch && explicit_csrf && json) {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "browser mutation boundary rejected the request",
            })),
        )
            .into_response();
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::middleware;
    use axum::routing::{get, post};
    use tower::ServiceExt;

    use super::enforce_browser_mutation_boundary;

    fn guarded_probe() -> Router {
        Router::new()
            .route("/probe", get(|| async { StatusCode::NO_CONTENT }))
            .route("/probe", post(|| async { StatusCode::NO_CONTENT }))
            .route_layer(middleware::from_fn(enforce_browser_mutation_boundary))
    }

    async fn request(method: Method, headers: &[(&str, &str)]) -> Result<StatusCode, String> {
        let mut request = Request::builder().method(method).uri("/probe");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        guarded_probe()
            .oneshot(
                request
                    .body(Body::empty())
                    .map_err(|error| format!("build browser mutation request: {error}"))?,
            )
            .await
            .map(|response| response.status())
            .map_err(|error| format!("execute browser mutation request: {error}"))
    }

    #[tokio::test]
    async fn browser_mutations_require_exact_same_origin_fetch_and_csrf_headers()
    -> Result<(), String> {
        let valid_headers = [
            ("host", "steward.test"),
            ("origin", "https://steward.test"),
            ("sec-fetch-site", "same-origin"),
            ("x-steward-csrf", "1"),
            ("content-type", "application/json"),
        ];
        assert_eq!(
            request(Method::POST, &valid_headers).await?,
            StatusCode::NO_CONTENT,
            "a same-origin JSON mutation with the explicit CSRF header must proceed"
        );

        for (case, headers) in [
            (
                "missing origin",
                vec![
                    ("host", "steward.test"),
                    ("sec-fetch-site", "same-origin"),
                    ("x-steward-csrf", "1"),
                    ("content-type", "application/json"),
                ],
            ),
            (
                "cross origin",
                vec![
                    ("host", "steward.test"),
                    ("origin", "https://other.test"),
                    ("sec-fetch-site", "cross-site"),
                    ("x-steward-csrf", "1"),
                    ("content-type", "application/json"),
                ],
            ),
            (
                "missing CSRF header",
                vec![
                    ("host", "steward.test"),
                    ("origin", "https://steward.test"),
                    ("sec-fetch-site", "same-origin"),
                    ("content-type", "application/json"),
                ],
            ),
            (
                "form content type",
                vec![
                    ("host", "steward.test"),
                    ("origin", "https://steward.test"),
                    ("sec-fetch-site", "same-origin"),
                    ("x-steward-csrf", "1"),
                    ("content-type", "application/x-www-form-urlencoded"),
                ],
            ),
        ] {
            assert_eq!(
                request(Method::POST, &headers).await?,
                StatusCode::FORBIDDEN,
                "{case} must fail before a browser mutation handler runs"
            );
        }
        assert_eq!(
            request(Method::GET, &[]).await?,
            StatusCode::NO_CONTENT,
            "read-only browser API requests do not need mutation headers"
        );
        Ok(())
    }
}
