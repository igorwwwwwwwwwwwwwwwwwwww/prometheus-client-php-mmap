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

$metricKey = static function (string $family, string $name, array $labels = []): string {
    $labelNames = array_keys($labels);
    sort($labelNames);
    $labelValues = [];
    foreach ($labelNames as $label) {
        $labelValues[] = (string) $labels[$label];
    }
    return json_encode(
        [$family, $name, $labelNames, $labelValues],
        JSON_THROW_ON_ERROR | JSON_UNESCAPED_SLASHES,
    );
};

register_shutdown_function(static function () use (
    $counterStore,
    $gaugeAllStore,
    $gaugeLivesumStore,
    $gaugeMaxStore,
    $metricsDir,
    $metricKey,
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
        $metricKey('http_requests_total', 'http_requests_total', $labels),
        1.0,
    );

    $duration = microtime(true) - $requestStart;
    $counterStore->increment(
        $metricKey('http_request_duration_seconds', 'http_request_duration_seconds_total', $labels),
        $duration,
    );

    $peakBytes = (float) memory_get_peak_usage(true);
    $gaugeMaxStore->set(
        $metricKey('php_request_memory_peak_bytes', 'php_request_memory_peak_bytes', $labels),
        $peakBytes,
    );

    $gcEnd = gc_status();
    $deltaRuns = max(0, ($gcEnd['runs'] ?? 0) - ($gcStart['runs'] ?? 0));
    $deltaCollected = max(0, ($gcEnd['collected'] ?? 0) - ($gcStart['collected'] ?? 0));
    $deltaCollectorTime = max(0.0, ($gcEnd['collector_time'] ?? 0.0) - ($gcStart['collector_time'] ?? 0.0));
    $deltaDestructorTime = max(0.0, ($gcEnd['destructor_time'] ?? 0.0) - ($gcStart['destructor_time'] ?? 0.0));
    $deltaFreeTime = max(0.0, ($gcEnd['free_time'] ?? 0.0) - ($gcStart['free_time'] ?? 0.0));

    $counterStore->increment(
        $metricKey('php_gc_runs_total', 'php_gc_runs_total', $labels),
        (float) $deltaRuns,
    );
    $counterStore->increment(
        $metricKey('php_gc_collected_total', 'php_gc_collected_total', $labels),
        (float) $deltaCollected,
    );
    $counterStore->increment(
        $metricKey('php_gc_collector_time_seconds', 'php_gc_collector_time_seconds_total', $labels),
        $deltaCollectorTime,
    );
    $counterStore->increment(
        $metricKey(
            'php_gc_destructor_time_seconds',
            'php_gc_destructor_time_seconds_total',
            $labels,
        ),
        $deltaDestructorTime,
    );
    $counterStore->increment(
        $metricKey('php_gc_free_time_seconds', 'php_gc_free_time_seconds_total', $labels),
        $deltaFreeTime,
    );

    $gaugeAllStore->set(
        $metricKey('demo_inflight_requests', 'demo_inflight_requests'),
        0.0,
    );

});

$gaugeLivesumStore->set(
    $metricKey('demo_workers_alive', 'demo_workers_alive'),
    1.0,
);

$gaugeAllStore->set(
    $metricKey('demo_inflight_requests', 'demo_inflight_requests'),
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
        $metricKey('demo_hello_requests_total', 'demo_hello_requests_total'),
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
        $metricKey('demo_sleep_requests_total', 'demo_sleep_requests_total'),
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
