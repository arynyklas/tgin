//! Shared authentication primitives for the control / observability plane.
//!
//! `secret_eq` is the single constant-time comparison used by every secret
//! check in the process — the webhook ingress secret-token check and the
//! optional control-plane bearer gate both route through it, so the
//! timing-safe comparison lives in exactly one place.
//!
//! `guard` wraps a router with an optional `Authorization: Bearer <token>`
//! gate. `None` leaves the router open, preserving the default no-auth posture
//! and Prometheus scrape ergonomics; `Some(token)` rejects with `401` every
//! request that does not present the exact bearer credential, before the inner
//! handler runs.

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::Router;

use subtle::ConstantTimeEq;

/// Constant-time equality of a presented secret against the expected value.
/// Compares same-length, zero-padded buffers so a length mismatch still runs
/// the full `ct_eq` and cannot be distinguished by timing; the trailing
/// length check rejects mismatches.
pub fn secret_eq(provided: &[u8], expected: &[u8]) -> bool {
    let len_eq = provided.len() == expected.len();
    let mut padded = vec![0u8; expected.len()];
    let copy_len = provided.len().min(expected.len());
    padded[..copy_len].copy_from_slice(&provided[..copy_len]);
    let bytes_eq: bool = padded.ct_eq(expected).into();
    len_eq && bytes_eq
}

/// True when `headers` carries an `Authorization: Bearer <token>` whose token
/// matches `expected` in constant time. A missing header, a non-`Bearer`
/// scheme, or a non-ASCII value all read as the empty credential and fail the
/// comparison (`expected` is non-empty — config validation rejects an empty
/// `auth_token`).
pub fn bearer_authorized(headers: &HeaderMap, expected: &str) -> bool {
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    secret_eq(provided.as_bytes(), expected.as_bytes())
}

/// Wrap `router` with an optional bearer gate. `None` returns it untouched
/// (open); `Some(token)` layers a middleware that rejects any request lacking
/// the exact bearer credential with `401 Unauthorized` ahead of the inner
/// handler. Generic over the router's state `S` so the same gate composes onto
/// the `/api` sub-router and the `/status`+`/metrics` sub-router alike.
pub fn guard<S>(router: Router<S>, token: Option<String>) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let Some(token) = token else {
        return router;
    };
    router.layer(middleware::from_fn(
        move |req: Request<Body>, next: Next| {
            let token = token.clone();
            async move {
                if bearer_authorized(req.headers(), &token) {
                    next.run(req).await
                } else {
                    StatusCode::UNAUTHORIZED.into_response()
                }
            }
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt;

    #[test]
    fn secret_eq_matches_only_exact() {
        assert!(secret_eq(b"hunter2", b"hunter2"));
        assert!(!secret_eq(b"hunter2", b"hunter3"));
        // Length-mismatch cases must still fail (and exercise the padded path).
        assert!(!secret_eq(b"hunter", b"hunter2"), "shorter");
        assert!(!secret_eq(b"hunter234", b"hunter2"), "longer");
        assert!(!secret_eq(b"", b"hunter2"), "empty provided");
    }

    fn headers_with(value: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = value {
            h.insert(header::AUTHORIZATION, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn bearer_authorized_requires_exact_scheme_and_token() {
        assert!(bearer_authorized(&headers_with(Some("Bearer tok")), "tok"));
        assert!(!bearer_authorized(&headers_with(Some("Bearer wrong")), "tok"));
        // Missing scheme, wrong-case scheme, and absent header all fail.
        assert!(!bearer_authorized(&headers_with(Some("tok")), "tok"));
        assert!(!bearer_authorized(&headers_with(Some("bearer tok")), "tok"));
        assert!(!bearer_authorized(&headers_with(None), "tok"));
    }

    fn ok_router() -> Router {
        Router::new().route("/x", get(|| async { "ok" }))
    }

    #[tokio::test]
    async fn guard_none_is_open() {
        let app = guard(ok_router(), None);
        let res = app
            .oneshot(Request::get("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn guard_some_rejects_missing_and_wrong_admits_correct() {
        let make = || guard(ok_router(), Some("tok".to_string()));

        let res = make()
            .oneshot(Request::get("/x").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "missing bearer");

        let res = make()
            .oneshot(
                Request::get("/x")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "wrong bearer");

        let res = make()
            .oneshot(
                Request::get("/x")
                    .header("authorization", "Bearer tok")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "correct bearer");
    }
}
