<?php

declare(strict_types=1);

require_once __DIR__ . '/lib.php';

$metricsDir = __DIR__ . '/metrics';
$registry = new PrometheusMmapRegistry($metricsDir);

$requestCounter = $registry->counter('http_requests_total', ['code', 'method', 'route']);
$requestDurationCounter = $registry->counter(
    'http_request_duration_seconds',
    ['code', 'method', 'route'],
    'http_request_duration_seconds_total',
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
$method = strtolower($_SERVER['REQUEST_METHOD'] ?? 'get');
$requestStart = microtime(true);
$gcStart = gc_status();

register_shutdown_function(static function () use (
    $requestCounter,
    $requestDurationCounter,
    $requestMemoryPeakGauge,
    $gcRunsCounter,
    $gcCollectedCounter,
    $gcCollectorTimeCounter,
    $gcDestructorTimeCounter,
    $gcFreeTimeCounter,
    $inflightGauge,
    &$route,
    $method,
    $requestStart,
    $gcStart,
): void {
    if (function_exists('fastcgi_finish_request')) {
        fastcgi_finish_request();
    }

    $finalCode = http_response_code();
    if (!is_int($finalCode) || $finalCode <= 0) {
        $finalCode = 200;
    }

    $labels = ['route' => $route, 'method' => $method, 'code' => (string) $finalCode];

    $requestCounter->inc($labels, 1.0);

    $duration = microtime(true) - $requestStart;
    $requestDurationCounter->inc($labels, $duration);

    $peakBytes = (float) memory_get_peak_usage(true);
    $requestMemoryPeakGauge->set($labels, $peakBytes);

    $gcEnd = gc_status();
    $deltaRuns = max(0, ($gcEnd['runs'] ?? 0) - ($gcStart['runs'] ?? 0));
    $deltaCollected = max(0, ($gcEnd['collected'] ?? 0) - ($gcStart['collected'] ?? 0));
    $deltaCollectorTime = max(0.0, ($gcEnd['collector_time'] ?? 0.0) - ($gcStart['collector_time'] ?? 0.0));
    $deltaDestructorTime = max(0.0, ($gcEnd['destructor_time'] ?? 0.0) - ($gcStart['destructor_time'] ?? 0.0));
    $deltaFreeTime = max(0.0, ($gcEnd['free_time'] ?? 0.0) - ($gcStart['free_time'] ?? 0.0));

    $gcRunsCounter->inc($labels, (float) $deltaRuns);
    $gcCollectedCounter->inc($labels, (float) $deltaCollected);
    $gcCollectorTimeCounter->inc($labels, $deltaCollectorTime);
    $gcDestructorTimeCounter->inc($labels, $deltaDestructorTime);
    $gcFreeTimeCounter->inc($labels, $deltaFreeTime);

    $inflightGauge->set([], 0.0);

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
