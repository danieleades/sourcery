//! Benchmarks for the `PostgreSQL` snapshot store.
//!
//! These benchmarks require Docker to be running and will spin up a `PostgreSQL`
//! container using testcontainers.
//!
//! Run with: `cargo bench -p sourcery-postgres`

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use sourcery_core::snapshot::{Snapshot, SnapshotStore};
use sourcery_postgres::snapshot::Store;
use sqlx::PgPool;
use std::sync::OnceLock;
use testcontainers::{ContainerAsync, runners::AsyncRunner};
use testcontainers_modules::postgres::Postgres;
use tokio::runtime::Runtime;
use uuid::Uuid;

/// Shared test database for benchmarks (to avoid spinning up containers per benchmark)
struct BenchDb {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

static BENCH_DB: OnceLock<BenchDb> = OnceLock::new();

fn get_runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| Runtime::new().expect("Failed to create Tokio runtime"))
}

fn get_bench_db() -> &'static BenchDb {
    BENCH_DB.get_or_init(|| {
        get_runtime().block_on(async {
            let container = Postgres::default().start().await.unwrap();
            let host = container.get_host().await.unwrap();
            let port = container.get_host_port_ipv4(5432).await.unwrap();

            let connection_string = format!("postgres://postgres:postgres@{host}:{port}/postgres");
            let pool = PgPool::connect(&connection_string).await.unwrap();

            // Run migrations once
            let store = Store::always(pool.clone());
            store.migrate().await.unwrap();

            BenchDb {
                _container: container,
                pool,
            }
        })
    })
}

fn bench_snapshot_load(c: &mut Criterion) {
    let db = get_bench_db();
    let rt = get_runtime();
    let store = Store::always(db.pool.clone());

    // Pre-populate some snapshots
    let ids: Vec<Uuid> = (0..100).map(|_| Uuid::new_v4()).collect();
    rt.block_on(async {
        for id in &ids {
            store
                .offer_snapshot::<std::convert::Infallible, _>("bench.aggregate", id, 100, || {
                    Ok(Snapshot {
                        position: 1000,
                        data: vec![0u8; 1024], // 1KB snapshot
                    })
                })
                .await
                .unwrap();
        }
    });

    let mut group = c.benchmark_group("snapshot_load");
    group.throughput(Throughput::Elements(1));

    group.bench_function("load_existing_1kb", |b| {
        let mut idx = 0;
        b.iter(|| {
            let id = &ids[idx % ids.len()];
            idx += 1;
            rt.block_on(async { store.load("bench.aggregate", id).await.unwrap() })
        });
    });

    group.bench_function("load_nonexistent", |b| {
        b.iter(|| {
            let id = Uuid::new_v4();
            rt.block_on(async { store.load("bench.aggregate", &id).await.unwrap() })
        });
    });

    group.finish();
}

fn bench_snapshot_offer(c: &mut Criterion) {
    let db = get_bench_db();
    let rt = get_runtime();

    let mut group = c.benchmark_group("snapshot_offer");
    group.throughput(Throughput::Elements(1));

    // Benchmark insert (new snapshot)
    group.bench_function("insert_1kb", |b| {
        let store = Store::always(db.pool.clone());
        b.iter(|| {
            let id = Uuid::new_v4();
            rt.block_on(async {
                store
                    .offer_snapshot::<std::convert::Infallible, _>(
                        "bench.aggregate",
                        &id,
                        100,
                        || {
                            Ok(Snapshot {
                                position: 1000,
                                data: vec![0u8; 1024],
                            })
                        },
                    )
                    .await
                    .unwrap()
            })
        });
    });

    // Benchmark upsert (updating existing snapshot)
    group.bench_function("upsert_1kb", |b| {
        let store = Store::always(db.pool.clone());
        let id = Uuid::new_v4();
        let mut position = 0i64;

        // Create initial snapshot
        rt.block_on(async {
            store
                .offer_snapshot::<std::convert::Infallible, _>("bench.aggregate", &id, 100, || {
                    Ok(Snapshot {
                        position: 0,
                        data: vec![0u8; 1024],
                    })
                })
                .await
                .unwrap()
        });

        b.iter(|| {
            position += 1;
            let pos = position;
            rt.block_on(async {
                store
                    .offer_snapshot::<std::convert::Infallible, _>(
                        "bench.aggregate",
                        &id,
                        100,
                        || {
                            Ok(Snapshot {
                                position: pos,
                                data: vec![0u8; 1024],
                            })
                        },
                    )
                    .await
                    .unwrap()
            })
        });
    });

    // Benchmark policy decline (no DB write)
    group.bench_function("policy_decline", |b| {
        let store = Store::never(db.pool.clone());
        let id = Uuid::new_v4();

        b.iter(|| {
            rt.block_on(async {
                store
                    .offer_snapshot::<std::convert::Infallible, _>(
                        "bench.aggregate",
                        &id,
                        100,
                        || {
                            Ok(Snapshot {
                                position: 1000,
                                data: vec![0u8; 1024],
                            })
                        },
                    )
                    .await
                    .unwrap()
            })
        });
    });

    group.finish();
}

fn bench_snapshot_sizes(c: &mut Criterion) {
    let db = get_bench_db();
    let rt = get_runtime();
    let store = Store::always(db.pool.clone());

    let mut group = c.benchmark_group("snapshot_sizes");

    for size in [256, 1024, 4096, 16384, 65536] {
        group.throughput(Throughput::Bytes(size as u64));

        group.bench_function(format!("insert_{size}b"), |b| {
            b.iter(|| {
                let id = Uuid::new_v4();
                rt.block_on(async {
                    store
                        .offer_snapshot::<std::convert::Infallible, _>(
                            "bench.aggregate",
                            &id,
                            100,
                            || {
                                Ok(Snapshot {
                                    position: 1000,
                                    data: vec![0u8; size],
                                })
                            },
                        )
                        .await
                        .unwrap()
                })
            });
        });

        // Pre-populate for load benchmark
        let id = Uuid::new_v4();
        rt.block_on(async {
            store
                .offer_snapshot::<std::convert::Infallible, _>("bench.aggregate", &id, 100, || {
                    Ok(Snapshot {
                        position: 1000,
                        data: vec![0u8; size],
                    })
                })
                .await
                .unwrap()
        });

        group.bench_function(format!("load_{size}b"), |b| {
            b.iter(|| rt.block_on(async { store.load("bench.aggregate", &id).await.unwrap() }));
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_snapshot_load,
    bench_snapshot_offer,
    bench_snapshot_sizes
);
criterion_main!(benches);
