<?php

$metricsDir = sys_get_temp_dir() . '/php_prom_mmap_demo';
if (!is_dir($metricsDir)) {
    mkdir($metricsDir, 0777, true);
}

$dbPath = $metricsDir . '/counter_' . getmypid() . '-0.db';
$store = new PrometheusMmapStore($dbPath);

$key = json_encode(
    ['http_requests_total', 'http_requests_total', ['method'], ['GET']],
    JSON_THROW_ON_ERROR
);

$store->increment($key, 2.0);
$store->increment($key, 3.0);
$store->flush();

echo prometheus_mmap_render_dir($metricsDir);
