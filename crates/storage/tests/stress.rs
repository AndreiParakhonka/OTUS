//! Stress-тесты на корректность конкурентного счётчика переходов (урок 2).
//!
//! 8 потоков по 10 000 инкрементов в одну ссылку: у корректных
//! реализаций итог ровно 80 000. Намеренно сломанная версия
//! (read-modify-write с разлочкой между чтением и записью)
//! теряет обновления — это отдельный демонстрационный тест.
//!
//! Тесты работают с синхронным ядром хранилищ (инхерентные методы),
//! поэтому runtime tokio им не нужен.

use domain::ShortLink;
use storage::{DashMapRepo, InMemoryRepo, InMemoryRepoV1, broken::LostUpdateRepo};

const THREADS: usize = 8;
const HITS_PER_THREAD: usize = 10_000;
const TOTAL: u64 = (THREADS * HITS_PER_THREAD) as u64;

/// Запускает 8 потоков по 10 000 операций `hit` и возвращает управление,
/// когда все завершились.
fn hammer(hit: impl Fn() + Sync) {
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(|| {
                for _ in 0..HITS_PER_THREAD {
                    hit();
                }
            });
        }
    });
}

#[test]
fn v1_no_lost_updates() {
    // v1 корректна (инкремент под write-lock), просто медленна на hot path.
    let repo = InMemoryRepoV1::new();
    repo.insert(ShortLink::new("hot", "https://example.com/"))
        .unwrap();
    hammer(|| {
        repo.record_hit("hot").unwrap();
    });
    assert_eq!(repo.stats("hot").unwrap().hits, TOTAL);
}

#[test]
fn v2_no_lost_updates() {
    // v2: read-lock + AtomicU64 — корректно и без write-lock.
    let repo = InMemoryRepo::new();
    repo.insert(ShortLink::new("hot", "https://example.com/"))
        .unwrap();
    hammer(|| {
        repo.record_hit("hot").unwrap();
    });
    assert_eq!(repo.stats("hot").unwrap().hits, TOTAL);
}

#[test]
fn dashmap_no_lost_updates() {
    let repo = DashMapRepo::new();
    repo.insert(ShortLink::new("hot", "https://example.com/"))
        .unwrap();
    hammer(|| {
        repo.record_hit("hot").unwrap();
    });
    assert_eq!(repo.stats("hot").unwrap().hits, TOTAL);
}

/// Демонстрация: сломанная версия теряет обновления.
///
/// Итог недетерминирован (зависит от interleaving планировщика), поэтому
/// жёсткое `assert_eq!(hits, TOTAL)` здесь превратилось бы в flaky-тест.
/// Проверяем инвариант `hits <= TOTAL` и печатаем, сколько потеряно —
/// на практике почти всегда теряются десятки процентов инкрементов.
#[test]
fn broken_repo_loses_updates() {
    let repo = LostUpdateRepo::new();
    repo.insert(ShortLink::new("hot", "https://example.com/"))
        .unwrap();
    hammer(|| {
        repo.record_hit("hot").unwrap();
    });
    let hits = repo.stats("hot").unwrap().hits;
    assert!(hits <= TOTAL, "counter overshoot is impossible: {hits}");
    println!(
        "broken repo: {hits} of {TOTAL} hits recorded, lost {}",
        TOTAL - hits
    );
}
