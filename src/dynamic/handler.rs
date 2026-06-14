use axum::body::Body;
use axum::extract::Request;
use axum::http::{header::CONTENT_TYPE, Method, Response};
use serde_json::{json, Value};

use crate::dynamic::longpoll_registry;

use crate::route::longpull::GET_UPDATES_BODY_LIMIT;

pub async fn dynamic_handler(request: Request) -> Response<Body> {
    let (parts, body) = request.into_parts();

    let method = parts.method;
    let uri = parts.uri;
    let headers = parts.headers;
    let path = uri.path().to_string();

    if method != Method::POST {
        return error_response(json!({
            "ok": false,
            "error_code": 405,
            "description": "method not allowed"
        }));
    }

    let body_bytes = match axum::body::to_bytes(body, GET_UPDATES_BODY_LIMIT).await {
        Ok(b) => b,
        Err(_) => {
            return error_response(json!({
                "ok": false,
                "error_code": 413,
                "description": "request body too large"
            }))
        }
    };

    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if let Some(route) = longpoll_registry::get(&path) {
        return route.handle_get_updates(content_type, &body_bytes).await;
    }

    error_response(json!({
        "ok": false,
        "error_code": 404,
        "description": format!("Path {} not found in dynamic registry", path)
    }))
}

/// Telegram-shaped error envelope. The HTTP status is 200 (matches the
/// existing behaviour where the "error" lives inside the JSON body, not
/// in the HTTP status — this is what Telegram clients expect from the Bot
/// API).
fn error_response(value: Value) -> Response<Body> {
    let body = serde_json::to_vec(&value).expect("static json! never fails to serialize");
    Response::builder()
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("static headers and Bytes body always build a valid Response")
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::to_bytes;
    use axum::http::Request;
    use bytes::Bytes;
    use std::sync::Arc;

    use crate::base::Routeable;
    use crate::route::longpull::LongPollRoute;

    // The registry is process-global; these tests use distinct `__dynamic_test_*`
    // path keys and `remove` after so they don't interfere with each other or
    // with parallel tests.

    async fn envelope(response: Response<Body>) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn test_dynamic_handler_dispatches_json() {
        let path = "/__dynamic_test_json/getUpdates";
        let route = Arc::new(LongPollRoute::new(path.to_string()));
        route
            .process(Bytes::from(serde_json::to_vec(&json!({"update_id": 7})).unwrap()))
            .await;
        longpoll_registry::insert(path.to_string(), route);

        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"timeout":0,"limit":10}"#))
            .unwrap();

        let body = envelope(dynamic_handler(request).await).await;

        longpoll_registry::remove(path);

        let result = body["result"].as_array().unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["update_id"], 7);
    }

    #[tokio::test]
    async fn test_dynamic_handler_rejects_oversized_body() {
        let path = "/__dynamic_test_oversized/getUpdates";
        let big = vec![b'x'; GET_UPDATES_BODY_LIMIT + 1];

        let request = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(big))
            .unwrap();

        let body = envelope(dynamic_handler(request).await).await;

        assert_eq!(body["error_code"], 413);
    }

    #[tokio::test]
    async fn test_dynamic_handler_checks_method_before_body() {
        // An oversized GET: pre-reorder the body would be buffered first and
        // yield 413; post-reorder the method check returns 405 before any read.
        let path = "/__dynamic_test_method/getUpdates";
        let big = vec![b'x'; GET_UPDATES_BODY_LIMIT + 1];

        let request = Request::builder()
            .method("GET")
            .uri(path)
            .body(Body::from(big))
            .unwrap();

        let body = envelope(dynamic_handler(request).await).await;

        assert_eq!(body["error_code"], 405);
    }
}
