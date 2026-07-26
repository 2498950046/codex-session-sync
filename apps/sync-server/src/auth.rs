use std::{fmt, sync::Arc};

use axum::{
    Json,
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION, header::WWW_AUTHENTICATE},
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;
use sync_core::{ApiError, ApiErrorCode};

#[derive(Clone)]
pub struct AuthState {
    token: Arc<[u8]>,
}

impl AuthState {
    pub fn new(token: impl AsRef<[u8]>) -> Self {
        Self {
            token: Arc::from(token.as_ref()),
        }
    }
}

impl fmt::Debug for AuthState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthState")
            .field("token", &"[REDACTED]")
            .finish()
    }
}

pub async fn require_auth(
    State(state): State<AuthState>,
    request: Request,
    next: Next,
) -> Response {
    if is_authorized(&state, &request) {
        next.run(request).await
    } else {
        unauthorized_response()
    }
}

fn is_authorized(state: &AuthState, request: &Request) -> bool {
    let mut values = request.headers().get_all(AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }

    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(candidate) = value.strip_prefix("Bearer ") else {
        return false;
    };
    if candidate.is_empty() {
        return false;
    }

    bool::from(state.token.as_ref().ct_eq(candidate.as_bytes()))
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(WWW_AUTHENTICATE, "Bearer")],
        Json(ApiError {
            code: ApiErrorCode::Unauthorized,
            message: "authentication required".to_string(),
            current_head: None,
            missing_objects: Vec::new(),
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::{Body, to_bytes},
        http::{HeaderValue, Request, StatusCode, header::AUTHORIZATION},
        middleware,
        routing::get,
    };
    use serde_json::Value;
    use tower::ServiceExt;

    use super::*;

    const TOKEN: &str = "test-token-that-must-stay-secret";

    fn app() -> Router {
        let protected = Router::new()
            .route("/protected", get(|| async { StatusCode::NO_CONTENT }))
            .route_layer(middleware::from_fn_with_state(
                AuthState::new(TOKEN),
                require_auth,
            ));

        Router::new()
            .route("/health", get(|| async { "ok" }))
            .merge(protected)
    }

    #[tokio::test]
    async fn public_health_does_not_require_authentication() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_route_accepts_the_configured_bearer_token() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn protected_route_rejects_invalid_authorization_headers() {
        let requests = [
            Request::builder()
                .uri("/protected")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/protected")
                .header(AUTHORIZATION, TOKEN)
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/protected")
                .header(AUTHORIZATION, format!("Basic {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/protected")
                .header(AUTHORIZATION, "Bearer wrong-token")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/protected")
                .header(AUTHORIZATION, "bearer test-token-that-must-stay-secret")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/protected")
                .header(
                    AUTHORIZATION,
                    HeaderValue::from_bytes(b"Bearer \xff").unwrap(),
                )
                .body(Body::empty())
                .unwrap(),
        ];

        for request in requests {
            assert_unauthorized(app().oneshot(request).await.unwrap()).await;
        }

        let mut duplicate = Request::builder()
            .uri("/protected")
            .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        duplicate.headers_mut().append(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer another-token"),
        );
        assert_unauthorized(app().oneshot(duplicate).await.unwrap()).await;
    }

    #[test]
    fn debug_output_redacts_the_token() {
        let output = format!("{:?}", AuthState::new(TOKEN));

        assert_eq!(output, "AuthState { token: \"[REDACTED]\" }");
        assert!(!output.contains(TOKEN));
    }

    async fn assert_unauthorized(response: Response) {
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(WWW_AUTHENTICATE),
            Some(&HeaderValue::from_static("Bearer"))
        );

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["code"], "unauthorized");
        assert_eq!(body["message"], "authentication required");
    }
}
