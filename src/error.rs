use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::time::Instant;

#[derive(Debug)]
pub struct Error {
    pub status: StatusCode,
    pub code: &'static str,
    pub stage: &'static str,
    pub operation_id: String,
}

impl Error {
    pub fn new(status: StatusCode, code: &'static str, stage: &'static str) -> Self {
        Self { status, code, stage, operation_id: uuid::Uuid::new_v4().to_string() }
    }
    pub fn io(stage: &'static str, error: std::io::Error) -> Self {
        let result = Self::new(StatusCode::INTERNAL_SERVER_ERROR, "HOST_IO", stage);
        tracing::error!(operation_id = %result.operation_id, stage, category = ?error.kind(), errno = error.raw_os_error(), result = "failed");
        result
    }
    pub fn unavailable(stage: &'static str) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "VM_UNAVAILABLE", stage)
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        tracing::warn!(operation_id = %self.operation_id, stage = self.stage, category = self.code, result = "failed");
        (
            self.status,
            Json(
                json!({"error": self.code, "stage": self.stage, "operationId": self.operation_id}),
            ),
        )
            .into_response()
    }
}

pub fn audit(id: &str, operation: &str, stage: &str, started: Instant, success: bool) {
    tracing::info!(
        operation_id = id,
        operation,
        stage,
        elapsed_ms = started.elapsed().as_millis() as u64,
        result = if success { "success" } else { "failed" }
    );
}
