# prometheus-client-php-mmap

A sloppy Rust `ext-php-rs` extension inspired by [GitLab's
`prometheus-client-mmap` Ruby gem](https://gitlab.com/gitlab-org/ruby/gems/prometheus-client-mmap).

## Features

- mmap-backed metric storage (`.db` files), compatible entry structure:
  - 4-byte `used` header (+4 bytes pad)
  - repeated entries: `[u32 key_len][key_json][space padding][f64 value]`
- metric operations: `increment`, `set`, `get`, `flush`
- multiprocess merge and render to Prometheus text format
- native PHP extension API via [`ext-php-rs`](https://github.com/extphprs/ext-php-rs)

## Consistency Model

- **One-writer-per-file assumption**: each metric file is written by a single worker process.
- **Publication order**: entry bytes are written first, then `used` is advanced. A `Release` fence is used before writing `used` so readers do not observe an advanced boundary before entry bytes are visible on weakly ordered CPUs.
- **Reader contract**: readers treat `used` as parse boundary and aggregate by scanning `*.db` files in the metrics directory.
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
  See: https://prometheus.io/docs/instrumenting/content_negotiation/ and https://prometheus.io/docs/instrumenting/exposition_formats/
- Add stale `*.db` cleanup strategy (dead PID / age-based GC) to avoid merge slowdown and stale metrics from exited workers.
- Make scrape aggregation tolerant of partial/corrupt trailing entries (render valid prefix instead of failing whole scrape).
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
- `PrometheusMmapStore::increment(string $keyJson, float $by = 1.0): float`
- `PrometheusMmapStore::set(string $keyJson, float $value): float`
- `PrometheusMmapStore::get(string $keyJson): float`
- `PrometheusMmapStore::flush(): void`
- `prometheus_mmap_render_dir(string $dir): string`

## Example usage

```php
<?php
$metricsDir = '/tmp/prometheus-mmap';
if (!is_dir($metricsDir)) {
  mkdir($metricsDir, 0777, true);
}

$store = new PrometheusMmapStore($metricsDir . '/counter_' . getmypid() . '-0.db');
$key = json_encode(['http_requests_total', 'http_requests_total', ['method'], ['GET']]);

$store->increment($key, 2.0);
$store->increment($key, 3.0);
$store->flush();

echo prometheus_mmap_render_dir($metricsDir);
```
