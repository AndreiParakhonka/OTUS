//! Фоновая задача-«уборщик» протухших ссылок (урок 3).
//!
//! Канон graceful shutdown: `CancellationToken` (сигнал «пора
//! завершаться») + `TaskTracker` (дожидание всех фоновых задач) +
//! `select!` в цикле задачи.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use domain::LinkRepository;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

/// Запускает уборщика в `tracker`; задача завершится по `token.cancel()`.
pub fn spawn_cleaner(
    repo: Arc<dyn LinkRepository>,
    period: Duration,
    tracker: &TaskTracker,
    token: CancellationToken,
) {
    tracker.spawn(run_cleaner(repo, period, token));
}

async fn run_cleaner(repo: Arc<dyn LinkRepository>, period: Duration, token: CancellationToken) {
    let mut interval = tokio::time::interval(period);
    // Если тик пропущен (сервис был занят) — не навёрстываем очередь тиков.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        // `select!` гоняет две futures; обе ветки cancellation-safe:
        // `cancelled()` и `tick()` можно безопасно пересоздавать.
        tokio::select! {
            _ = token.cancelled() => {
                tracing::info!("cleaner: cancellation requested, exiting");
                return;
            }
            _ = interval.tick() => {
                let removed = repo.purge_expired(SystemTime::now()).await;
                if removed > 0 {
                    tracing::info!(removed, "cleaner: purged expired links");
                } else {
                    tracing::debug!("cleaner: nothing to purge");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use domain::ShortLink;
    use storage::InMemoryRepo;

    use super::*;

    /// Тест таймерной логики на **виртуальном времени** tokio:
    /// `start_paused = true` останавливает часы, а `sleep` внутри теста
    /// мгновенно «проматывает» их до ближайшего таймера (auto-advance) —
    /// секунды ожидания не тратятся.
    #[tokio::test(start_paused = true)]
    async fn cleaner_purges_expired_links() {
        let repo = Arc::new(InMemoryRepo::new());
        // Ссылка с expires_at в прошлом — кандидат на удаление.
        repo.insert(
            ShortLink::new("old", "https://example.com/old")
                .with_expires_at(SystemTime::now() - Duration::from_secs(3600)),
        )
        .unwrap();
        repo.insert(ShortLink::new("fresh", "https://example.com/fresh"))
            .unwrap();

        let token = CancellationToken::new();
        let tracker = TaskTracker::new();
        spawn_cleaner(
            repo.clone(),
            Duration::from_secs(30),
            &tracker,
            token.clone(),
        );

        // Проматываем виртуальное время за первый тик интервала.
        tokio::time::sleep(Duration::from_secs(31)).await;

        assert!(repo.get("old").is_err(), "expired link must be purged");
        assert!(repo.get("fresh").is_ok(), "live link must survive");

        // Корректное завершение: cancel + дожидание задачи с таймаутом.
        token.cancel();
        tracker.close();
        tokio::time::timeout(Duration::from_secs(5), tracker.wait())
            .await
            .expect("cleaner must exit promptly after cancel");
    }
}
