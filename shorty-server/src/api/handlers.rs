//! Handlers — обычные async-функции; extractors в аргументах,
//! `Result<_, AppError>` на выходе. Про HTTP-коды ошибок handlers
//! не знают: это забота `AppError::into_response`.

use std::time::{Duration, SystemTime};

use axum::{
    Json,
    extract::{Path, State},
    http::{StatusCode, header},
    response::IntoResponse,
};
use domain::{RepoError, ShortLink};
use serde::Serialize;

use super::{
    dto::{
        CreateLinkRequest, LinkResponse, expires_at_from_ttl, parse_target_url,
        validate_custom_code,
    },
    error::{AppError, AppJson},
};
use crate::AppState;

// ---------------------------------------------------------------------------
// CRUD ссылок
// ---------------------------------------------------------------------------

/// `POST /api/v1/links` — создать ссылку.
/// `201` + `Location` + тело; `422` при невалидных данных; `409` если код занят.
pub async fn create_link(
    State(state): State<AppState>,
    AppJson(req): AppJson<CreateLinkRequest>,
) -> Result<impl IntoResponse, AppError> {
    let target_url = parse_target_url(&req.target_url)?;
    let expires_at = expires_at_from_ttl(req.ttl_seconds)?;

    let code = match req.custom_code {
        Some(code) => {
            validate_custom_code(&code)?;
            // Проверка занятости и вставка — одна атомарная операция
            // репозитория (никакого contains + insert: между ними
            // параллельный запрос успел бы занять код — check-then-act).
            insert_link(&state, &code, target_url.as_str(), expires_at).await?;
            code
        }
        None => generate_code(&state, target_url.as_str(), expires_at).await?,
    };

    let stats = state.repo.stats(&code).await?;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, format!("/api/v1/links/{code}"))],
        Json(LinkResponse::from(stats)),
    ))
}

/// `GET /api/v1/links/{code}` — метаданные ссылки и счётчик переходов.
pub async fn get_link(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<LinkResponse>, AppError> {
    let stats = state.repo.stats(&code).await?;
    Ok(Json(stats.into()))
}

/// `GET /{code}` — redirect, hot path сервиса (урок 2).
/// `307` + `Location`, счётчик инкрементируется атомарно.
pub async fn redirect(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let link = state.repo.get(&code).await?;
    // Протухшая, но ещё не убранная уборщиком ссылка снаружи
    // неотличима от отсутствующей.
    if link.is_expired(SystemTime::now()) {
        return Err(AppError::NotFound);
    }
    state.repo.record_hit(&code).await?;
    Ok((
        StatusCode::TEMPORARY_REDIRECT,
        [(header::LOCATION, link.target_url)],
    ))
}

/// `DELETE /api/v1/links/{code}` — удаление, `204 No Content`.
///
/// DELETE несуществующего кода: выбираем информативность — `404`
/// (идемпотентность результата от этого не страдает: ресурса нет в обоих
/// случаях). Решение зафиксировано здесь и в тестах; в уроке 10 оно
/// попадёт в OpenAPI-контракт.
pub async fn delete_link(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<StatusCode, AppError> {
    state.repo.remove(&code).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Fallback для неизвестных путей: 404 в едином формате ошибок,
/// а не пустое тело по умолчанию.
pub async fn fallback_404() -> AppError {
    AppError::NotFound
}

// ---------------------------------------------------------------------------
// Вспомогательное: генерация кода
// ---------------------------------------------------------------------------

/// Генерация кода с повтором при коллизии. Каждая попытка — атомарный
/// `insert`; вероятность коллизии nanoid при длине 8 ничтожна, но retry
/// делает поведение корректным, а не «почти всегда корректным».
async fn generate_code(
    state: &AppState,
    target_url: &str,
    expires_at: Option<SystemTime>,
) -> Result<String, AppError> {
    for _ in 0..state.config.max_generate_attempts {
        let code = nanoid::nanoid!(state.config.code_length);
        match try_insert_link(state, &code, target_url, expires_at).await {
            Ok(()) => return Ok(code),
            Err(RepoError::CodeTaken(_)) => continue,
            Err(other) => return Err(other.into()),
        }
    }
    Err(AppError::Internal(anyhow::anyhow!(
        "failed to generate a unique code in {} attempts",
        state.config.max_generate_attempts
    )))
}

async fn insert_link(
    state: &AppState,
    code: &str,
    target_url: &str,
    expires_at: Option<SystemTime>,
) -> Result<(), AppError> {
    try_insert_link(state, code, target_url, expires_at)
        .await
        .map_err(Into::into)
}

async fn try_insert_link(
    state: &AppState,
    code: &str,
    target_url: &str,
    expires_at: Option<SystemTime>,
) -> Result<(), RepoError> {
    let mut link = ShortLink::new(code, target_url);
    if let Some(at) = expires_at {
        link = link.with_expires_at(at);
    }
    state.repo.insert(link).await
}

// ---------------------------------------------------------------------------
// Технические маршруты (уроки 1 и 3)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Health {
    status: &'static str,
}

#[derive(Serialize)]
pub struct Version {
    version: &'static str,
}

pub async fn healthz() -> Json<Health> {
    Json(Health { status: "ok" })
}

pub async fn version() -> Json<Version> {
    Json(Version {
        version: env!("CARGO_PKG_VERSION"),
    })
}

/// ПРАВИЛЬНО: блокирующая работа — в blocking-пуле (урок 3).
pub async fn slow() -> &'static str {
    tokio::task::spawn_blocking(|| {
        std::thread::sleep(Duration::from_secs(2));
    })
    .await
    .expect("blocking task panicked");
    "done: spawn_blocking kept workers free\n"
}

/// НАМЕРЕННО СЛОМАНО (демонстрация урока 3): синхронный sleep
/// монополизирует worker-поток. См. README.
pub async fn slow_blocking() -> &'static str {
    std::thread::sleep(Duration::from_secs(2));
    "done: but a worker thread was blocked for 2s!\n"
}
