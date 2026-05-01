use axum::body::Body;
use axum::extract::Request;
use axum::http::{header::CONTENT_TYPE, Method, Response};
use serde_json::{json, Value};

use crate::dynamic::longpoll_registry;

use crate::route::longpull::GetUpdatesParams;

pub async fn dynamic_handler(request: Request) -> Response<Body> {
    let (parts, body) = request.into_parts();

    let method = parts.method;
    let uri = parts.uri;
    let headers = parts.headers;
    let path = uri.path().to_string();

    let body_bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => {
            return error_response(json!({
                "ok": false,
                "error_code": 400,
                "description": "failed to read request body"
            }))
        }
    };

    if method != Method::POST {
        return error_response(json!({
            "ok": false,
            "error_code": 405,
            "description": "method not allowed"
        }));
    }

    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let params: GetUpdatesParams = if content_type.contains("application/json") {
        match serde_json::from_slice(&body_bytes) {
            Ok(p) => p,
            Err(_) => {
                return error_response(json!({
                    "ok": false,
                    "error_code": 400,
                    "description": "invalid json body"
                }))
            }
        }
    } else {
        match serde_urlencoded::from_bytes(&body_bytes) {
            Ok(p) => p,
            Err(_) => GetUpdatesParams {
                offset: None,
                timeout: None,
                limit: None,
            },
        }
    };

    let route = longpoll_registry::get(&path);

    if let Some(route) = route {
        return route.handle_request(params).await;
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
