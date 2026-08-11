//! Микробенчмарк: цена write-lock на hot path (урок 2).
//!
//! Смесь 95% `record_hit` / 5% `get` по одной горячей ссылке в 8 потоков:
//! - v1 (`InMemoryRepoV1`): каждый hit — write-lock на всю карту;
//! - v2 (`InMemoryRepo`): hit — read-lock + атомарный `fetch_add`;
//! - v3 (`DashMapRepo`): шардированная карта (на одном ключе ~ как v2).
//!
//! Бенчмарк использует синхронное ядро хранилищ (инхерентные методы).
//! Запуск: `cargo bench -p storage` (быстрая проверка: `cargo bench -- --test`).

use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use domain::ShortLink;
use storage::{DashMapRepo, InMemoryRepo, InMemoryRepoV1};

const THREADS: u64 = 8;

/// Выполняет `iters` операций (95% hit / 5% get), поровну распределённых
/// по 8 потокам, и возвращает суммарное время стены.
fn run_mixed(iters: u64, hit: &(impl Fn() + Sync), get: &(impl Fn() + Sync)) -> Duration {
    let per_thread = iters.div_ceil(THREADS);
    let start = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..THREADS {
            s.spawn(move || {
                for i in 0..per_thread {
                    // Каждая 20-я операция — чтение, остальные — hit.
                    if i % 20 == 0 { get() } else { hit() }
                }
            });
        }
    });
    start.elapsed()
}

fn bench_hot_link(c: &mut Criterion) {
    let mut group = c.benchmark_group("hot_link_95hit_5get_8threads");

    let v1 = InMemoryRepoV1::new();
    v1.insert(ShortLink::new("hot", "https://example.com/"))
        .unwrap();
    group.bench_function("v1_rwlock_write_on_hit", |b| {
        b.iter_custom(|iters| {
            run_mixed(
                iters,
                &|| {
                    std::hint::black_box(v1.record_hit("hot").unwrap());
                },
                &|| {
                    std::hint::black_box(v1.get("hot").unwrap());
                },
            )
        });
    });

    let v2 = InMemoryRepo::new();
    v2.insert(ShortLink::new("hot", "https://example.com/"))
        .unwrap();
    group.bench_function("v2_readlock_atomic_hit", |b| {
        b.iter_custom(|iters| {
            run_mixed(
                iters,
                &|| {
                    std::hint::black_box(v2.record_hit("hot").unwrap());
                },
                &|| {
                    std::hint::black_box(v2.get("hot").unwrap());
                },
            )
        });
    });

    let v3 = DashMapRepo::new();
    v3.insert(ShortLink::new("hot", "https://example.com/"))
        .unwrap();
    group.bench_function("v3_dashmap", |b| {
        b.iter_custom(|iters| {
            run_mixed(
                iters,
                &|| {
                    std::hint::black_box(v3.record_hit("hot").unwrap());
                },
                &|| {
                    std::hint::black_box(v3.get("hot").unwrap());
                },
            )
        });
    });

    group.finish();
}

criterion_group!(benches, bench_hot_link);
criterion_main!(benches);
