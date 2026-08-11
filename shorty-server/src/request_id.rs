//! Request id, доступный из любого места обработки запроса.
//!
//! `SetRequestIdLayer` кладёт id в заголовок запроса; это middleware
//! копирует его в tokio task-local, чтобы `AppError::into_response`
//! мог добавить `request_id` в JSON-тело ошибки, не таская id через
//! аргументы всех handlers.

use axum::{extract::Request, middleware::Next, response::Response};

tokio::task_local! {
    static REQUEST_ID: Option<String>;
}

/// Middleware (`axum::middleware::from_fn`): исполняет остаток цепочки
/// внутри скоупа task-local со значением request id текущего запроса.
pub async fn request_id_scope(req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    REQUEST_ID.scope(id, next.run(req)).await
}

/// Request id текущего запроса, если мы внутри его скоупа.
pub fn current_request_id() -> Option<String> {
    REQUEST_ID.try_with(Clone::clone).ok().flatten()
}
