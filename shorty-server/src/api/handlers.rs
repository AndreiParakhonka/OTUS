//! Handlers — обычные async-функции; extractors в аргументах,
//! `Result<_, AppError>` на выходе. Про HTTP-коды ошибок handlers
//! не знают: это забота `AppError::into_response`.

use std::time::{Duration, SystemTime};

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};
use domain::{RepoError, ShortLink};
use serde::{Deserialize, Serialize};

use super::{
    dto::{
        CreateLinkRequest, LinkResponse, LinkStatsWindowResponse, TopLinkResponse,
        expires_at_from_ttl, parse_target_url, unix_secs, validate_custom_code,
    },
    error::{AppError, AppJson},
};
use crate::{AppState, rate_limit::Allowance};

// ---------------------------------------------------------------------------
// CRUD ссылок
// ---------------------------------------------------------------------------

/// `POST /api/v1/links` — создать ссылку.
/// `201` + `Location` + тело; `422` при невалидных данных; `409` если код занят;
/// `429` при превышении rate limit (см. `rate_limit`).
pub async fn create_link(
    State(state): State<AppState>,
    headers: HeaderMap,
    AppJson(req): AppJson<CreateLinkRequest>,
) -> Result<impl IntoResponse, AppError> {
    // Rate limit «10 созданий в минуту с клиента». Клиента определяем по
    // `X-Forwarded-For` (за прокси) либо по `X-Api-Key`, иначе — «unknown».
    let client = client_key(&headers);
    match state.rate_limiter.check(&client).await {
        Allowance::Allowed => {}
        Allowance::Denied { retry_after } => {
            return Err(AppError::RateLimited { retry_after });
        }
    }

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
// Статистика
// ---------------------------------------------------------------------------

/// `GET /api/v1/links/{code}/stats` — переходы за всё время и за последние
/// 60 секунд (скользящее окно, реализовано в хранилище).
pub async fn link_stats(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Result<Json<LinkStatsWindowResponse>, AppError> {
    let total = state.repo.stats(&code).await?;
    let window = state
        .repo
        .stats_window(&code, Duration::from_secs(60))
        .await?;
    Ok(Json(LinkStatsWindowResponse {
        code: total.link.code.clone(),
        target_url: total.link.target_url,
        created_at_unix: unix_secs(total.link.created_at),
        expires_at_unix: total.link.expires_at.map(unix_secs),
        total_hits: total.hits,
        hits_last_60s: window.hits,
    }))
}

/// Query-параметры `GET /api/v1/stats/top`.
#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub struct TopQuery {
    /// Сколько строк вернуть (по умолчанию 10, максимум 100).
    pub limit: Option<usize>,
}

/// `GET /api/v1/stats/top?limit=N` — топ-N ссылок по числу переходов.
pub async fn top_links(
    State(state): State<AppState>,
    Query(query): Query<TopQuery>,
) -> Result<Json<Vec<TopLinkResponse>>, AppError> {
    let limit = query.limit.unwrap_or(10).clamp(1, 100);
    let top = state.repo.top(limit).await?;
    let response: Vec<TopLinkResponse> = top
        .into_iter()
        .map(|t| TopLinkResponse {
            code: t.code,
            hits: t.hits,
        })
        .collect();
    Ok(Json(response))
}

/// Ключ клиента для rate limiter'а: IP (из `X-Forwarded-For`) или
/// `X-Api-Key`. Задокументировано в README.
fn client_key(headers: &HeaderMap) -> String {
    if let Some(ip) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return ip
            .split(',')
            .next()
            .map(str::trim)
            .unwrap_or(ip)
            .to_string();
    }
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        return key.trim().to_string();
    }
    "unknown".to_string()
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
