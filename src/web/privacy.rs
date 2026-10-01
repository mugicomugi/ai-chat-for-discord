//! The user's own data: how much is stored about them, and erasing it. Any logged-in user may
//! use this, whether or not they have rights in a guild.

use axum::{
    Json,
    extract::State,
    http::header,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::{ApiError, AppState, JsonBody, auth::Session};
use crate::privacy::{self, Holdings};

/// The word the UI sends once the user has confirmed, so that a stray request cannot erase.
const CONFIRMATION: &str = "DELETE";

pub async fn show(
    State(state): State<AppState>,
    session: Session,
) -> Result<Json<Holdings>, ApiError> {
    state
        .db
        .privacy_holdings(session.user_id)
        .await
        .map(Json)
        .map_err(|_| ApiError::Database)
}

#[derive(Deserialize)]
pub struct DeleteRequest {
    confirm: String,
}

/// Erases the user's data, sessions included, so the response also clears the cookie.
pub async fn delete(
    State(state): State<AppState>,
    session: Session,
    JsonBody(request): JsonBody<DeleteRequest>,
) -> Result<Response, ApiError> {
    if request.confirm != CONFIRMATION {
        return Err(ApiError::Invalid(
            "削除の確認ができませんでした。画面を再読み込みして、もう一度操作してください。".into(),
        ));
    }
    let erasure = privacy::erase_user(&state.db, Some(&state.chat), session.user_id)
        .await
        .map_err(|_| ApiError::Database)?;
    tracing::info!(
        talk_runs = erasure.talk_runs,
        talk_runs_in_progress = erasure.talk_runs_in_progress,
        web_conversations = erasure.web_conversations,
        web_sessions = erasure.web_sessions,
        source = "web",
        "privacy_erased"
    );
    Ok((
        [(
            header::SET_COOKIE,
            state.cookies.clear(state.cookies.session()),
        )],
        Json(erasure),
    )
        .into_response())
}
