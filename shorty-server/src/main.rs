//! Тонкий бинарник: runtime, конфигурация, запуск и жизненный цикл.
//! Вся логика приложения — в библиотеке `shorty_server` (см. lib.rs),
//! иначе её было бы нечем тестировать.

use std::{sync::Arc, time::Duration};

use domain::LinkRepository;
use shorty_server::{AppState, Config, build_router, cleanup};
use storage::InMemoryRepo;
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tracing_subscriber::EnvFilter;

/// Период фоновой очистки протухших ссылок.
const CLEANUP_PERIOD: Duration = Duration::from_secs(30);
/// Сколько ждём фоновые задачи при shutdown, прежде чем выйти принудительно.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Ctrl+C (локальная разработка) + SIGTERM (Docker/оркестратор) через
/// `select!` — реагируем на первый пришедший. На Windows SIGTERM нет,
/// там остаётся только Ctrl+C.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("Ctrl+C received, stopping server"),
        _ = terminate => tracing::info!("SIGTERM received, stopping server"),
    }
}

async fn run() {
    let addr = std::env::var("LISTEN_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("failed to bind {addr}: {e}"));

    let repo: Arc<dyn LinkRepository> = Arc::new(InMemoryRepo::new());
    let state = AppState {
        repo: repo.clone(),
        config: Arc::new(Config::default()),
    };

    // Инфраструктура graceful shutdown: токен — «сигнал всем завершаться»,
    // трекер — «дождаться, пока все фоновые задачи выйдут» (урок 3).
    let shutdown_token = CancellationToken::new();
    let tracker = TaskTracker::new();
    cleanup::spawn_cleaner(repo, CLEANUP_PERIOD, &tracker, shutdown_token.clone());

    tracing::info!(%addr, "shorty server listening");

    // 1. Сервер работает до Ctrl+C/SIGTERM, затем дорабатывает текущие соединения.
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("server error");

    // 2. Сигналим фоновым задачам и дожидаемся их — но не дольше
    //    SHUTDOWN_TIMEOUT, чтобы зависшая задача не держала процесс вечно.
    shutdown_token.cancel();
    tracker.close();
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, tracker.wait()).await {
        Ok(()) => tracing::info!("all background tasks finished, bye"),
        Err(_) => tracing::warn!(
            timeout_secs = SHUTDOWN_TIMEOUT.as_secs(),
            "background tasks did not finish in time, exiting anyway"
        ),
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Runtime собираем явно, чтобы число worker-потоков можно было
    // ограничить для демонстрации блокировки: SHORTY_WORKER_THREADS=2.
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.enable_all();
    if let Ok(n) = std::env::var("SHORTY_WORKER_THREADS") {
        let n: usize = n.parse().expect("SHORTY_WORKER_THREADS must be a number");
        builder.worker_threads(n);
        tracing::info!(workers = n, "worker thread count overridden");
    }
    builder
        .build()
        .expect("failed to build tokio runtime")
        .block_on(run());
}
