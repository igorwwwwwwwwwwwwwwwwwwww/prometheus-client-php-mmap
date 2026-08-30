use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use prometheus_client_php_mmap::aggregate::aggregate_dir_to_prometheus_text;
use prometheus_client_php_mmap::mmap_file::MmapMetricStore;
use tempfile::tempdir;

fn bench_increment_existing(c: &mut Criterion) {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("counter_1-0.db");

    let mut store = MmapMetricStore::open(&db).expect("open");
    store
        .increment(
            "http_requests_total",
            "http_requests_total",
            r#"{route="/hello"}"#,
            1.0,
        )
        .expect("prime");

    c.bench_function("mmap_increment_existing_key", |b| {
        b.iter(|| {
            store
                .increment(
                    black_box("http_requests_total"),
                    black_box("http_requests_total"),
                    black_box(r#"{route="/hello"}"#),
                    black_box(1.0),
                )
                .expect("increment");
        });
    });
}

fn bench_set_existing(c: &mut Criterion) {
    let dir = tempdir().expect("tempdir");
    let db = dir.path().join("gauge_all_1-0.db");

    let mut store = MmapMetricStore::open(&db).expect("open");
    store
        .set("demo_inflight_requests", "demo_inflight_requests", "", 0.0)
        .expect("prime");

    c.bench_function("mmap_set_existing_key", |b| {
        b.iter(|| {
            store
                .set(
                    black_box("demo_inflight_requests"),
                    black_box("demo_inflight_requests"),
                    black_box(""),
                    black_box(1.0),
                )
                .expect("set");
        });
    });
}

fn bench_render_dir(c: &mut Criterion) {
    c.bench_function("aggregate_render_dir_2_files_1k_series", |b| {
        b.iter_batched(
            || {
                let dir = tempdir().expect("tempdir");
                let p1 = dir.path().join("counter_1-0.db");
                let p2 = dir.path().join("counter_2-0.db");
                let mut s1 = MmapMetricStore::open(&p1).expect("open p1");
                let mut s2 = MmapMetricStore::open(&p2).expect("open p2");
                for i in 0..1_000usize {
                    let labels = format!(r#"{{id="{}"}}"#, i);
                    s1.increment("http_requests_total", "http_requests_total", &labels, 1.0)
                        .expect("s1 increment");
                    s2.increment("http_requests_total", "http_requests_total", &labels, 1.0)
                        .expect("s2 increment");
                }
                s1.flush().expect("flush s1");
                s2.flush().expect("flush s2");
                dir
            },
            |dir| {
                let out = aggregate_dir_to_prometheus_text(dir.path().to_str().expect("utf8 path"))
                    .expect("render");
                black_box(out);
            },
            BatchSize::SmallInput,
        )
    });
}

criterion_group!(
    benches,
    bench_increment_existing,
    bench_set_existing,
    bench_render_dir
);
criterion_main!(benches);
