<?php

declare(strict_types=1);

$metricsDir = __DIR__ . '/metrics';
if (!is_dir($metricsDir)) {
    mkdir($metricsDir, 0777, true);
}

// Ruby-style filename layout: <type>_<pid>-<n>.db
$counterDbPath = $metricsDir . '/counter_' . getmypid() . '-0.db';
$counterStore = new PrometheusMmapStore($counterDbPath);

// Gauge files demonstrate multiprocess modes:
// - all: keep one series per worker (pid label retained)
// - livesum: sum worker values into one series (pid not retained)
$gaugeAllDbPath = $metricsDir . '/gauge_all_' . getmypid() . '-0.db';
$gaugeAllStore = new PrometheusMmapStore($gaugeAllDbPath);

$gaugeLivesumDbPath = $metricsDir . '/gauge_livesum_' . getmypid() . '-0.db';
$gaugeLivesumStore = new PrometheusMmapStore($gaugeLivesumDbPath);

$gaugeMaxDbPath = $metricsDir . '/gauge_max_' . getmypid() . '-0.db';
$gaugeMaxStore = new PrometheusMmapStore($gaugeMaxDbPath);

$path = parse_url($_SERVER['REQUEST_URI'] ?? '/', PHP_URL_PATH) ?: '/';
$method = strtolower($_SERVER['REQUEST_METHOD'] ?? 'get');
$requestStart = microtime(true);
$gcStart = gc_status();

function escapeLabelValue(string $value): string
{
    return str_replace(
        ["\\", "\n", '"'],
        ["\\\\", "\\n", '\\"'],
        $value,
    );
}

function metricLabels(array $labels = []): string
{
    if ($labels === []) {
        return '';
    }

    ksort($labels);
    $parts = [];
    foreach ($labels as $label => $value) {
        $parts[] = $label . '="' . escapeLabelValue((string) $value) . '"';
    }
    return '{' . implode(',', $parts) . '}';
}

register_shutdown_function(static function () use (
    $counterStore,
    $gaugeAllStore,
    $gaugeLivesumStore,
    $gaugeMaxStore,
    $metricsDir,
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

    $counterStore->increment(
        'http_requests_total',
        'http_requests_total',
        metricLabels($labels),
        1.0,
    );

    $duration = microtime(true) - $requestStart;
    $counterStore->increment(
        'http_request_duration_seconds',
        'http_request_duration_seconds_total',
        metricLabels($labels),
        $duration,
    );

    $peakBytes = (float) memory_get_peak_usage(true);
    $gaugeMaxStore->set(
        'php_request_memory_peak_bytes',
        'php_request_memory_peak_bytes',
        metricLabels($labels),
        $peakBytes,
    );

    $gcEnd = gc_status();
    $deltaRuns = max(0, ($gcEnd['runs'] ?? 0) - ($gcStart['runs'] ?? 0));
    $deltaCollected = max(0, ($gcEnd['collected'] ?? 0) - ($gcStart['collected'] ?? 0));
    $deltaCollectorTime = max(0.0, ($gcEnd['collector_time'] ?? 0.0) - ($gcStart['collector_time'] ?? 0.0));
    $deltaDestructorTime = max(0.0, ($gcEnd['destructor_time'] ?? 0.0) - ($gcStart['destructor_time'] ?? 0.0));
    $deltaFreeTime = max(0.0, ($gcEnd['free_time'] ?? 0.0) - ($gcStart['free_time'] ?? 0.0));

    $counterStore->increment(
        'php_gc_runs_total',
        'php_gc_runs_total',
        metricLabels($labels),
        (float) $deltaRuns,
    );
    $counterStore->increment(
        'php_gc_collected_total',
        'php_gc_collected_total',
        metricLabels($labels),
        (float) $deltaCollected,
    );
    $counterStore->increment(
        'php_gc_collector_time_seconds',
        'php_gc_collector_time_seconds_total',
        metricLabels($labels),
        $deltaCollectorTime,
    );
    $counterStore->increment(
        'php_gc_destructor_time_seconds',
        'php_gc_destructor_time_seconds_total',
        metricLabels($labels),
        $deltaDestructorTime,
    );
    $counterStore->increment(
        'php_gc_free_time_seconds',
        'php_gc_free_time_seconds_total',
        metricLabels($labels),
        $deltaFreeTime,
    );

    $gaugeAllStore->set(
        'demo_inflight_requests',
        'demo_inflight_requests',
        '',
        0.0,
    );

});

$gaugeLivesumStore->set(
    'demo_workers_alive',
    'demo_workers_alive',
    '',
    1.0,
);

$gaugeAllStore->set(
    'demo_inflight_requests',
    'demo_inflight_requests',
    '',
    1.0,
);

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
    $counterStore->increment(
        'demo_hello_requests_total',
        'demo_hello_requests_total',
        '',
        1.0,
    );
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
    $counterStore->increment(
        'demo_sleep_requests_total',
        'demo_sleep_requests_total',
        '',
        1.0,
    );
    exit;
}

if ($path === '/metrics') {
    $route = '/metrics';
    register_shutdown_function(static function () use ($metricsDir): void {
        if (function_exists('fastcgi_finish_request')) {
            fastcgi_finish_request();
        }
        $budgetMs = (int) ($_ENV['PMMAP_GC_BUDGET_MS'] ?? getenv('PMMAP_GC_BUDGET_MS') ?: '10');
        $scanLimit = (int) ($_ENV['PMMAP_GC_SCAN_LIMIT'] ?? getenv('PMMAP_GC_SCAN_LIMIT') ?: '64');
        $deleteLimit = (int) ($_ENV['PMMAP_GC_DELETE_LIMIT'] ?? getenv('PMMAP_GC_DELETE_LIMIT') ?: '16');
        $deadGraceSec = (int) ($_ENV['PMMAP_GC_DEAD_GRACE_SEC'] ?? getenv('PMMAP_GC_DEAD_GRACE_SEC') ?: '600');
        prometheus_mmap_gc_dir($metricsDir, $budgetMs, $scanLimit, $deleteLimit, $deadGraceSec);
    });
    http_response_code(200);
    header('Content-Type: text/plain; version=0.0.4; charset=utf-8');
    echo prometheus_mmap_render_dir($metricsDir);
    exit;
}

$route = 'unmatched';
http_response_code(404);
header('Content-Type: text/plain; charset=utf-8');
echo "not found\n";
