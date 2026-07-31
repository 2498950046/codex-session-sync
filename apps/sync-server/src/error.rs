use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use sync_core::{ApiError, ApiErrorCode};

use crate::metadata::MetadataError;
use crate::object_store::ObjectStoreError;

#[derive(Debug)]
pub struct HttpError {
    status: StatusCode,
    body: ApiError,
}

impl HttpError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ApiErrorCode::InvalidRequest,
            message,
        )
    }

    pub fn invalid_digest(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ApiErrorCode::InvalidDigest,
            message,
        )
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            ApiErrorCode::ObjectTooLarge,
            message,
        )
    }

    pub fn namespace_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ApiErrorCode::NamespaceNotFound,
            "namespace not found",
        )
    }

    pub fn revision_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ApiErrorCode::RevisionNotFound,
            "revision not found",
        )
    }

    pub fn missing_objects(missing_objects: Vec<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            body: ApiError {
                code: ApiErrorCode::MissingObjects,
                message: "revision references objects that are not present".to_string(),
                current_head: None,
                missing_objects,
            },
        }
    }

    fn new(status: StatusCode, code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ApiError {
                code,
                message: message.into(),
                current_head: None,
                missing_objects: Vec::new(),
            },
        }
    }

    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "sync server request failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiErrorCode::InternalError,
            "internal server error",
        )
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<MetadataError> for HttpError {
    fn from(error: MetadataError) -> Self {
        match error {
            MetadataError::NotFound {
                kind: "namespace", ..
            } => Self::namespace_not_found(),
            MetadataError::NotFound {
                kind: "revision", ..
            } => Self::revision_not_found(),
            MetadataError::NotFound { .. } => Self::new(
                StatusCode::NOT_FOUND,
                ApiErrorCode::InvalidRequest,
                "resource not found",
            ),
            MetadataError::HeadMismatch { current } => Self {
                status: StatusCode::CONFLICT,
                body: ApiError {
                    code: ApiErrorCode::HeadMismatch,
                    message: "namespace head changed since the client last synchronized"
                        .to_string(),
                    current_head: current,
                    missing_objects: Vec::new(),
                },
            },
            MetadataError::EpochMismatch { current } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::HeadMismatch,
                format!("namespace epoch changed; current epoch is {current}"),
            ),
            MetadataError::RevisionConflict { .. } => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCode::InvalidRequest,
                "revision conflicts with immutable server metadata",
            ),
            MetadataError::TooManyObjects { max } => Self::invalid_request(format!(
                "revision must reference no more than {max} unique objects"
            )),
            MetadataError::TooManyObjectReferences { max } => Self::invalid_request(format!(
                "revision must contain no more than {max} object references"
            )),
            MetadataError::InvalidName => {
                Self::invalid_request("displayName must contain 1 to 128 non-control characters")
            }
            MetadataError::RevisionValidation(error) => Self::invalid_request(error.to_string()),
            error @ (MetadataError::UnsupportedSchema { .. }
            | MetadataError::Sqlite(_)
            | MetadataError::Join(_)
            | MetadataError::Json(_)) => Self::internal(error),
        }
    }
}

impl From<ObjectStoreError> for HttpError {
    fn from(error: ObjectStoreError) -> Self {
        match error {
            ObjectStoreError::InvalidDigest { .. } => Self::invalid_digest(error.to_string()),
            ObjectStoreError::Stream { .. } => Self::invalid_request("request body stream failed"),
            ObjectStoreError::ObjectTooLarge { .. } => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                ApiErrorCode::ObjectTooLarge,
                error.to_string(),
            ),
            ObjectStoreError::LengthMismatch { .. } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCode::LengthMismatch,
                error.to_string(),
            ),
            ObjectStoreError::HashMismatch { .. } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCode::HashMismatch,
                error.to_string(),
            ),
            ObjectStoreError::NotFound { .. } => Self::new(
                StatusCode::NOT_FOUND,
                ApiErrorCode::ObjectNotFound,
                "object not found",
            ),
            error @ ObjectStoreError::Io { .. } => Self::internal(error),
        }
    }
}
