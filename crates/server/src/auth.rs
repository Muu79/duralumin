use axum::extract::{FromRequestParts, Query};
use axum::http::{StatusCode, request::Parts};
use serde::Deserialize;

use crate::AppState;

pub struct AuthGuard;

#[derive(Deserialize)]
struct KeyQuery {
    key: Option<String>,
}

impl FromRequestParts<AppState> for AuthGuard {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let Some(expected) = &state.config.auth_token else {
            return Ok(AuthGuard);
        };

        if let Some(auth) = parts.headers.get("Authorization") {
            if let Ok(v) = auth.to_str() {
                if v.strip_prefix("Bearer ").map(|t| t == expected).unwrap_or(false) {
                    return Ok(AuthGuard);
                }
            }
        }

        if let Ok(Query(q)) = Query::<KeyQuery>::from_request_parts(parts, state).await {
            if q.key.as_deref() == Some(expected.as_str()) {
                return Ok(AuthGuard);
            }
        }

        Err(StatusCode::UNAUTHORIZED)
    }
}
