# prometheus-client-php-mmap

A sloppy Rust `ext-php-rs` extension inspired by [GitLab's
`prometheus-client-mmap` Ruby gem](https://gitlab.com/gitlab-org/ruby/gems/prometheus-client-mmap).

## Features

- mmap-backed metric storage (`.db` files), versioned entry structure:
  - 4-byte `used` header + 4-byte format `version`
  - repeated entries: `[u32 family_len][u32 sample_len][u32 labels_len][family][sample][labels][space padding][f64 value]`
- metric operations: `increment`, `set`, `get`, `flush`
- multiprocess merge and render to Prometheus text format
- native PHP extension API via [`ext-php-rs`](https://github.com/extphprs/ext-php-rs)

Labels are stored in canonical Prometheus text form, sorted and escaped once at write time.
Example labels blob: `{method="GET"}`.

## Consistency Model

- **One-writer-per-file assumption**: each metric file is written by a single worker process.
- **Publication order**: entry bytes are written first, then `used` is advanced. A `Release` fence is used before writing `used` so readers do not observe an advanced boundary before entry bytes are visible on weakly ordered CPUs.
- **Architecture note**: x86-64 TSO often masks publication-order bugs, while weakly ordered architectures (e.g. ARM, RISC-V, POWER) can expose them. The `Release` fence keeps behavior portable and correct across architectures (typically a no-op on x86, real ordering barrier on weaker models).
- **64-bit value writes**: metric values are encoded as 8-byte `f64`. On 64-bit targets this is typically observed atomically when naturally aligned; on 32-bit targets concurrent read/write can observe torn/intermediate values. Expected impact is transient wrong samples (not process crashes). Treat 32-bit as unsupported for strong correctness guarantees.
- **Reader contract**: readers treat `used` as parse boundary, aggregate by scanning `*.db` files in the metrics directory, and skip files with unknown format versions.
- **Flush/durability**:
  - Writes update mmap memory immediately.
  - `flush()` requests `msync` and improves durability.
  - Without `flush()`, kernel writeback is asynchronous; recent updates can be lost on crash/power loss.
- **Visibility**: stale reads can happen briefly (scrape lag), but should converge as writes become visible.
- **Persistent handles**: extension-side file handles/mappings are cached per worker process for performance.
  - If metric `*.db` files are manually deleted while workers are running, restart php-fpm workers to re-establish mappings.
- **Scope**: this is a pragmatic observability model (high throughput, best-effort durability), not a transactional storage model.

## TODO

- Evaluate implementing `PrometheusProto` (binary protobuf exposition format) in addition to text format.
  See: [content negotiation docs](https://prometheus.io/docs/instrumenting/content_negotiation/) and [exposition formats docs](https://prometheus.io/docs/instrumenting/exposition_formats/)
- Add real histogram emission in the PHP demo (`_bucket`, `_sum`, `_count`) with stable bucket boundaries.
- Track feature parity with Python multiprocess mode where it makes sense (modes/lifecycle/operational guidance): [client_python multiprocess docs](https://github.com/prometheus/client_python/blob/master/docs/content/multiprocess/_index.md)
- Make GC more robust to PID reuse by extending filename identity (for example PID + worker start marker).
- Evaluate optional writer-ownership locking for file claim/allocation (Ruby-style exclusive lock) to safely support surrogate/shared worker identifiers.
- Evaluate a PHP-FPM worker identity strategy (no stable worker-slot ID exposed): use `(pid,start-time)` and/or explicit slot mapping when needed.
- Add an interoperability test harness against a local Prometheus instance/repo scrape to validate end-to-end ingestion/render behavior.
- Evaluate extracting a standalone multiprocess Prometheus mmap core/toolkit (shared format + merge/GC), with thin PHP/Ruby bindings.
- Evaluate a separate exporter daemon for mmap read/merge/render, so app workers only write metrics.
- Add explicit persistent-handle lifecycle APIs (e.g. `close_all` / `reopen(path)`) to avoid requiring full php-fpm restart after manual file cleanup.

## Tool versions

- `ext-php-rs = 0.15.6`
- `cargo-php = 0.1.17`

Install cargo-php:

```bash
cargo install cargo-php --version 0.1.17
```

## Build and install extension

```bash
cargo php install --release
```

For `php-fpm`, the only extra step vs normal install is reloading/restarting the fpm service so workers pick up the new extension:

```bash
brew services restart php
# or your system-specific php-fpm reload command
```

Generate stubs:

```bash
cargo php stubs --stdout
```

## Exported PHP API

- `new PrometheusMmapStore(string $path)`
- `PrometheusMmapStore::increment(string $family, string $sample, string $labels, float $by = 1.0): float`
- `PrometheusMmapStore::set(string $family, string $sample, string $labels, float $value): float`
- `PrometheusMmapStore::get(string $family, string $sample, string $labels): float`
- `PrometheusMmapStore::flush(): void`
- `prometheus_mmap_render_dir(string $dir): string`
- `prometheus_mmap_gc_dir(string $dir, int $budgetMs = 10, int $scanLimit = 64, int $deleteLimit = 16, int $deadGraceSec = 600): int`

`prometheus_mmap_gc_dir(...)` performs best-effort stale `*.db` cleanup:
- uses a non-blocking lock (`.gc.lock`) so only one process runs GC work at a time
- advances a cursor file (`.gc.cursor`) to amortize directory scanning across calls
- deletes only files with numeric PID names that are not alive and older than `deadGraceSec`

## Example usage

Low-level extension API:

```php
<?php
$metricsDir = '/tmp/prometheus-mmap';
if (!is_dir($metricsDir)) {
  mkdir($metricsDir, 0777, true);
}

$store = new PrometheusMmapStore($metricsDir . '/counter_' . getmypid() . '-0.db');
$labels = '{method="GET"}';

$store->increment('http_requests_total', 'http_requests_total', $labels, 2.0);
$store->increment('http_requests_total', 'http_requests_total', $labels, 3.0);
$store->flush();

echo prometheus_mmap_render_dir($metricsDir);
```

For a small userspace wiring layer, see `examples/lib.php` and `examples/minimal_app.php`.
