use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;

#[derive(Debug, Serialize)]
#[allow(dead_code)]
pub struct ErrorPayload {
    pub status: String,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

pub fn error_payload(
    message: &str,
    code: &str,
    details: Option<serde_json::Value>,
) -> serde_json::Value {
    let mut payload = json!({
        "status": "error",
        "code": code,
        "message": message,
    });
    if let Some(d) = details {
        payload["details"] = d;
    }
    payload
}

pub struct AppError {
    pub status_code: StatusCode,
    pub body: serde_json::Value,
}

impl AppError {
    pub fn new(status_code: StatusCode, message: &str, code: &str) -> Self {
        Self {
            status_code,
            body: json!({ "detail": error_payload(message, code, None) }),
        }
    }

    #[allow(dead_code)]
    pub fn with_details(
        status_code: StatusCode,
        message: &str,
        code: &str,
        details: serde_json::Value,
    ) -> Self {
        Self {
            status_code,
            body: json!({ "detail": error_payload(message, code, Some(details)) }),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status_code, axum::Json(self.body)).into_response()
    }
}

pub fn http_error(status_code: StatusCode, message: &str, code: &str) -> AppError {
    AppError::new(status_code, message, code)
}
