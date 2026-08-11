//! Единый слой ошибок API.
//!
//! Handlers возвращают `Result<_, AppError>` и не знают про HTTP-коды:
//! маппинг «вариант → status + JSON-тело» живёт в одном месте —
//! `impl IntoResponse for AppError`. Тело всегда одного формата:
//! `{code, message, request_id}` (машиночитаемый `code` — для логики
//! клиента, `message` — для человека, `request_id` — для поиска в логах).

use axum::{
    Json,
    extract::{FromRequest, Request},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};

use crate::request_id::current_request_id;

/// Ошибка уровня приложения.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Ресурс не найден → 404.
    #[error("resource not found")]
    NotFound,
    /// Код ссылки занят → 409.
    #[error("code is already taken")]
    CodeTaken,
    /// Семантически невалидные данные (URL, код, TTL) → 422.
    #[error("{0}")]
    Validation(String),
    /// Отказ extractor'а (битый JSON, лишние поля, слишком большое
    /// тело). Статус берём у rejection axum, но тело — наше.
    #[error("{message}")]
    InvalidBody { status: StatusCode, message: String },
    /// Всё непредвиденное → 500. Детали (`sqlx`, паники зависимостей…)
    /// наружу не уходят — только в лог; `anyhow` здесь — «сумка» на
    /// самом верху, в сигнатурах домена его нет.
    #[error("internal error")]
    Internal(#[from] anyhow::Error),
}

/// Стандартное тело ошибки — один формат на весь сервис.
#[derive(Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub request_id: Option<String>,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let request_id = current_request_id();

        let (status, code) = match &self {
            AppError::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            AppError::CodeTaken => (StatusCode::CONFLICT, "code_taken"),
            AppError::Validation(_) => (StatusCode::UNPROCESSABLE_ENTITY, "validation_error"),
            AppError::InvalidBody { status, .. } => (
                *status,
                match *status {
                    StatusCode::PAYLOAD_TOO_LARGE => "payload_too_large",
                    StatusCode::UNSUPPORTED_MEDIA_TYPE => "unsupported_media_type",
                    StatusCode::UNPROCESSABLE_ENTITY => "validation_error",
                    _ => "bad_request",
                },
            ),
            AppError::Internal(err) => {
                // Полная ошибка — в лог с request id; клиенту — стерильное
                // «internal error» с тем же id для поиска в логах.
                tracing::error!(error = ?err, request_id = ?request_id, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        };

        let body = ErrorBody {
            code: code.to_string(),
            message: self.to_string(),
            request_id,
        };
        (status, Json(body)).into_response()
    }
}

impl From<domain::RepoError> for AppError {
    fn from(err: domain::RepoError) -> Self {
        match err {
            domain::RepoError::NotFound(_) => AppError::NotFound,
            domain::RepoError::CodeTaken(_) => AppError::CodeTaken,
        }
    }
}

/// Обёртка над `axum::Json`, приводящая отказы extractor'а к нашему
/// формату ошибок: клиент всегда получает `{code, message, request_id}`,
/// а не дефолтное текстовое тело axum («у вас три формата ошибок»).
pub struct AppJson<T>(pub T);

impl<S, T> FromRequest<S> for AppJson<T>
where
    S: Send + Sync,
    T: serde::de::DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(value)) => Ok(AppJson(value)),
            Err(rejection) => Err(AppError::InvalidBody {
                status: rejection.status(),
                message: rejection.body_text(),
            }),
        }
    }
}
