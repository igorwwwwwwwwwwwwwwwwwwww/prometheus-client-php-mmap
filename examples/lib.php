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

final class PrometheusMmapRequestMetrics
{
    public function __construct(
        private string $method,
        private int $requestStartNs,
        private array $gcStart,
        private PrometheusMmapCounter $requestCounter,
        private PrometheusMmapCounter $requestDurationCounter,
        private PrometheusMmapGauge $requestMemoryPeakGauge,
        private PrometheusMmapCounter $gcRunsCounter,
        private PrometheusMmapCounter $gcCollectedCounter,
        private PrometheusMmapCounter $gcCollectorTimeCounter,
        private PrometheusMmapCounter $gcDestructorTimeCounter,
        private PrometheusMmapCounter $gcFreeTimeCounter,
        private PrometheusMmapGauge $inflightGauge,
    ) {
    }

    public function record(string $route): void
    {
        $finalCode = http_response_code();
        if (!is_int($finalCode) || $finalCode <= 0) {
            $finalCode = 200;
        }

        $labels = ['route' => $route, 'method' => $this->method, 'code' => (string) $finalCode];

        $this->requestCounter->inc($labels, 1.0);
        $this->requestDurationCounter->inc(
            $labels,
            (hrtime(true) - $this->requestStartNs) / 1_000_000_000,
        );
        $this->requestMemoryPeakGauge->set($labels, (float) memory_get_peak_usage(true));

        $gcEnd = gc_status();
        $this->gcRunsCounter->inc($labels, (float) max(0, ($gcEnd['runs'] ?? 0) - ($this->gcStart['runs'] ?? 0)));
        $this->gcCollectedCounter->inc($labels, (float) max(0, ($gcEnd['collected'] ?? 0) - ($this->gcStart['collected'] ?? 0)));
        $this->gcCollectorTimeCounter->inc($labels, max(0.0, ($gcEnd['collector_time'] ?? 0.0) - ($this->gcStart['collector_time'] ?? 0.0)));
        $this->gcDestructorTimeCounter->inc($labels, max(0.0, ($gcEnd['destructor_time'] ?? 0.0) - ($this->gcStart['destructor_time'] ?? 0.0)));
        $this->gcFreeTimeCounter->inc($labels, max(0.0, ($gcEnd['free_time'] ?? 0.0) - ($this->gcStart['free_time'] ?? 0.0)));
        $this->inflightGauge->set([], 0.0);
    }
}
