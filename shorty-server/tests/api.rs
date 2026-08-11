//! Тесты HTTP-слоя без реального сокета: `Router` — это `tower::Service`,
//! поэтому `ServiceExt::oneshot` скармливает ему `http::Request` и
//! возвращает `Response` in-process. Быстро и параллелизуемо.
//!
//! Проверяем не только happy path: формат ошибок — такой же контракт,
//! как и успешные ответы.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode, header},
};
use domain::ShortLink;
use http_body_util::BodyExt;
use serde::de::DeserializeOwned;
use shorty_server::{
    AppState, Config, RATE_LIMIT_WINDOW,
    api::{
        dto::{LinkResponse, LinkStatsWindowResponse, TopLinkResponse},
        error::ErrorBody,
    },
    build_router,
    rate_limit::SharedRateLimiter,
};
use storage::InMemoryRepo;
use tower::ServiceExt;

fn test_state() -> (AppState, Arc<InMemoryRepo>) {
    let repo = Arc::new(InMemoryRepo::new());
    let config = Arc::new(Config::default());
    let rate_limiter = Arc::new(SharedRateLimiter::new(
        config.rate_limit_per_minute,
        RATE_LIMIT_WINDOW,
        config.rate_limit_max_clients,
    ));
    let state = AppState {
        repo: repo.clone(),
        config,
        rate_limiter,
    };
    (state, repo)
}

/// Состояние с узким лимитом rate limiter'а — для детерминированного теста 429.
fn state_with_rate_limit(per_minute: usize) -> AppState {
    let repo = Arc::new(InMemoryRepo::new());
    AppState {
        repo,
        config: Arc::new(Config {
            rate_limit_per_minute: per_minute,
            ..Config::default()
        }),
        rate_limiter: Arc::new(SharedRateLimiter::new(
            per_minute,
            RATE_LIMIT_WINDOW,
            10_000,
        )),
    }
}

fn app() -> Router {
    build_router(test_state().0)
}

fn app_with_state(state: AppState) -> Router {
    build_router(state)
}

fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
    post_json_from(uri, body, None)
}

/// POST с указанием клиента (`X-Forwarded-For`) — управление rate limit.
fn post_json_from(uri: &str, body: serde_json::Value, client: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, mime::APPLICATION_JSON.as_ref());
    if let Some(client) = client {
        builder = builder.header("x-forwarded-for", client);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

async fn json_body<T: DeserializeOwned>(response: Response<Body>) -> T {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        panic!(
            "invalid json body: {e}; raw: {}",
            String::from_utf8_lossy(&bytes)
        )
    })
}

// ---------------------------------------------------------------------------
// Happy path
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_then_get() {
    let app = app();

    let response = app
        .clone()
        .oneshot(post_json(
            "/api/v1/links",
            serde_json::json!({
                "target_url": "https://example.com/page",
                "custom_code": "promo2026",
                "ttl_seconds": 3600,
            }),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(
        response.headers()[header::LOCATION],
        "/api/v1/links/promo2026"
    );
    // request id доезжает до клиента в заголовке ответа.
    assert!(response.headers().contains_key("x-request-id"));

    let created: LinkResponse = json_body(response).await;
    assert_eq!(created.code, "promo2026");
    assert_eq!(created.target_url, "https://example.com/page");
    assert_eq!(created.hits, 0);
    assert!(created.expires_at_unix.is_some());

    let response = app.oneshot(get("/api/v1/links/promo2026")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let fetched: LinkResponse = json_body(response).await;
    assert_eq!(fetched.code, "promo2026");
    assert_eq!(fetched.hits, 0);
}

#[tokio::test]
async fn create_generates_code_when_custom_is_absent() {
    let response = app()
        .oneshot(post_json(
            "/api/v1/links",
            serde_json::json!({"target_url": "https://example.com/"}),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let created: LinkResponse = json_body(response).await;
    assert_eq!(created.code.len(), Config::default().code_length);
    assert!(
        created
            .code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    );
    assert_eq!(created.expires_at_unix, None);
}

#[tokio::test]
async fn redirect_increments_counter() {
    let app = app();
    app.clone()
        .oneshot(post_json(
            "/api/v1/links",
            serde_json::json!({"target_url": "https://example.com/hot", "custom_code": "hot-link"}),
        ))
        .await
        .unwrap();

    let response = app.clone().oneshot(get("/hot-link")).await.unwrap();
    assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.headers()[header::LOCATION],
        "https://example.com/hot"
    );

    let response = app.oneshot(get("/api/v1/links/hot-link")).await.unwrap();
    let stats: LinkResponse = json_body(response).await;
    assert_eq!(stats.hits, 1);
}

#[tokio::test]
async fn delete_then_404() {
    let app = app();
    app.clone()
        .oneshot(post_json(
            "/api/v1/links",
            serde_json::json!({"target_url": "https://example.com/", "custom_code": "gone"}),
        ))
        .await
        .unwrap();

    let request = Request::builder()
        .method("DELETE")
        .uri("/api/v1/links/gone")
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = app
        .clone()
        .oneshot(get("/api/v1/links/gone"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Задокументированное решение контракта: повторный DELETE — 404.
    let request = Request::builder()
        .method("DELETE")
        .uri("/api/v1/links/gone")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Контракт ошибок
// ---------------------------------------------------------------------------

#[tokio::test]
async fn duplicate_code_conflict() {
    let app = app();
    let payload =
        serde_json::json!({"target_url": "https://example.com/", "custom_code": "occupied"});

    let response = app
        .clone()
        .oneshot(post_json("/api/v1/links", payload.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = app
        .oneshot(post_json("/api/v1/links", payload))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let error: ErrorBody = json_body(response).await;
    assert_eq!(error.code, "code_taken");
    assert!(error.request_id.is_some(), "error body carries request id");
}

#[tokio::test]
async fn invalid_url_is_422_in_unified_format() {
    for target in ["not a url at all", "ftp://example.com/file"] {
        let response = app()
            .oneshot(post_json(
                "/api/v1/links",
                serde_json::json!({"target_url": target}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let error: ErrorBody = json_body(response).await;
        assert_eq!(error.code, "validation_error");
    }
}

#[tokio::test]
async fn invalid_custom_code_is_422() {
    for code in ["ab", "тест-код", "x".repeat(33).as_str()] {
        let response = app()
            .oneshot(post_json(
                "/api/v1/links",
                serde_json::json!({"target_url": "https://example.com/", "custom_code": code}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}

#[tokio::test]
async fn malformed_json_is_our_error_format_not_axum_default() {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/links")
        .header(header::CONTENT_TYPE, mime::APPLICATION_JSON.as_ref())
        .body(Body::from("{not json"))
        .unwrap();
    let response = app().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let error: ErrorBody = json_body(response).await;
    assert_eq!(error.code, "bad_request");
}

#[tokio::test]
async fn unknown_field_rejected_by_deny_unknown_fields() {
    let response = app()
        .oneshot(post_json(
            "/api/v1/links",
            serde_json::json!({"target_url": "https://example.com/", "tarhet_url_typo": 1}),
        ))
        .await
        .unwrap();
    // JsonDataError у axum — 422; тело — наш единый формат.
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let error: ErrorBody = json_body(response).await;
    assert_eq!(error.code, "validation_error");
}

#[tokio::test]
async fn unknown_paths_fall_back_to_unified_404() {
    for uri in [
        "/no-such-code",
        "/api/v1/links/absent",
        "/deeply/nested/path",
    ] {
        let response = app().oneshot(get(uri)).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "uri: {uri}");
        let error: ErrorBody = json_body(response).await;
        assert_eq!(error.code, "not_found");
    }
}

#[tokio::test]
async fn oversized_body_is_413_in_unified_format() {
    let huge = "x".repeat(Config::default().max_body_bytes + 1);
    let response = app()
        .oneshot(post_json(
            "/api/v1/links",
            serde_json::json!({"target_url": format!("https://example.com/{huge}")}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let error: ErrorBody = json_body(response).await;
    assert_eq!(error.code, "payload_too_large");
}

#[tokio::test]
async fn expired_link_is_gone_from_outside() {
    // Протухшая, но ещё не убранная уборщиком ссылка — снаружи 404.
    let (state, repo) = test_state();
    repo.insert(
        ShortLink::new("stale", "https://example.com/")
            .with_expires_at(SystemTime::now() - Duration::from_secs(60)),
    )
    .unwrap();

    let response = build_router(state).oneshot(get("/stale")).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Конкурентный тест: state под параллельной нагрузкой
// ---------------------------------------------------------------------------

/// 100 задач параллельно дёргают redirect — счётчик ровно 100.
/// `flavor = "multi_thread"` обязателен: однопоточный runtime исполняет
/// задачи по очереди и не поймал бы гонку. ДЗ требует >= 100 задач.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_redirects_count_exactly() {
    let app = app();
    app.clone()
        .oneshot(post_json(
            "/api/v1/links",
            serde_json::json!({"target_url": "https://example.com/", "custom_code": "stress"}),
        ))
        .await
        .unwrap();

    let mut handles = Vec::new();
    for _ in 0..100 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let response = app.oneshot(get("/stress")).await.unwrap();
            assert_eq!(response.status(), StatusCode::TEMPORARY_REDIRECT);
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }

    let response = app.oneshot(get("/api/v1/links/stress")).await.unwrap();
    let stats: LinkResponse = json_body(response).await;
    assert_eq!(stats.hits, 100);
}

// ---------------------------------------------------------------------------
// ДЗ: статистика окна, топ, rate limit, конкурентное создание
// ---------------------------------------------------------------------------

#[tokio::test]
async fn link_stats_window_reports_total_and_recent() {
    let app = app();
    app.clone()
        .oneshot(post_json(
            "/api/v1/links",
            serde_json::json!({"target_url": "https://example.com/", "custom_code": "stat"}),
        ))
        .await
        .unwrap();
    // 3 перехода сейчас (попадут и в общий счётчик, и в окно 60 с).
    for _ in 0..3 {
        app.clone().oneshot(get("/stat")).await.unwrap();
    }

    let response = app.oneshot(get("/api/v1/links/stat/stats")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let stats: LinkStatsWindowResponse = json_body(response).await;
    assert_eq!(stats.total_hits, 3);
    assert_eq!(stats.hits_last_60s, 3);
}

#[tokio::test]
async fn stats_top_returns_ranked_links() {
    let app = app();
    // Создаём три ссылки с разной популярностью.
    for (code, count) in [("a-link", 1u32), ("b-link", 5), ("c-link", 3)] {
        app.clone()
            .oneshot(post_json(
                "/api/v1/links",
                serde_json::json!({
                    "target_url": format!("https://example.com/{code}"),
                    "custom_code": code,
                }),
            ))
            .await
            .unwrap();
        for _ in 0..count {
            app.clone().oneshot(get(&format!("/{code}"))).await.unwrap();
        }
    }

    let response = app
        .clone()
        .oneshot(get("/api/v1/stats/top?limit=2"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let top: Vec<TopLinkResponse> = json_body(response).await;
    assert_eq!(top.len(), 2);
    assert_eq!(top[0].code, "b-link");
    assert_eq!(top[1].code, "c-link");

    // default limit=10, но ссылок всего 3 → вернутся все.
    let response = app.oneshot(get("/api/v1/stats/top")).await.unwrap();
    let top: Vec<TopLinkResponse> = json_body(response).await;
    assert_eq!(top.len(), 3);
    assert_eq!(top[0].code, "b-link");
}

#[tokio::test]
async fn rate_limit_returns_429_with_retry_after() {
    let app = app_with_state(state_with_rate_limit(2));
    let payload =
        serde_json::json!({"target_url": "https://example.com/", "custom_code": "x-link"});

    // Первые два создания с одного клиента — успех.
    for i in 0..2 {
        let response = app
            .clone()
            .oneshot(post_json_from(
                "/api/v1/links",
                serde_json::json!({
                    "target_url": format!("https://example.com/r{i}"),
                    "custom_code": format!("cl-{i}"),
                }),
                Some("1.2.3.4"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    // Третий — 429 с Retry-After и единым форматом.
    let response = app
        .oneshot(post_json_from("/api/v1/links", payload, Some("1.2.3.4")))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        response.headers().get(header::RETRY_AFTER).is_some(),
        "429 обязан нести Retry-After"
    );
    let error: ErrorBody = json_body(response).await;
    assert_eq!(error.code, "rate_limited");
    assert!(error.request_id.is_some());
}

#[tokio::test]
async fn rate_limit_is_per_client() {
    let app = app_with_state(state_with_rate_limit(1));
    let body = serde_json::json!({"target_url": "https://example.com/"});

    // Клиент A исчерпал лимит (второй запрос — 429).
    let r = app
        .clone()
        .oneshot(post_json_from(
            "/api/v1/links",
            body.clone(),
            Some("10.0.0.1"),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let r = app
        .clone()
        .oneshot(post_json_from(
            "/api/v1/links",
            body.clone(),
            Some("10.0.0.1"),
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);

    // Клиент B не затронут.
    let r = app
        .oneshot(post_json_from("/api/v1/links", body, Some("10.0.0.2")))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
}

/// Конкурентное создание с одинаковым custom_code: ровно один 201, остальные 409.
/// Каждая задача — «свой клиент» (уникальный X-Forwarded-For), чтобы rate limit
/// не вмешивался в проверку атомарности вставки кода.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_create_same_code_single_success() {
    let app = app();
    let mut handles = Vec::new();
    for i in 0..20u32 {
        let app = app.clone();
        handles.push(tokio::spawn(async move {
            let client = format!("7.7.{}.{}", i % 256, i);
            let response = app
                .oneshot(post_json_from(
                    "/api/v1/links",
                    serde_json::json!({"target_url": "https://example.com/", "custom_code": "dupe"}),
                    Some(&client),
                ))
                .await
                .unwrap();
            response.status()
        }));
    }
    let mut created = 0;
    for handle in handles {
        let status = handle.await.unwrap();
        if status == StatusCode::CREATED {
            created += 1;
        } else {
            assert_eq!(status, StatusCode::CONFLICT);
        }
    }
    assert_eq!(created, 1, "ровно одна задача должна выиграть вставку кода");
}
