//! DTO — wire-формат API.
//!
//! `serde`-атрибуты определяют контракт: `deny_unknown_fields` на входных
//! типах ловит опечатки клиента (и случайное изменение контракта при
//! рефакторинге), `rename_all` фиксирует стиль имён явно.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use domain::LinkStats;
use serde::{Deserialize, Serialize};
use url::Url;

use super::error::AppError;

/// Тело `POST /api/v1/links`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct CreateLinkRequest {
    pub target_url: String,
    #[serde(default)]
    pub custom_code: Option<String>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// Представление ссылки в ответах API.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LinkResponse {
    pub code: String,
    pub target_url: String,
    pub created_at_unix: u64,
    pub expires_at_unix: Option<u64>,
    pub hits: u64,
}

impl From<LinkStats> for LinkResponse {
    fn from(stats: LinkStats) -> Self {
        Self {
            code: stats.link.code,
            target_url: stats.link.target_url,
            created_at_unix: unix_secs(stats.link.created_at),
            expires_at_unix: stats.link.expires_at.map(unix_secs),
            hits: stats.hits,
        }
    }
}

/// Ответ `GET /api/v1/links/{code}/stats`: суммарные переходы и переходы
/// за последние 60 секунд (скользящее окно, см. README)
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LinkStatsWindowResponse {
    pub code: String,
    pub target_url: String,
    pub created_at_unix: u64,
    pub expires_at_unix: Option<u64>,
    /// Все переходы за всё время жизни ссылки.
    pub total_hits: u64,
    /// Переходы за последние 60 секунд.
    pub hits_last_60s: u64,
}

/// Элемент `GET /api/v1/stats/top`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TopLinkResponse {
    pub code: String,
    pub hits: u64,
}

pub fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Максимальная длина target_url (ДЗ: ≤ 2048) 
pub const MAX_URL_LEN: usize = 2048;
/// Минимальный разрешённый TTL в секундах  
pub const MIN_TTL_SECONDS: u64 = 60;

/// «Parse, don't validate»: не проверяем строку, а превращаем её в `Url`,
/// который не может быть невалидным. Дальше границы API строка-«сырец»
/// не проходит. ДЗ: длина ≤ 2048.
pub fn parse_target_url(raw: &str) -> Result<Url, AppError> {
    if raw.len() > MAX_URL_LEN {
        return Err(AppError::Validation(format!(
            "target_url exceeds {MAX_URL_LEN} characters"
        )));
    }
    let url =
        Url::parse(raw).map_err(|e| AppError::Validation(format!("invalid target_url: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(AppError::Validation(format!(
            "target_url must use http or https, got '{}'",
            url.scheme()
        )));
    }
    Ok(url)
}

/// Правила `custom_code`: 4..=32 символа из `[a-zA-Z0-9_-]`.
pub fn validate_custom_code(code: &str) -> Result<(), AppError> {
    if !(4..=32).contains(&code.len()) {
        return Err(AppError::Validation(
            "custom_code must be 4..=32 characters long".to_string(),
        ));
    }
    if !code
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(AppError::Validation(
            "custom_code may contain only [a-zA-Z0-9_-]".to_string(),
        ));
    }
    Ok(())
}

/// TTL → абсолютный момент истечения. ДЗ: `ttl_seconds >= 60`, если задан
pub fn expires_at_from_ttl(ttl_seconds: Option<u64>) -> Result<Option<SystemTime>, AppError> {
    match ttl_seconds {
        None => Ok(None),
        Some(secs) if secs < MIN_TTL_SECONDS => Err(AppError::Validation(format!(
            "ttl_seconds must be at least {MIN_TTL_SECONDS}"
        ))),
        Some(secs) => Ok(Some(SystemTime::now() + Duration::from_secs(secs))),
    }
}
