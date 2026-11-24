use axum::http::{Method, StatusCode, Uri};
use axum::response::IntoResponse;
use kp_model::EpochMs;
use serde_json::Value;

use super::{public_error, rest_operation};
use crate::service::Operation;
use crate::NetError;

#[test]
fn why_removes_exactly_one_path_prefix() -> crate::Result<()> {
    let uri = Uri::from_static("/v1/why//v1/why/fact");
    let operation = rest_operation(&Method::GET, &uri, Value::Null, EpochMs(0))?;
    assert!(matches!(operation, Operation::Why(id, _) if id == "/v1/why/fact"));
    Ok(())
}

#[test]
fn store_feedback_permission_denial_is_http_forbidden() {
    let error = NetError::Store(kp_store::StoreError::PermissionDenied);
    assert_eq!(public_error(&error), "forbidden");
    assert_eq!(error.into_response().status(), StatusCode::FORBIDDEN);
}
