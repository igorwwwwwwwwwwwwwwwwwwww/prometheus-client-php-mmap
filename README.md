# prometheus-client-php-mmap

A sloppy Rust `ext-php-rs` extension inspired by [GitLab's
`prometheus-client-mmap` Ruby gem](https://gitlab.com/gitlab-org/ruby/gems/prometheus-client-mmap).

## Features

- mmap-backed metric storage (`.db` files), compatible entry structure:
  - 4-byte `used` header (+4 bytes pad)
  - repeated entries: `[u32 key_len][key_json][space padding][f64 value]`
- metric operations: `increment`, `set`, `get`, `flush`
- multiprocess merge and render to Prometheus text format
- native PHP extension API via `ext-php-rs` (no PHP FFI required)

## TODO

- Evaluate implementing `PrometheusProto` (binary protobuf exposition format) in addition to text format.
  See: https://prometheus.io/docs/instrumenting/content_negotiation/ and https://prometheus.io/docs/instrumenting/exposition_formats/
- Add stale `*.db` cleanup strategy (dead PID / age-based GC) to avoid merge slowdown and stale metrics from exited workers.

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

When using persistent mmap handles in long-lived php-fpm workers, manual deletion of metric `*.db` files requires a php-fpm worker restart to re-establish mapped files.

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
