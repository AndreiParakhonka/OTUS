# shorty — практика урока 4: production-ready REST-сервис

Снапшот учебного проекта `shorty` на конец урока 4. Включает результаты
уроков 1–3 (каркас, конкурентное хранилище, async-контракт и graceful
shutdown) плюс полный REST CRUD, единый слой ошибок, middleware-стек и
тесты роутера. Это прямая стартовая точка домашнего задания 1.

## API-контракт

| Метод и путь | Описание | Ответы |
|---|---|---|
| `POST /api/v1/links` | создать ссылку | `201` + `Location`, `409` код занят, `422` невалидные данные |
| `GET /api/v1/links/{code}` | метаданные + счётчик | `200`, `404` |
| `GET /{code}` | redirect (hot path) | `307` + `Location`, `404` |
| `DELETE /api/v1/links/{code}` | удалить | `204`, `404` (задокументированное решение) |
| `GET /healthz`, `GET /version` | технические | `200` |
| `GET /slow`, `GET /slow-blocking` | демо урока 3 | `200` |

Тело `POST`: `{"target_url": "...", "custom_code": "promo2026"?, "ttl_seconds": 3600?}`.
Все ошибки — в едином формате `{"code": "...", "message": "...", "request_id": "..."}`.

## Что демонстрирует проект

- **Слои приложения**: `main.rs` — тонкий бинарник; `build_router(state)`
  в `lib.rs` — тестируемая сборка приложения; `api/` — DTO, ошибки,
  handlers; домен (`crates/domain`) не знает про axum.
- **`AppState`** `{ repo: Arc<dyn LinkRepository>, config: Arc<Config> }` —
  `Clone` на каждый запрос, поэтому внутри `Arc`.
- **Валидация на границе** («parse, don't validate»): `target_url`
  парсится в `url::Url` (только `http`/`https`), `custom_code` —
  4..=32 символа `[a-zA-Z0-9_-]`, `ttl_seconds >= 1`.
- **Единый слой ошибок**: `AppError` (`thiserror`) + `impl IntoResponse`,
  маппинг в 404/409/422/400/413/500 в одном месте; `Internal` логируется
  через `tracing::error!` целиком, наружу — стерильное `internal error`
  с `request_id` для поиска в логах.
- **Отказы extractors в нашем формате**: обёртка `AppJson` сводит
  rejections axum (битый JSON, `deny_unknown_fields`, превышение лимита
  тела) к тому же `{code, message, request_id}`.
- **Middleware-стек** (`ServiceBuilder`, порядок «луковицы»):
  `SetRequestIdLayer` → task-local с request id → `TraceLayer`
  (request id — поле спана) → `TimeoutLayer` (5 с, 503) →
  `RequestBodyLimitLayer` (16 КБ) → `PropagateRequestIdLayer`.
- **Генерация кода** через `nanoid` с повтором при коллизии; проверка
  занятости и вставка — одна атомарная операция репозитория.
- **Тесты роутера без сокета** (`tower::ServiceExt::oneshot`,
  `shorty-server/tests/api.rs`): happy path, 409/422/400/413, единый
  формат 404 (включая fallback), redirect + счётчик, и конкурентный тест —
  50 параллельных redirect, счётчик ровно 50 (`flavor = "multi_thread"`).

## Как запустить

```bash
cargo run
# LISTEN_ADDR=127.0.0.1:9090 RUST_LOG=debug cargo run
```

## Примеры curl

```bash
# создать ссылку с кастомным кодом и TTL
curl -i -X POST localhost:8080/api/v1/links \
  -H 'content-type: application/json' \
  -d '{"target_url":"https://rust-lang.org/","custom_code":"rustlang","ttl_seconds":3600}'
# HTTP/1.1 201 Created, Location: /api/v1/links/rustlang

# создать со сгенерированным кодом
curl -s -X POST localhost:8080/api/v1/links \
  -H 'content-type: application/json' \
  -d '{"target_url":"https://example.com/"}'

# redirect + счётчик
curl -i localhost:8080/rustlang
# HTTP/1.1 307 Temporary Redirect, Location: https://rust-lang.org/

# метаданные и счётчик переходов
curl -s localhost:8080/api/v1/links/rustlang
# {"code":"rustlang","target_url":"https://rust-lang.org/",...,"hits":1}

# ошибки — единый формат
curl -s -X POST localhost:8080/api/v1/links \
  -H 'content-type: application/json' -d '{"target_url":"ftp://x"}'
# 422 {"code":"validation_error","message":"...","request_id":"..."}
curl -s localhost:8080/api/v1/links/absent
# 404 {"code":"not_found","message":"resource not found","request_id":"..."}

# удалить
curl -i -X DELETE localhost:8080/api/v1/links/rustlang
# HTTP/1.1 204 No Content
```

## Тесты и бенчмарк

```bash
cargo test --workspace             # API-тесты + уборщик + stress урока 2
cargo bench -p storage             # бенчмарк урока 2
cargo bench -p storage -- --test   # быстрая smoke-проверка бенчмарка
```

## Проверки

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
