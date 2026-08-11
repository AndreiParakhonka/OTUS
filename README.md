# shorty  сервис коротких ссылок (Домашнее задание 1)

Backend-сервис на `axum`: короткие ссылки со статистикой переходов,
rate limiting и TTL. Cargo-workspace из трёх частей:

- `crates/domain`  доменные типы и контракт `LinkRepository` (async),
  **не зависит** от axum/http;
- `crates/storage`  in-memory реализации хранилища (`RwLock`+атомик,
  `DashMap`) и бенчмарк;
- `shorty-server`  бинарник + библиотека с HTTP-слоем (handlers, DTO,
  единый слой ошибок, middleware-стек, rate limiter, фоновые задачи).

## API-контракт

| Метод и путь | Описание | Ответы |
|---|---|---|
| `POST /api/v1/links` | создать ссылку | `201` + `Location`, `409` код занят, `422` невалидные данные, `429` превышен rate limit |
| `GET /api/v1/links/{code}` | метаданные + счётчик | `200`, `404` |
| `GET /api/v1/links/{code}/stats` | переходы за всё время + за 60 с | `200`, `404` |
| `GET /api/v1/stats/top?limit=N` | топ-N ссылок по переходам | `200` (default 10, max 100) |
| `DELETE /api/v1/links/{code}` | удалить | `204`, повторный `404` (задокументированное решение) |
| `GET /{code}` | redirect (hot path) | `307` + `Location`, `404` |
| `GET /healthz`, `GET /version` | технические | `200` |

Тело `POST`: `{"target_url": "...", "custom_code": "promo2026"?, "ttl_seconds": 3600?}`.
Все ошибки  в едином формате `{"code": "...", "message": "...", "request_id": "..."}`;
статусы ошибок: `422` невалидные данные, `409` занятый код, `429` + `Retry-After`, `400/413/422` отказы extractor'а, `500` внутренние.

**Валидация (по ДЗ):** `target_url`  абсолютный URL со схемой `http`/`https`, длина ≤ 2048;
`custom_code`  4..=32 символа из `[a-zA-Z0-9_-]`; `ttl_seconds >= 60`, если задан.
Если `custom_code` не задан  генерируется уникальный код длиной 8 (атомарно, без check-then-act).

## Что демонстрирует проект

- **Слои приложения**: `main.rs`  тонкий бинарник; `build_router(state)`
  в `lib.rs`  тестируемая сборка приложения; `api/`  DTO, ошибки,
  handlers; домен (`crates/domain`) не знает про axum.
- **`AppState`** `{ repo: Arc<dyn LinkRepository>, config: Arc<Config>,
  rate_limiter: Arc<SharedRateLimiter> }`  `Clone` на каждый запрос,
  поэтому внутри `Arc`.
- **Валидация на границе** («parse, don't validate»): `target_url`
  парсится в `url::Url` (только `http`/`https`, длина ≤ 2048), `custom_code` 
  4..=32 символа `[a-zA-Z0-9_-]`, `ttl_seconds >= 60`.
- **Единый слой ошибок**: `AppError` (`thiserror`) + `impl IntoResponse`,
  маппинг в 404/409/422/400/413/429/500 в одном месте; `Internal` логируется
  через `tracing::error!` целиком, наружу  стерильное `internal error`
  с `request_id` для поиска в логах.
- **Rate limiter** (собственный, без `governor`/`tower::limit`): скользящее
  окно «10 созданий в минуту с клиента» (клиент  IP из `X-Forwarded-For`
  или `X-Api-Key`). На переборе  `429` + `Retry-After`. Память ограничена:
  ленивая подрезка окна, потолок `max_clients` с выселением самой старой
  карточки, плюс фоновая задача-«уборщик».
- **Статистика**: `stats_window` хранит скользящее окно переходов за 60 с
  (запись момента времени в `VecDeque`, инкремент total  атомик);
  `top` ранжирует ссылки по суммарному числу переходов.
- **Отказы extractors в нашем формате**: обёртка `AppJson` сводит
  rejections axum (битый JSON, `deny_unknown_fields`, превышение лимита
  тела) к тому же `{code, message, request_id}`.
- **Middleware-стек** (`ServiceBuilder`, порядок «луковицы»):
  `SetRequestIdLayer` → task-local с request id → `TraceLayer`
  (request id  поле спана) → `TimeoutLayer` (5 с, 503) →
  `RequestBodyLimitLayer` (16 КБ) → `PropagateRequestIdLayer`.
- **Генерация кода** через `nanoid` с повтором при коллизии; проверка
  занятости и вставка  одна атомарная операция репозитория.
- **Тесты роутера без сокета** (`tower::ServiceExt::oneshot`,
  `shorty-server/tests/api.rs`): happy path, 404/409/422/400/413/429,
  единый формат ошибок, redirect + счётчик, статистика окна, топ-N,
  конкурентный redirect (100 задач  счётчик ровно 100) и конкурентное
  создание с одинаковым кодом (ровно один `201`, остальные `409`).

## Как запустить

```bash
cargo run
# LISTEN_ADDR=127.0.0.1:9090 RUST_LOG=debug cargo run
```

Параметры через переменные окружения (с разумными дефолтами):

| Переменная | Дефолт | Назначение |
|---|---|---|
| `LISTEN_ADDR` | `0.0.0.0:{PORT}` | адрес прослушивания |
| `PORT` | `8080` | порт, если `LISTEN_ADDR` не задан |
| `SHORTY_RATE_LIMIT_PER_MIN` | `10` | максимум созданий ссылок в минуту с клиента |
| `SHORTY_RATE_LIMIT_MAX_CLIENTS` | `10000` | потолок карточек клиентов в rate limiter'е |
| `SHORTY_MAX_BODY_BYTES` | `16384` | лимит размера тела запроса |
| `SHORTY_WORKER_THREADS` | авто | число worker-потоков tokio |
| `RUST_LOG` | `info` | уровень логов (`tracing`) |

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

# статистика: всего и за последние 60 секунд
curl -s localhost:8080/api/v1/links/rustlang/stats
# {"code":"rustlang",...,"total_hits":3,"hits_last_60s":3}

# топ-N ссылок по переходам
curl -s 'localhost:8080/api/v1/stats/top?limit=3'

# превышение rate limit на POST /api/v1/links -> 429 + Retry-After
for i in $(seq 1 11); do
  curl -s -o /dev/null -w '%{http_code}\n' -X POST localhost:8080/api/v1/links \
    -H 'content-type: application/json' -H 'x-forwarded-for: 203.0.113.9' \
    -d '{"target_url":"https://example.com/'$i'"}'
done
# ... 201 ... 201, затем: 429 {"code":"rate_limited",...} c заголовком Retry-After
```

## Архитектурные решения

**Хранилище и счётчики.** Структуру карты держим под `RwLock`, а счётчик
переходов  в `AtomicU64` внутри `Arc<LinkEntry>` (`crates/storage`). На
redirect инкремент берёт только **read-lock на карту** + атомарный
`fetch_add`  никакой эксклюзивной блокировки всего хранилища. Это
устраняет гонку «потерянных обновлений» (см. `broken::LostUpdateRepo` и
stress-тест на 80 000 инкрементов). `DashMapRepo`  альтернатива без
глобального лока вообще (шардирование), но на одном горячем ключе выигрыша
перед v2 почти нет; в сервисе по умолчанию `InMemoryRepo` (v2).

**Вставка кода без check-then-act.** «Проверка занятости + вставка»  одна
атомарная операция через `HashMap::entry`/`DashMap::entry` под одним
захватом лока. Параллельные POST с одинаковым `custom_code` дают ровно
один `201`, остальные `409` (покрыто конкурентным тестом).

**Rate limiter «10 в минуту с клиента»**  реализован самостоятельно
(запрещённые `governor`/`tower::limit` не использованы). Это **скользящее
окно**: `HashMap<String, VecDeque<Instant>>`, где для каждого клиента
хранятся моменты последних запросов. Запрос пропущен, пока `len < limit`;
при переполнении клиент получает `429` с `Retry-After`, равным времени до
выхода самой старой записи за границу окна. Синхронизация  `tokio::sync::Mutex`
(состояние в `SharedRateLimiter`), т.к. обращаются многие concurrent-запросы.
Клиент определяется по `X-Forwarded-For` (за прокси) или `X-Api-Key`, иначе  `unknown`.
Память не растёт бесконечно: ленивая подрезка устаревших времён при каждом
обращении + жёсткий потолок `max_clients` с выселением самой старой карточки
+ фоновая задача-«уборщик» (`spawn_limiter_cleaner`).

**Окно статистики 60 с.** Точное **скользящее окно**: при каждом переходе
total-счётчик инкрементируется атомиком, а момент времени кладётся в
`VecDeque`, подрезаемый лениво (убираем записи старше 60 с). `GET .../stats`
возвращает `total_hits` и точное `hits_last_60s`. Альтернатива  дискретное
окно с пер-секундными бакетами; выбрано скользящее как более прямолинейное
и легко тестируемое.

**Фоновые задачи и graceful shutdown.** Очистка протухших ссылок
(`cleanup`) и уборщик rate limiter'а исполняются в `TaskTracker` и
корректно выходят по `CancellationToken` (через `select!`), не обрываясь
abort'ом. При SIGINT/SIGTERM сервер дорабатывает соединения, затем
`cancel()` + `tracker.wait()` с общим таймаутом 10 с. В async-коде нет
`std::thread::sleep` и удержания std-lock guard через `.await` (std-мьютекс
только в коротких синхронных секциях).

**Контрактные решения (зафиксированы).**
- `DELETE` несуществующей ссылки → `404` (информативность; идемпотентность
  результата не страдает  ресурса нет в обоих случаях). Покрыто тестом.
- 404 по неизвестному пути  в едином формате ошибок (fallback).
- `ttl_seconds < 60` отбрасывается (`422`), `target_url` длиннее 2048  тоже.

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
