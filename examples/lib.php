<?php

declare(strict_types=1);

function prometheus_mmap_escape_label_value(string $value): string
{
    return str_replace(
        ["\\", "\n", '"'],
        ["\\\\", "\\n", '\\"'],
        $value,
    );
}

function prometheus_mmap_metric_labels(array $labels = []): string
{
    if ($labels === []) {
        return '';
    }

    ksort($labels);
    $parts = [];
    foreach ($labels as $label => $value) {
        $parts[] = $label . '="' . prometheus_mmap_escape_label_value((string) $value) . '"';
    }
    return '{' . implode(',', $parts) . '}';
}

function prometheus_mmap_format_bucket_bound(float $bound): string
{
    if (is_infinite($bound) && $bound > 0) {
        return '+Inf';
    }
    return sprintf('%.15g', $bound);
}

final class PrometheusMmapRegistry
{
    private string $dir;
    private string $workerId;

    /** @var array<string, PrometheusMmapStore> */
    private array $stores = [];

    public function __construct(string $dir, string|int|null $workerId = null)
    {
        $this->dir = $dir;
        $this->workerId = (string) ($workerId ?? getmypid());

        if (!is_dir($dir)) {
            mkdir($dir, 0777, true);
        }
    }

    public function counter(string $family, array $labelNames = [], ?string $sample = null): PrometheusMmapCounter
    {
        return new PrometheusMmapCounter(
            $this->store('counter_' . $this->workerId . '-0.db'),
            $family,
            $sample ?? $family,
            $labelNames,
        );
    }

    public function gauge(
        string $family,
        array $labelNames = [],
        string $mode = 'all',
        ?string $sample = null,
    ): PrometheusMmapGauge {
        if (!in_array($mode, ['all', 'liveall', 'livesum', 'max', 'min'], true)) {
            throw new InvalidArgumentException('invalid gauge mode: ' . $mode);
        }

        return new PrometheusMmapGauge(
            $this->store('gauge_' . $mode . '_' . $this->workerId . '-0.db'),
            $family,
            $sample ?? $family,
            $labelNames,
        );
    }

    public function histogram(
        string $family,
        array $labelNames = [],
        ?array $buckets = null,
    ): PrometheusMmapHistogram {
        return new PrometheusMmapHistogram(
            $this->store('histogram_' . $this->workerId . '-0.db'),
            $family,
            $labelNames,
            $buckets ?? PrometheusMmapHistogram::defaultBuckets(),
        );
    }

    public function render(): string
    {
        return prometheus_mmap_render_dir($this->dir);
    }

    public function gc(
        int $budgetMs = 10,
        int $scanLimit = 64,
        int $deleteLimit = 16,
        int $deadGraceSec = 600,
    ): int {
        return prometheus_mmap_gc_dir(
            $this->dir,
            $budgetMs,
            $scanLimit,
            $deleteLimit,
            $deadGraceSec,
        );
    }

    private function store(string $file): PrometheusMmapStore
    {
        return $this->stores[$file] ??= new PrometheusMmapStore($this->dir . '/' . $file);
    }
}

abstract class PrometheusMmapMetric
{
    public function __construct(
        protected PrometheusMmapStore $store,
        protected string $family,
        protected string $sample,
        protected array $labelNames,
    ) {
        $this->assertMetricName($family);
        $this->assertMetricName($sample);
        foreach ($labelNames as $labelName) {
            $this->assertLabelName($labelName);
        }
    }

    protected function labels(array $labels): string
    {
        $expected = $this->labelNames;
        sort($expected);

        $actual = array_keys($labels);
        sort($actual);

        if ($expected !== $actual) {
            throw new InvalidArgumentException(sprintf(
                'labels for %s must be exactly [%s], got [%s]',
                $this->sample,
                implode(', ', $expected),
                implode(', ', $actual),
            ));
        }

        return prometheus_mmap_metric_labels($labels);
    }

    protected function assertMetricName(string $name): void
    {
        if (!preg_match('/^[a-zA-Z_:][a-zA-Z0-9_:]*$/', $name)) {
            throw new InvalidArgumentException('invalid metric name: ' . $name);
        }
    }

    protected function assertLabelName(string $name): void
    {
        if (!preg_match('/^[a-zA-Z_][a-zA-Z0-9_]*$/', $name)) {
            throw new InvalidArgumentException('invalid label name: ' . $name);
        }
    }
}

final class PrometheusMmapCounter extends PrometheusMmapMetric
{
    public function inc(array $labels = [], float $by = 1.0): float
    {
        return $this->store->increment($this->family, $this->sample, $this->labels($labels), $by);
    }
}

final class PrometheusMmapGauge extends PrometheusMmapMetric
{
    public function set(array $labels = [], float $value = 0.0): float
    {
        return $this->store->set($this->family, $this->sample, $this->labels($labels), $value);
    }

    public function inc(array $labels = [], float $by = 1.0): float
    {
        $encodedLabels = $this->labels($labels);
        $current = $this->store->get($this->family, $this->sample, $encodedLabels);
        return $this->store->set($this->family, $this->sample, $encodedLabels, $current + $by);
    }
}

final class PrometheusMmapHistogram
{
    private PrometheusMmapStore $store;
    private string $family;
    private array $labelNames;
    private array $buckets;

    public function __construct(PrometheusMmapStore $store, string $family, array $labelNames, array $buckets)
    {
        $this->store = $store;
        $this->family = $family;
        $this->labelNames = $labelNames;
        $this->buckets = self::normalizeBuckets($buckets);

        if (!preg_match('/^[a-zA-Z_:][a-zA-Z0-9_:]*$/', $family)) {
            throw new InvalidArgumentException('invalid metric name: ' . $family);
        }
        foreach ($labelNames as $labelName) {
            if (!preg_match('/^[a-zA-Z_][a-zA-Z0-9_]*$/', $labelName)) {
                throw new InvalidArgumentException('invalid label name: ' . $labelName);
            }
            if ($labelName === 'le') {
                throw new InvalidArgumentException('histogram label names must not include reserved label: le');
            }
        }
    }

    public static function defaultBuckets(): array
    {
        return [0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];
    }

    public function observe(array $labels = [], float $value = 0.0): void
    {
        $encodedLabels = $this->labels($labels);
        $this->store->increment($this->family, $this->family . '_sum', $encodedLabels, $value);
        $this->store->increment($this->family, $this->family . '_count', $encodedLabels, 1.0);

        foreach ($this->buckets as $upperBound) {
            if ($value <= $upperBound) {
                $this->store->increment(
                    $this->family,
                    $this->family . '_bucket',
                    $this->bucketLabels($labels, prometheus_mmap_format_bucket_bound($upperBound)),
                    1.0,
                );
            }
        }

        $this->store->increment(
            $this->family,
            $this->family . '_bucket',
            $this->bucketLabels($labels, '+Inf'),
            1.0,
        );
    }

    private function labels(array $labels): string
    {
        $expected = $this->labelNames;
        sort($expected);

        $actual = array_keys($labels);
        sort($actual);

        if ($expected !== $actual) {
            throw new InvalidArgumentException(sprintf(
                'labels for %s must be exactly [%s], got [%s]',
                $this->family,
                implode(', ', $expected),
                implode(', ', $actual),
            ));
        }

        return prometheus_mmap_metric_labels($labels);
    }

    private function bucketLabels(array $labels, string $le): string
    {
        $labels['le'] = $le;
        return prometheus_mmap_metric_labels($labels);
    }

    private static function normalizeBuckets(array $buckets): array
    {
        if ($buckets === []) {
            throw new InvalidArgumentException('histogram buckets must not be empty');
        }

        $normalized = [];
        foreach ($buckets as $bucket) {
            if (!is_int($bucket) && !is_float($bucket)) {
                throw new InvalidArgumentException('histogram buckets must be numeric');
            }
            $bucket = (float) $bucket;
            if (is_nan($bucket) || is_infinite($bucket)) {
                throw new InvalidArgumentException('histogram buckets must be finite');
            }
            $normalized[] = $bucket;
        }

        sort($normalized, SORT_NUMERIC);
        for ($i = 1, $n = count($normalized); $i < $n; $i++) {
            if ($normalized[$i - 1] >= $normalized[$i]) {
                throw new InvalidArgumentException('histogram buckets must be strictly increasing');
            }
        }

        return $normalized;
    }
}

final class PrometheusMmapRequestMetrics
{
    private PrometheusMmapCounter $requestCounter;
    private PrometheusMmapHistogram $requestDurationHistogram;
    private PrometheusMmapGauge $requestMemoryUsageGauge;
    private PrometheusMmapGauge $requestMemoryAllocatedGauge;
    private PrometheusMmapGauge $requestMemoryPeakGauge;
    private PrometheusMmapGauge $requestMemoryPeakAllocatedGauge;
    private PrometheusMmapCounter $gcRunsCounter;
    private PrometheusMmapCounter $gcCollectedCounter;
    private PrometheusMmapCounter $gcCollectorTimeCounter;
    private PrometheusMmapCounter $gcDestructorTimeCounter;
    private PrometheusMmapCounter $gcFreeTimeCounter;
    private PrometheusMmapGauge $inflightGauge;
    private PrometheusMmapGauge $workersAliveGauge;

    public function __construct(
        private PrometheusMmapRegistry $registry,
        private string $method,
        private int $requestStartNs,
        private array $gcStart,
    ) {
        $labels = ['code', 'method', 'route'];
        $this->requestCounter = $registry->counter('http_requests_total', $labels);
        $this->requestDurationHistogram = $registry->histogram('http_request_duration_seconds', $labels);
        $this->requestMemoryUsageGauge = $registry->gauge('php_request_memory_usage_bytes', $labels, 'max');
        $this->requestMemoryAllocatedGauge = $registry->gauge('php_request_memory_allocated_bytes', $labels, 'max');
        $this->requestMemoryPeakGauge = $registry->gauge('php_request_memory_peak_bytes', $labels, 'max');
        $this->requestMemoryPeakAllocatedGauge = $registry->gauge('php_request_memory_peak_allocated_bytes', $labels, 'max');
        $this->gcRunsCounter = $registry->counter('php_gc_runs_total', $labels);
        $this->gcCollectedCounter = $registry->counter('php_gc_collected_total', $labels);
        $this->gcCollectorTimeCounter = $registry->counter('php_gc_collector_time_seconds', $labels, 'php_gc_collector_time_seconds_total');
        $this->gcDestructorTimeCounter = $registry->counter('php_gc_destructor_time_seconds', $labels, 'php_gc_destructor_time_seconds_total');
        $this->gcFreeTimeCounter = $registry->counter('php_gc_free_time_seconds', $labels, 'php_gc_free_time_seconds_total');
        $this->inflightGauge = $registry->gauge('php_requests_in_flight', [], 'all');
        $this->workersAliveGauge = $registry->gauge('php_workers_alive', [], 'livesum');
    }

    public function requestStart(): void
    {
        $this->workersAliveGauge->set([], 1.0);
        $this->inflightGauge->set([], 1.0);
    }

    public function record(string $route): void
    {
        $finalCode = http_response_code();
        if (!is_int($finalCode) || $finalCode <= 0) {
            $finalCode = 200;
        }

        $labels = ['route' => $route, 'method' => $this->method, 'code' => (string) $finalCode];

        $this->requestCounter->inc($labels, 1.0);
        $this->requestDurationHistogram->observe(
            $labels,
            (hrtime(true) - $this->requestStartNs) / 1_000_000_000,
        );
        $this->requestMemoryUsageGauge->set($labels, (float) memory_get_usage(false));
        $this->requestMemoryAllocatedGauge->set($labels, (float) memory_get_usage(true));
        $this->requestMemoryPeakGauge->set($labels, (float) memory_get_peak_usage(false));
        $this->requestMemoryPeakAllocatedGauge->set($labels, (float) memory_get_peak_usage(true));

        $gcEnd = gc_status();
        $this->gcRunsCounter->inc($labels, (float) max(0, ($gcEnd['runs'] ?? 0) - ($this->gcStart['runs'] ?? 0)));
        $this->gcCollectedCounter->inc($labels, (float) max(0, ($gcEnd['collected'] ?? 0) - ($this->gcStart['collected'] ?? 0)));
        $this->gcCollectorTimeCounter->inc($labels, max(0.0, ($gcEnd['collector_time'] ?? 0.0) - ($this->gcStart['collector_time'] ?? 0.0)));
        $this->gcDestructorTimeCounter->inc($labels, max(0.0, ($gcEnd['destructor_time'] ?? 0.0) - ($this->gcStart['destructor_time'] ?? 0.0)));
        $this->gcFreeTimeCounter->inc($labels, max(0.0, ($gcEnd['free_time'] ?? 0.0) - ($this->gcStart['free_time'] ?? 0.0)));
        $this->inflightGauge->set([], 0.0);
    }
}
