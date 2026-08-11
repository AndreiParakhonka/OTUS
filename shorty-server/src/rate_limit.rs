//! Самодельный rate limiter (ДЗ): **скользящее окно**.
//!
//! Готовые крейты (`governor`, `tower::limit`) учебной задачей запрещены,
//! поэтому окно реализовано поверх стандартных примитивов синхронизации.
//!
//! Алгоритм: для каждого клиента (по IP / `X-Api-Key`) храним моменты
//! времени последних запросов в `VecDeque`. Запрос разрешён, пока длина
//! окна `< limit`; когда окно заполнено — клиент получает 429 с
//! `Retry-After`, равным оставшемуся времени до выхода самой старой записи
//! за границу окна.
//!
//! Память ограничена: ленивая подрезка устаревших времён на каждом
//! обращении, жёсткий потолок `max_clients` с выселением самой старой
//! карточки, плюс фоновый «уборщик» целиком удаляет карточки
//! простаивающих клиентов. Поэтому состояние не растёт безгранично.
//!
//! Тестируемость: алгоритм принимает `now: tokio::time::Instant` явным
//! параметром, поэтому в `#[tokio::test(start_paused = true)]` его можно
//! «проматывать» `tokio::time::advance` без реального сна.

use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Вердикт rate limiter'а на один запрос.
pub enum Allowance {
    /// Запрос пропущен.
    Allowed,
    /// Лимит превышен; `retry_after` — сколько секунд ждать до retry.
    Denied { retry_after: Duration },
}

/// Скользящее окно запросов по ключу клиента.
struct Window {
    /// Моменты времени последних запросов (в порядке поступления),
    /// подрезаются лениво при каждом обращении.
    times: VecDeque<Instant>,
}

/// Rate limiter: `max_clients` клиентов, `window` длиной, `limit` запросов
/// на клиента в окне.
pub struct RateLimiter {
    /// Ключ клиента (IP или X-Api-Key) → окно запросов.
    clients: HashMap<String, Window>,
    /// Предел числа запросов на клиента за `window`.
    limit: usize,
    /// Длина окна (например, 60 с).
    window: Duration,
    /// Потолок на число записей в памяти (защита от неограниченного роста).
    max_clients: usize,
}

impl RateLimiter {
    pub fn new(limit: usize, window: Duration, max_clients: usize) -> Self {
        Self {
            clients: HashMap::new(),
            limit,
            window,
            max_clients,
        }
    }

    /// Решить, пропустить ли запрос «сейчас», и при разрешении учесть его
    /// в окне. Возвращает `Denied { retry_after }` при переборе лимита.
    pub fn check(&mut self, key: &str, now: Instant) -> Allowance {
        // Ленивая подрезка устаревших карточек — O(число клиентов), но
        // только под единственным локом; масштаб (десятки клиентов) мал.
        self.remove_expired(now);

        // Жёсткий потолок памяти: если добавление нового клиента превысит
        // `max_clients`, выселяем карточку с самой старой записью.
        if !self.clients.contains_key(key) && self.clients.len() >= self.max_clients {
            self.evict_oldest();
        }

        let entry = self
            .clients
            .entry(key.to_owned())
            .or_insert_with(|| Window {
                times: VecDeque::new(),
            });

        // Trim: убираем запросы, уже вышедшие за окно.
        let cutoff = now.checked_sub(self.window).unwrap_or_else(Instant::now);
        while let Some(&front) = entry.times.front() {
            if front >= cutoff {
                break;
            }
            entry.times.pop_front();
        }

        if entry.times.len() >= self.limit {
            // Окно полное: Retry-After = когда выйдет самая старая запись.
            if let Some(&oldest) = entry.times.front() {
                let retry_after = self.window.saturating_sub(now - oldest);
                return Allowance::Denied { retry_after };
            }
        }

        entry.times.push_back(now);
        Allowance::Allowed
    }

    /// Выселить клиента с самой старой записью (O(число клиентов)).
    /// Используется только при превышении `max_clients`.
    fn evict_oldest(&mut self) {
        let mut oldest_key: Option<String> = None;
        let mut oldest_time: Option<Instant> = None;
        for (k, w) in &self.clients {
            if let Some(&t) = w.times.front()
                && oldest_time.is_none_or(|oldest| t < oldest)
            {
                oldest_key = Some(k.clone());
                oldest_time = Some(t);
            }
        }
        if let Some(k) = oldest_key {
            self.clients.remove(&k);
        }
    }

    /// Удалить карточки клиентов, у которых все запросы старше окна.
    /// Возвращает число удалённых. Нужен фоновому уборщику и тестам.
    pub fn remove_expired(&mut self, now: Instant) -> usize {
        let cutoff = now.checked_sub(self.window).unwrap_or_else(Instant::now);
        let before = self.clients.len();
        self.clients.retain(|_, w| {
            while let Some(&front) = w.times.front() {
                if front >= cutoff {
                    break;
                }
                w.times.pop_front();
            }
            !w.times.is_empty()
        });
        before - self.clients.len()
    }

    /// Сколько записей о клиентах сейчас в памяти (для тестов).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.clients.len()
    }
}

/// Синхронная обёртка: `tokio::sync::Mutex`, т.к. состояние limiter'а
/// доступно из многих параллельных handlers POST.
pub struct SharedRateLimiter {
    inner: tokio::sync::Mutex<RateLimiter>,
}

impl SharedRateLimiter {
    pub fn new(limit: usize, window: Duration, max_clients: usize) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(RateLimiter::new(limit, window, max_clients)),
        }
    }

    /// Проверить и (при допуске) зачесть запрос.
    pub async fn check(&self, key: &str) -> Allowance {
        let now = tokio::time::Instant::now();
        self.inner.lock().await.check(key, now)
    }

    /// Удалить протухшие карточки; возвращает число удалённых.
    pub async fn purge(&self, now: tokio::time::Instant) -> usize {
        self.inner.lock().await.remove_expired(now)
    }

    /// Для тестов: число записей о клиентах.
    #[cfg(test)]
    #[allow(clippy::len_without_is_empty)] // тестовый датчик размера
    pub async fn len(&self) -> usize {
        self.inner.lock().await.len()
    }
}

/// Фоновая задача: периодически удаляет карточки простаивающих клиентов,
/// чтобы память limiter'а не росла бесконечно. Корректно завершается по
/// `CancellationToken`; исполняется в `TaskTracker`, чтобы shutdown дожидался её.
pub fn spawn_limiter_cleaner(
    limiter: Arc<SharedRateLimiter>,
    period: Duration,
    token: CancellationToken,
    tracker: &TaskTracker,
) {
    tracker.spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = interval.tick() => {
                    limiter.purge(tokio::time::Instant::now()).await;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Тесты с виртуальным временем tokio: advance проматывает часы без сна.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::{Allowance, RateLimiter};
    use tokio::time::{Duration, Instant};

    trait IsAllowed {
        fn is_allowed(&self) -> bool;
    }
    impl IsAllowed for Allowance {
        fn is_allowed(&self) -> bool {
            matches!(self, Allowance::Allowed)
        }
    }

    /// Лимит восстанавливается со временем: 3/мин, четвёртый — 429,
    /// после окна — снова 200.
    #[tokio::test(start_paused = true)]
    async fn sliding_window_restores_limit() {
        let window = Duration::from_secs(60);
        let mut limiter = RateLimiter::new(3, window, 100);

        for _ in 0..3 {
            assert!(
                limiter.check("ip", Instant::now()).is_allowed(),
                "первая тройка должна проходить"
            );
        }

        // Четвёртый — запрещён, Retry-After близок к 60 с.
        match limiter.check("ip", Instant::now()) {
            Allowance::Denied { retry_after } => {
                assert!(retry_after > Duration::from_secs(58));
            }
            Allowance::Allowed => panic!("четвёртый запрос обязан упереться в лимит"),
        }

        // Проматываем время за окно (с запасом: граница `>= cutoff` считается «в окне»).
        tokio::time::advance(window + Duration::from_secs(1)).await;
        assert!(
            limiter.check("ip", Instant::now()).is_allowed(),
            "после окна доступ к лимиту должен восстановиться"
        );
    }

    /// Память не растёт безгранично: после устаревания карточки клиентов
    /// удаляются.
    #[tokio::test(start_paused = true)]
    async fn stale_clients_are_pruned() {
        let window = Duration::from_secs(60);
        let mut limiter = RateLimiter::new(1, window, 60);
        let now = Instant::now();

        limiter.check("a", now);
        limiter.check("b", now);
        assert_eq!(limiter.len(), 2);

        tokio::time::advance(window + Duration::from_secs(1)).await;
        let removed = limiter.remove_expired(Instant::now());
        assert_eq!(removed, 2);
        assert_eq!(limiter.len(), 0);
    }

    /// Разные клиенты независимы: исчерпание лимита одним не блокирует другого.
    #[tokio::test(start_paused = true)]
    async fn independent_clients() {
        let mut limiter = RateLimiter::new(2, Duration::from_secs(60), 60);
        let now = Instant::now();
        assert!(limiter.check("a", now).is_allowed());
        assert!(limiter.check("a", now).is_allowed());
        assert!(matches!(limiter.check("a", now), Allowance::Denied { .. }));
        assert!(limiter.check("b", now).is_allowed());
        assert!(limiter.check("b", now).is_allowed());
    }
}
