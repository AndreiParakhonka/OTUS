//! Сервис коротких ссылок `shorty` (снапшот на конец урока 4).
//!
//! Логика приложения вынесена в библиотечный crate, а `main.rs` остался
//! тонким: только runtime, конфигурация и запуск. Так `build_router`
//! тестируется без реального сокета (`tower::ServiceExt::oneshot`).

pub mod api;
pub mod cleanup;
pub mod request_id;

use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    routing::{get, post},
};
use domain::LinkRepository;
use tower::ServiceBuilder;
use tower_http::{
    limit::RequestBodyLimitLayer,
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

/// Конфигурация приложения. Живёт в `AppState` как `Arc<Config>`:
/// дешёвый clone на каждый запрос, данные — в одном экземпляре.
#[derive(Debug, Clone)]
pub struct Config {
    /// Длина генерируемого кода ссылки.
    pub code_length: usize,
    /// Сколько раз пытаемся сгенерировать код при коллизиях.
    pub max_generate_attempts: usize,
    /// Таймаут обработки запроса (весь handler целиком).
    pub request_timeout: Duration,
    /// Лимит размера тела запроса (защита от DoS гигантским телом).
    pub max_body_bytes: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            code_length: 8,
            max_generate_attempts: 5,
            request_timeout: Duration::from_secs(5),
            max_body_bytes: 16 * 1024,
        }
    }
}

/// Состояние приложения. `Clone` обязателен: axum клонирует state на
/// каждый запрос — поэтому внутри `Arc`, а не сами данные.
/// Всё содержимое обязано быть `Send + Sync` (уроки 2–3).
#[derive(Clone)]
pub struct AppState {
    /// Хранилище за трейт-объектом: в тестах — `InMemoryRepo`,
    /// в уроке 5 сюда встанет PostgreSQL без переписывания handlers.
    pub repo: Arc<dyn LinkRepository>,
    pub config: Arc<Config>,
}

/// Сборка приложения: маршруты + middleware-стек.
///
/// Порядок слоёв в `ServiceBuilder` — сверху вниз для запроса
/// («луковица»): сначала выдаём request id, затем трассируем (спан видит
/// id), затем таймаут и лимит тела — ближе всех к handler'ам.
pub fn build_router(state: AppState) -> Router {
    let api_v1 = Router::new()
        .route("/links", post(api::handlers::create_link))
        .route(
            "/links/{code}",
            get(api::handlers::get_link).delete(api::handlers::delete_link),
        );

    Router::new()
        .route("/healthz", get(api::handlers::healthz))
        .route("/version", get(api::handlers::version))
        .route("/slow", get(api::handlers::slow))
        .route("/slow-blocking", get(api::handlers::slow_blocking))
        .nest("/api/v1", api_v1)
        // Redirect — hot path. Статические маршруты (`/healthz`) в axum
        // имеют приоритет над шаблоном `/{code}`.
        .route("/{code}", get(api::handlers::redirect))
        // 404 для неизвестных путей — в едином формате ошибок,
        // а не пустой ответ hyper.
        .fallback(api::handlers::fallback_404)
        .layer(
            ServiceBuilder::new()
                // Снаружи: каждый запрос получает x-request-id (UUID).
                .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
                // Прокидываем request id в task-local, чтобы тело ошибки
                // могло сослаться на него (см. api::error).
                .layer(axum::middleware::from_fn(request_id::request_id_scope))
                // Спан на каждый запрос; request id — поле спана,
                // поэтому он есть в каждой строке логов запроса.
                .layer(TraceLayer::new_for_http().make_span_with(make_span))
                // Таймаут на весь handler: подвисший запрос получает
                // 503 (+ клиенту стоит ретраить позже), а не держит
                // соединение вечно.
                .layer(TimeoutLayer::with_status_code(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    state.config.request_timeout,
                ))
                // Лимит тела: без него POST гигабайтным JSON — DoS.
                .layer(RequestBodyLimitLayer::new(state.config.max_body_bytes))
                // Возвращаем x-request-id клиенту в ответе.
                .layer(PropagateRequestIdLayer::x_request_id()),
        )
        .with_state(state)
}

fn make_span(req: &axum::extract::Request) -> tracing::Span {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    tracing::info_span!(
        "http_request",
        method = %req.method(),
        uri = %req.uri(),
        request_id = %request_id,
    )
}
