//! Доменный слой сервиса `shorty`.
//!
//! Правило архитектуры: `domain` не знает про `axum` и `sqlx` —
//! здесь только доменные типы и контракты (трейты), которые реализуют
//! инфраструктурные crates (`storage`) и используют слои выше (`api`).

use std::time::SystemTime;

/// Короткая ссылка — основная доменная сущность сервиса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortLink {
    /// Короткий код, по которому происходит redirect (`/{code}`).
    pub code: String,
    /// Целевой URL, куда ведёт ссылка.
    pub target_url: String,
    /// Момент создания ссылки.
    pub created_at: SystemTime,
    /// Опциональный срок жизни: после этого момента ссылка считается
    /// протухшей и удаляется фоновым уборщиком (урок 3).
    pub expires_at: Option<SystemTime>,
}

/// Статистика по ссылке: сама ссылка и количество переходов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkStats {
    pub link: ShortLink,
    pub hits: u64,
}

/// Ошибки контракта хранилища.
///
/// Типизированные ошибки через `thiserror`: по ним сможет матчиться
/// API-слой (урок 4), превращая `CodeTaken` в 409, а `NotFound` — в 404.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RepoError {
    /// Код уже занят другой ссылкой (конфликт уникальности).
    #[error("code '{0}' is already taken")]
    CodeTaken(String),
    /// Ссылка с таким кодом не найдена.
    #[error("link '{0}' not found")]
    NotFound(String),
}

/// Контракт хранилища ссылок — теперь async (урок 3).
///
/// Выбор способа: нативные `async fn` в трейте (стабильны с 1.75) не дают
/// object safety (`dyn LinkRepository`) и авто-`Send`-баундов, а в уроке 4
/// репозиторий поедет в `AppState` как `Arc<dyn LinkRepository>`. Поэтому
/// берём `#[async_trait]`: он разворачивает методы в
/// `Box<dyn Future + Send>` и даёт dyn-совместимость.
#[async_trait::async_trait]
pub trait LinkRepository: Send + Sync {
    /// Сохранить новую ссылку.
    ///
    /// Проверка занятости кода и вставка обязаны быть **одной атомарной
    /// операцией** (никакого check-then-act снаружи): при занятом коде —
    /// `RepoError::CodeTaken`.
    async fn insert(&self, link: ShortLink) -> Result<(), RepoError>;

    /// Получить ссылку по коду.
    async fn get(&self, code: &str) -> Result<ShortLink, RepoError>;

    /// Удалить ссылку по коду.
    async fn remove(&self, code: &str) -> Result<(), RepoError>;

    /// Зарегистрировать переход по ссылке (hot path сервиса).
    /// Возвращает новое значение счётчика.
    async fn record_hit(&self, code: &str) -> Result<u64, RepoError>;

    /// Ссылка вместе со счётчиком переходов.
    async fn stats(&self, code: &str) -> Result<LinkStats, RepoError>;

    /// Удалить все ссылки с `expires_at <= now`; вернуть число удалённых.
    /// Используется фоновым уборщиком.
    async fn purge_expired(&self, now: SystemTime) -> usize;
}

impl ShortLink {
    /// Удобный конструктор с `created_at = now` и без срока жизни.
    pub fn new(code: impl Into<String>, target_url: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            target_url: target_url.into(),
            created_at: SystemTime::now(),
            expires_at: None,
        }
    }

    /// Задать срок жизни ссылки.
    #[must_use]
    pub fn with_expires_at(mut self, expires_at: SystemTime) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    /// Протухла ли ссылка на момент `now`.
    pub fn is_expired(&self, now: SystemTime) -> bool {
        self.expires_at.is_some_and(|exp| exp <= now)
    }
}
