<?php

declare(strict_types=1);

$requestStartNs = hrtime(true);

require_once __DIR__ . '/lib.php';

$metricsDir = __DIR__ . '/metrics';
$registry = new PrometheusMmapRegistry($metricsDir);

$requestCounter = $registry->counter('http_requests_total', ['code', 'method', 'route']);
$requestDurationHistogram = $registry->histogram(
    'http_request_duration_seconds',
    ['code', 'method', 'route'],
);
$requestMemoryPeakGauge = $registry->gauge(
    'php_request_memory_peak_bytes',
    ['code', 'method', 'route'],
    'max',
);
$gcRunsCounter = $registry->counter('php_gc_runs_total', ['code', 'method', 'route']);
$gcCollectedCounter = $registry->counter('php_gc_collected_total', ['code', 'method', 'route']);
$gcCollectorTimeCounter = $registry->counter(
    'php_gc_collector_time_seconds',
    ['code', 'method', 'route'],
    'php_gc_collector_time_seconds_total',
);
$gcDestructorTimeCounter = $registry->counter(
    'php_gc_destructor_time_seconds',
    ['code', 'method', 'route'],
    'php_gc_destructor_time_seconds_total',
);
$gcFreeTimeCounter = $registry->counter(
    'php_gc_free_time_seconds',
    ['code', 'method', 'route'],
    'php_gc_free_time_seconds_total',
);
$helloCounter = $registry->counter('demo_hello_requests_total');
$sleepCounter = $registry->counter('demo_sleep_requests_total');

// Gauge files demonstrate multiprocess modes:
// - all: keep one series per worker (pid label retained)
// - livesum: sum worker values into one series (pid not retained)
$inflightGauge = $registry->gauge('demo_inflight_requests', [], 'all');
$workersAliveGauge = $registry->gauge('demo_workers_alive', [], 'livesum');

$path = parse_url($_SERVER['REQUEST_URI'] ?? '/', PHP_URL_PATH) ?: '/';

$requestMetrics = new PrometheusMmapRequestMetrics(
    strtolower($_SERVER['REQUEST_METHOD'] ?? 'get'),
    $requestStartNs,
    gc_status(),
    $requestCounter,
    $requestDurationHistogram,
    $requestMemoryPeakGauge,
    $gcRunsCounter,
    $gcCollectedCounter,
    $gcCollectorTimeCounter,
    $gcDestructorTimeCounter,
    $gcFreeTimeCounter,
    $inflightGauge,
);

register_shutdown_function(static function () use ($requestMetrics, &$route): void {
    if (function_exists('fastcgi_finish_request')) {
        fastcgi_finish_request();
    }
    $requestMetrics->record($route);
});

$workersAliveGauge->set([], 1.0);
$inflightGauge->set([], 1.0);

if ($path === '/') {
    $route = '/';
    http_response_code(200);
    header('Content-Type: text/plain; charset=utf-8');
    echo "ok\n";
    exit;
}

if ($path === '/hello') {
    $route = '/hello';
    http_response_code(200);
    header('Content-Type: text/plain; charset=utf-8');
    echo "hello\n";
    $helloCounter->inc([], 1.0);
    exit;
}

if ($path === '/phpinfo') {
    $route = '/phpinfo';
    http_response_code(200);
    phpinfo();
    exit;
}

if ($path === '/sleep') {
    $route = '/sleep';
    http_response_code(200);
    header('Content-Type: text/plain; charset=utf-8');
    header('X-Accel-Buffering: no');
    while (ob_get_level() > 0) {
        ob_end_flush();
    }
    echo "sleeping...\n";
    flush();
    sleep(5);
    echo "done\n";
    $sleepCounter->inc([], 1.0);
    exit;
}

if ($path === '/metrics') {
    $route = '/metrics';
    register_shutdown_function(static function () use ($registry): void {
        if (function_exists('fastcgi_finish_request')) {
            fastcgi_finish_request();
        }
        $budgetMs = (int) ($_ENV['PMMAP_GC_BUDGET_MS'] ?? getenv('PMMAP_GC_BUDGET_MS') ?: '10');
        $scanLimit = (int) ($_ENV['PMMAP_GC_SCAN_LIMIT'] ?? getenv('PMMAP_GC_SCAN_LIMIT') ?: '64');
        $deleteLimit = (int) ($_ENV['PMMAP_GC_DELETE_LIMIT'] ?? getenv('PMMAP_GC_DELETE_LIMIT') ?: '16');
        $deadGraceSec = (int) ($_ENV['PMMAP_GC_DEAD_GRACE_SEC'] ?? getenv('PMMAP_GC_DEAD_GRACE_SEC') ?: '600');
        $registry->gc($budgetMs, $scanLimit, $deleteLimit, $deadGraceSec);
    });
    http_response_code(200);
    header('Content-Type: text/plain; version=0.0.4; charset=utf-8');
    echo $registry->render();
    exit;
}

$route = 'unmatched';
http_response_code(404);
header('Content-Type: text/plain; charset=utf-8');
echo "not found\n";
