//! In-memory реализации хранилища ссылок (уроки 2–3).
//!
//! Урок 3: контракт `LinkRepository` стал async, но сама реализация
//! осталась синхронной — локи короткие, `.await` внутри критических
//! секций **нет** (std-guard не `Send`, держать его через `.await`
//! нельзя; правило из документации tokio: std-мьютекс для коротких
//! секций — норма в async-коде). Async-методы трейта — тонкие обёртки
//! над синхронным ядром (инхерентными методами).
//!
//! Эволюция хранилища (урок 2):
//! - [`InMemoryRepoV1`] — `RwLock<HashMap>` с write-lock на каждый переход;
//! - [`InMemoryRepo`] (v2) — read-lock + `AtomicU64` внутри `Arc<LinkEntry>`;
//! - [`DashMapRepo`] (v3) — шардированная карта `DashMap`;
//! - [`broken::LostUpdateRepo`] — намеренно сломанная версия
//!   (check-then-act) для демонстрации потерянных обновлений.

use std::{
    collections::HashMap,
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use dashmap::DashMap;
use domain::{LinkRepository, LinkStats, RepoError, ShortLink};

// ---------------------------------------------------------------------------
// v1: RwLock<HashMap> с изменяемым счётчиком в значении (наследие урока 2)
// ---------------------------------------------------------------------------

struct EntryV1 {
    link: ShortLink,
    hits: u64,
}

/// v1: вся карта под одним `RwLock`, счётчик — обычное поле.
///
/// Проблема: `record_hit` — это hot path (каждый redirect), а инкремент
/// счётчика требует **write-lock на всю карту**. Оставлена в проекте
/// ради бенчмарка урока 2; async-контракт она уже не реализует.
#[derive(Default)]
pub struct InMemoryRepoV1 {
    inner: RwLock<HashMap<String, EntryV1>>,
}

impl InMemoryRepoV1 {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, link: ShortLink) -> Result<(), RepoError> {
        let mut map = self.inner.write().expect("lock poisoned");
        match map.entry(link.code.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => Err(RepoError::CodeTaken(link.code)),
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(EntryV1 { link, hits: 0 });
                Ok(())
            }
        }
    }

    pub fn get(&self, code: &str) -> Result<ShortLink, RepoError> {
        let map = self.inner.read().expect("lock poisoned");
        map.get(code)
            .map(|e| e.link.clone())
            .ok_or_else(|| RepoError::NotFound(code.to_string()))
    }

    pub fn record_hit(&self, code: &str) -> Result<u64, RepoError> {
        let mut map = self.inner.write().expect("lock poisoned");
        let entry = map
            .get_mut(code)
            .ok_or_else(|| RepoError::NotFound(code.to_string()))?;
        entry.hits += 1;
        Ok(entry.hits)
    }

    pub fn stats(&self, code: &str) -> Result<LinkStats, RepoError> {
        let map = self.inner.read().expect("lock poisoned");
        map.get(code)
            .map(|e| LinkStats {
                link: e.link.clone(),
                hits: e.hits,
            })
            .ok_or_else(|| RepoError::NotFound(code.to_string()))
    }
}

// ---------------------------------------------------------------------------
// v2: read-lock + атомарный счётчик внутри Arc<LinkEntry>
// ---------------------------------------------------------------------------

/// Запись хранилища: ссылка и атомарный счётчик переходов.
pub struct LinkEntry {
    link: ShortLink,
    hits: AtomicU64,
}

impl LinkEntry {
    fn new(link: ShortLink) -> Arc<Self> {
        Arc::new(Self {
            link,
            hits: AtomicU64::new(0),
        })
    }

    fn stats(&self) -> LinkStats {
        LinkStats {
            link: self.link.clone(),
            // Независимый счётчик метрик — достаточно Relaxed:
            // никакие другие данные от него не «защищаются».
            hits: self.hits.load(Ordering::Relaxed),
        }
    }
}

/// v2 — основное хранилище сервиса: структура карты — под `RwLock`,
/// счётчик переходов — `AtomicU64`.
///
/// Логика живёт в коротких синхронных методах, async-контракт
/// (`impl LinkRepository`) делегирует в них.
#[derive(Default)]
pub struct InMemoryRepo {
    inner: RwLock<HashMap<String, Arc<LinkEntry>>>,
}

impl InMemoryRepo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, link: ShortLink) -> Result<(), RepoError> {
        // Атомарность через entry API: проверка занятости и вставка —
        // одна операция под одним захватом лока, без check-then-act.
        let mut map = self.inner.write().expect("lock poisoned");
        match map.entry(link.code.clone()) {
            std::collections::hash_map::Entry::Occupied(_) => Err(RepoError::CodeTaken(link.code)),
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(LinkEntry::new(link));
                Ok(())
            }
        }
    }

    pub fn get(&self, code: &str) -> Result<ShortLink, RepoError> {
        let map = self.inner.read().expect("lock poisoned");
        map.get(code)
            .map(|e| e.link.clone())
            .ok_or_else(|| RepoError::NotFound(code.to_string()))
    }

    pub fn remove(&self, code: &str) -> Result<(), RepoError> {
        let mut map = self.inner.write().expect("lock poisoned");
        map.remove(code)
            .map(|_| ())
            .ok_or_else(|| RepoError::NotFound(code.to_string()))
    }

    pub fn record_hit(&self, code: &str) -> Result<u64, RepoError> {
        let map = self.inner.read().expect("lock poisoned");
        let entry = map
            .get(code)
            .ok_or_else(|| RepoError::NotFound(code.to_string()))?;
        // fetch_add возвращает старое значение; Relaxed достаточно —
        // счётчик ни с чем не синхронизирован.
        Ok(entry.hits.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub fn stats(&self, code: &str) -> Result<LinkStats, RepoError> {
        let map = self.inner.read().expect("lock poisoned");
        map.get(code)
            .map(|e| e.stats())
            .ok_or_else(|| RepoError::NotFound(code.to_string()))
    }

    pub fn purge_expired(&self, now: SystemTime) -> usize {
        let mut map = self.inner.write().expect("lock poisoned");
        let before = map.len();
        map.retain(|_, entry| !entry.link.is_expired(now));
        before - map.len()
    }
}

/// Async-контракт поверх синхронного ядра.
///
/// `#[async_trait]` упаковывает каждый метод в `Box<dyn Future + Send>` —
/// это цена dyn-совместимости (`Arc<dyn LinkRepository>` в state урока 4).
#[async_trait::async_trait]
impl LinkRepository for InMemoryRepo {
    async fn insert(&self, link: ShortLink) -> Result<(), RepoError> {
        // Вызов резолвится в инхерентный (синхронный) метод выше.
        InMemoryRepo::insert(self, link)
    }

    async fn get(&self, code: &str) -> Result<ShortLink, RepoError> {
        InMemoryRepo::get(self, code)
    }

    async fn remove(&self, code: &str) -> Result<(), RepoError> {
        InMemoryRepo::remove(self, code)
    }

    async fn record_hit(&self, code: &str) -> Result<u64, RepoError> {
        InMemoryRepo::record_hit(self, code)
    }

    async fn stats(&self, code: &str) -> Result<LinkStats, RepoError> {
        InMemoryRepo::stats(self, code)
    }

    async fn purge_expired(&self, now: SystemTime) -> usize {
        InMemoryRepo::purge_expired(self, now)
    }
}

// ---------------------------------------------------------------------------
// v3: DashMap — шардированная карта
// ---------------------------------------------------------------------------

/// v3: `DashMap` делит карту на шарды, каждый под своим локом.
///
/// Contention на **разных** ключах исчезает без ручного шардирования.
/// На одном горячем ключе выигрыша перед v2 почти нет (оба упираются
/// в атомарный счётчик), а операций над несколькими ключами атомарно
/// уже не сделать — это цена шардирования.
#[derive(Default)]
pub struct DashMapRepo {
    inner: DashMap<String, Arc<LinkEntry>>,
}

impl DashMapRepo {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, link: ShortLink) -> Result<(), RepoError> {
        // entry API DashMap так же атомарен в пределах шарда.
        match self.inner.entry(link.code.clone()) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(RepoError::CodeTaken(link.code)),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(LinkEntry::new(link));
                Ok(())
            }
        }
    }

    pub fn get(&self, code: &str) -> Result<ShortLink, RepoError> {
        self.inner
            .get(code)
            .map(|e| e.link.clone())
            .ok_or_else(|| RepoError::NotFound(code.to_string()))
    }

    pub fn remove(&self, code: &str) -> Result<(), RepoError> {
        self.inner
            .remove(code)
            .map(|_| ())
            .ok_or_else(|| RepoError::NotFound(code.to_string()))
    }

    pub fn record_hit(&self, code: &str) -> Result<u64, RepoError> {
        let entry = self
            .inner
            .get(code)
            .ok_or_else(|| RepoError::NotFound(code.to_string()))?;
        Ok(entry.hits.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub fn stats(&self, code: &str) -> Result<LinkStats, RepoError> {
        self.inner
            .get(code)
            .map(|e| e.stats())
            .ok_or_else(|| RepoError::NotFound(code.to_string()))
    }

    pub fn purge_expired(&self, now: SystemTime) -> usize {
        let before = self.inner.len();
        self.inner.retain(|_, entry| !entry.link.is_expired(now));
        before - self.inner.len()
    }
}

#[async_trait::async_trait]
impl LinkRepository for DashMapRepo {
    async fn insert(&self, link: ShortLink) -> Result<(), RepoError> {
        DashMapRepo::insert(self, link)
    }

    async fn get(&self, code: &str) -> Result<ShortLink, RepoError> {
        DashMapRepo::get(self, code)
    }

    async fn remove(&self, code: &str) -> Result<(), RepoError> {
        DashMapRepo::remove(self, code)
    }

    async fn record_hit(&self, code: &str) -> Result<u64, RepoError> {
        DashMapRepo::record_hit(self, code)
    }

    async fn stats(&self, code: &str) -> Result<LinkStats, RepoError> {
        DashMapRepo::stats(self, code)
    }

    async fn purge_expired(&self, now: SystemTime) -> usize {
        DashMapRepo::purge_expired(self, now)
    }
}

// ---------------------------------------------------------------------------
// Намеренно сломанная версия — для демонстрации потерянных обновлений
// ---------------------------------------------------------------------------

pub mod broken {
    //! **Не использовать в реальном коде.**
    //!
    //! Демонстрация ошибки check-then-act / read-modify-write:
    //! `record_hit` читает счётчик под read-lock, отпускает лок,
    //! а затем записывает `старое + 1` под write-lock. Параллельные
    //! инкременты между чтением и записью теряются — data race
    //! компилятор не допустил, а вот логическую гонку не отменил.

    use std::{collections::HashMap, sync::RwLock};

    use domain::{LinkStats, RepoError, ShortLink};

    struct Entry {
        link: ShortLink,
        hits: u64,
    }

    /// Хранилище с потерянными обновлениями в `record_hit`.
    #[derive(Default)]
    pub struct LostUpdateRepo {
        inner: RwLock<HashMap<String, Entry>>,
    }

    impl LostUpdateRepo {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn insert(&self, link: ShortLink) -> Result<(), RepoError> {
            let mut map = self.inner.write().expect("lock poisoned");
            match map.entry(link.code.clone()) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    Err(RepoError::CodeTaken(link.code))
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(Entry { link, hits: 0 });
                    Ok(())
                }
            }
        }

        pub fn record_hit(&self, code: &str) -> Result<u64, RepoError> {
            // ШАГ 1: читаем значение под read-lock…
            let current = {
                let map = self.inner.read().expect("lock poisoned");
                map.get(code)
                    .map(|e| e.hits)
                    .ok_or_else(|| RepoError::NotFound(code.to_string()))?
            };
            // …лок отпущен: здесь другой поток успевает прочитать
            // то же самое значение.
            std::hint::spin_loop();
            // ШАГ 2: записываем current + 1 под write-lock —
            // параллельные инкременты потеряны.
            let mut map = self.inner.write().expect("lock poisoned");
            let entry = map
                .get_mut(code)
                .ok_or_else(|| RepoError::NotFound(code.to_string()))?;
            entry.hits = current + 1;
            Ok(entry.hits)
        }

        pub fn stats(&self, code: &str) -> Result<LinkStats, RepoError> {
            let map = self.inner.read().expect("lock poisoned");
            map.get(code)
                .map(|e| LinkStats {
                    link: e.link.clone(),
                    hits: e.hits,
                })
                .ok_or_else(|| RepoError::NotFound(code.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    #[test]
    fn v2_sync_contract() {
        let repo = InMemoryRepo::new();
        let link = ShortLink::new("rust", "https://www.rust-lang.org/");
        repo.insert(link.clone()).unwrap();

        // Повторная вставка того же кода — конфликт, а не молчаливая перезапись.
        let dup = repo.insert(ShortLink::new("rust", "https://example.com/"));
        assert_eq!(dup, Err(RepoError::CodeTaken("rust".to_string())));

        assert_eq!(repo.get("rust").unwrap().target_url, link.target_url);
        assert_eq!(repo.record_hit("rust").unwrap(), 1);
        assert_eq!(repo.record_hit("rust").unwrap(), 2);
        assert_eq!(repo.stats("rust").unwrap().hits, 2);

        repo.remove("rust").unwrap();
        assert_eq!(
            repo.get("rust"),
            Err(RepoError::NotFound("rust".to_string()))
        );
    }

    #[test]
    fn purge_expired_removes_only_expired() {
        let repo = InMemoryRepo::new();
        let now = SystemTime::now();

        repo.insert(
            ShortLink::new("old", "https://example.com/old")
                .with_expires_at(now - Duration::from_secs(60)),
        )
        .unwrap();
        repo.insert(
            ShortLink::new("fresh", "https://example.com/fresh")
                .with_expires_at(now + Duration::from_secs(60)),
        )
        .unwrap();
        repo.insert(ShortLink::new("forever", "https://example.com/"))
            .unwrap();

        assert_eq!(repo.purge_expired(now), 1);
        assert!(repo.get("old").is_err());
        assert!(repo.get("fresh").is_ok());
        assert!(repo.get("forever").is_ok());
    }

    #[tokio::test]
    async fn async_contract_via_dyn() {
        // Проверяем контракт через трейт-объект — именно так репозиторий
        // поедет в AppState урока 4 (`Arc<dyn LinkRepository>`).
        let repo: Arc<dyn LinkRepository> = Arc::new(InMemoryRepo::new());
        LinkRepository::insert(
            repo.as_ref(),
            ShortLink::new("rust", "https://www.rust-lang.org/"),
        )
        .await
        .unwrap();
        assert_eq!(repo.record_hit("rust").await.unwrap(), 1);
        assert_eq!(repo.stats("rust").await.unwrap().hits, 1);
    }
}
