use std::{
    collections::{BTreeMap, VecDeque},
    error::Error,
    fmt,
};

/// Built-in operational measurements with stable export names and types.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Metric {
    LatencyMilliseconds,
    GpuTimeMilliseconds,
    QueueDepth,
    DroppedItems,
    CpuUtilizationPercent,
    GpuUtilizationPercent,
    DiskBytesPerSecond,
    NetworkBytesPerSecond,
}

impl Metric {
    pub const ALL: [Self; 8] = [
        Self::LatencyMilliseconds,
        Self::GpuTimeMilliseconds,
        Self::QueueDepth,
        Self::DroppedItems,
        Self::CpuUtilizationPercent,
        Self::GpuUtilizationPercent,
        Self::DiskBytesPerSecond,
        Self::NetworkBytesPerSecond,
    ];

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LatencyMilliseconds => "latency_ms",
            Self::GpuTimeMilliseconds => "gpu_time_ms",
            Self::QueueDepth => "queue_depth",
            Self::DroppedItems => "dropped_items",
            Self::CpuUtilizationPercent => "cpu_utilization_percent",
            Self::GpuUtilizationPercent => "gpu_utilization_percent",
            Self::DiskBytesPerSecond => "disk_bytes_per_second",
            Self::NetworkBytesPerSecond => "network_bytes_per_second",
        }
    }

    #[must_use]
    pub const fn kind(self) -> MetricKind {
        match self {
            Self::DroppedItems => MetricKind::Counter,
            Self::LatencyMilliseconds | Self::GpuTimeMilliseconds => MetricKind::Histogram,
            Self::QueueDepth
            | Self::CpuUtilizationPercent
            | Self::GpuUtilizationPercent
            | Self::DiskBytesPerSecond
            | Self::NetworkBytesPerSecond => MetricKind::Gauge,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

impl MetricKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Gauge => "gauge",
            Self::Histogram => "histogram",
        }
    }
}

/// One point in a retained metric window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricPoint {
    pub monotonic_millis: u64,
    pub value: f64,
}

/// Bounded retained points plus lifetime scalar aggregates.
#[derive(Clone, Debug)]
pub struct MetricSeries {
    kind: MetricKind,
    capacity: usize,
    points: VecDeque<MetricPoint>,
    dropped: u64,
    count: u64,
    floating_count: f64,
    mean: Option<f64>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    current: Option<f64>,
}

impl MetricSeries {
    const fn new(kind: MetricKind, capacity: usize) -> Self {
        Self {
            kind,
            capacity,
            points: VecDeque::new(),
            dropped: 0,
            count: 0,
            floating_count: 0.0,
            mean: None,
            minimum: None,
            maximum: None,
            current: None,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> MetricKind {
        self.kind
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.points.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &MetricPoint> {
        self.points.iter()
    }

    #[must_use]
    pub fn counter_summary(&self) -> Option<CounterSummary> {
        (self.kind == MetricKind::Counter).then_some(CounterSummary {
            total: self.current.unwrap_or(0.0),
            updates: self.count,
            retained_samples: self.points.len(),
            dropped_samples: self.dropped,
        })
    }

    #[must_use]
    pub fn gauge_summary(&self) -> Option<GaugeSummary> {
        (self.kind == MetricKind::Gauge).then_some(GaugeSummary {
            current: self.current,
            minimum: self.minimum,
            maximum: self.maximum,
            samples: self.count,
            retained_samples: self.points.len(),
            dropped_samples: self.dropped,
        })
    }

    /// Summarizes lifetime extrema/mean and exact retained-window percentiles.
    #[must_use]
    pub fn histogram_summary(&self) -> Option<HistogramSummary> {
        if self.kind != MetricKind::Histogram {
            return None;
        }
        Some(HistogramSummary {
            count: self.count,
            retained_samples: self.points.len(),
            dropped_samples: self.dropped,
            minimum: self.minimum,
            maximum: self.maximum,
            mean: self.mean,
            p50: self.percentile(50),
            p95: self.percentile(95),
            p99: self.percentile(99),
        })
    }

    /// Returns a nearest-rank percentile over retained histogram samples.
    #[must_use]
    pub fn percentile(&self, percentile: u8) -> Option<f64> {
        if self.kind != MetricKind::Histogram || self.points.is_empty() || percentile > 100 {
            return None;
        }
        let mut values: Vec<_> = self.points.iter().map(|point| point.value).collect();
        values.sort_unstable_by(f64::total_cmp);
        let rank = if percentile == 0 {
            0
        } else {
            values
                .len()
                .saturating_mul(usize::from(percentile))
                .div_ceil(100)
                .saturating_sub(1)
        };
        values.get(rank).copied()
    }

    fn push(&mut self, point: MetricPoint) {
        self.count = self.count.saturating_add(1);
        self.floating_count += 1.0;
        self.mean = Some(self.mean.map_or(point.value, |mean| {
            mean + (point.value - mean) / self.floating_count
        }));
        self.minimum = Some(
            self.minimum
                .map_or(point.value, |value| value.min(point.value)),
        );
        self.maximum = Some(
            self.maximum
                .map_or(point.value, |value| value.max(point.value)),
        );
        self.current = Some(point.value);
        if self.capacity == 0 {
            self.dropped = self.dropped.saturating_add(1);
            return;
        }
        if self.points.len() == self.capacity {
            self.points.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.points.push_back(point);
    }

    fn last_time(&self) -> Option<u64> {
        self.points.back().map(|point| point.monotonic_millis)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CounterSummary {
    pub total: f64,
    pub updates: u64,
    pub retained_samples: usize,
    pub dropped_samples: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GaugeSummary {
    pub current: Option<f64>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub samples: u64,
    pub retained_samples: usize,
    pub dropped_samples: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HistogramSummary {
    pub count: u64,
    pub retained_samples: usize,
    pub dropped_samples: u64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub mean: Option<f64>,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub p99: Option<f64>,
}

/// Explicitly owned collection of bounded operational metric series.
#[derive(Clone, Debug)]
pub struct MetricStore {
    series: BTreeMap<Metric, MetricSeries>,
}

impl MetricStore {
    #[must_use]
    pub fn new(capacity_per_series: usize) -> Self {
        Self {
            series: Metric::ALL
                .into_iter()
                .map(|metric| {
                    (
                        metric,
                        MetricSeries::new(metric.kind(), capacity_per_series),
                    )
                })
                .collect(),
        }
    }

    /// Adds a non-negative amount and stores the resulting counter value.
    ///
    /// # Errors
    ///
    /// Rejects the wrong metric kind, non-finite/negative amounts, and times
    /// older than the last retained point.
    pub fn increment_counter(
        &mut self,
        metric: Metric,
        monotonic_millis: u64,
        amount: f64,
    ) -> Result<(), MetricError> {
        if !amount.is_finite() || amount < 0.0 {
            return Err(MetricError::InvalidValue);
        }
        let series = self.series_mut(metric, MetricKind::Counter, monotonic_millis)?;
        let value = series.current.unwrap_or(0.0) + amount;
        if !value.is_finite() {
            return Err(MetricError::InvalidValue);
        }
        series.push(MetricPoint {
            monotonic_millis,
            value,
        });
        Ok(())
    }

    /// Sets a finite gauge value.
    ///
    /// # Errors
    ///
    /// Rejects the wrong metric kind, non-finite values, and out-of-order time.
    pub fn set_gauge(
        &mut self,
        metric: Metric,
        monotonic_millis: u64,
        value: f64,
    ) -> Result<(), MetricError> {
        self.record(metric, MetricKind::Gauge, monotonic_millis, value)
    }

    /// Adds a finite histogram observation.
    ///
    /// # Errors
    ///
    /// Rejects the wrong metric kind, non-finite values, and out-of-order time.
    pub fn observe_histogram(
        &mut self,
        metric: Metric,
        monotonic_millis: u64,
        value: f64,
    ) -> Result<(), MetricError> {
        self.record(metric, MetricKind::Histogram, monotonic_millis, value)
    }

    /// Returns the always-present series for a built-in metric.
    ///
    /// # Panics
    ///
    /// Panics only if the store's private built-in metric invariant is broken.
    #[must_use]
    pub fn series(&self, metric: Metric) -> &MetricSeries {
        self.series
            .get(&metric)
            .expect("all built-in metrics exist")
    }

    #[must_use]
    pub fn iter(&self) -> impl ExactSizeIterator<Item = (Metric, &MetricSeries)> {
        self.series.iter().map(|(metric, series)| (*metric, series))
    }

    #[must_use]
    pub fn dropped_samples(&self) -> u64 {
        self.series
            .values()
            .fold(0_u64, |total, series| total.saturating_add(series.dropped))
    }

    fn record(
        &mut self,
        metric: Metric,
        expected: MetricKind,
        monotonic_millis: u64,
        value: f64,
    ) -> Result<(), MetricError> {
        if !value.is_finite() {
            return Err(MetricError::InvalidValue);
        }
        let series = self.series_mut(metric, expected, monotonic_millis)?;
        series.push(MetricPoint {
            monotonic_millis,
            value,
        });
        Ok(())
    }

    fn series_mut(
        &mut self,
        metric: Metric,
        expected: MetricKind,
        monotonic_millis: u64,
    ) -> Result<&mut MetricSeries, MetricError> {
        let series = self
            .series
            .get_mut(&metric)
            .expect("all built-in metrics exist");
        if series.kind != expected {
            return Err(MetricError::WrongKind {
                metric,
                expected,
                actual: series.kind,
            });
        }
        if series
            .last_time()
            .is_some_and(|last| monotonic_millis < last)
        {
            return Err(MetricError::OutOfOrder);
        }
        Ok(series)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetricError {
    WrongKind {
        metric: Metric,
        expected: MetricKind,
        actual: MetricKind,
    },
    InvalidValue,
    OutOfOrder,
}

impl fmt::Display for MetricError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongKind {
                metric,
                expected,
                actual,
            } => write!(
                formatter,
                "metric {} is {}, not {}",
                metric.as_str(),
                actual.as_str(),
                expected.as_str()
            ),
            Self::InvalidValue => formatter.write_str("metric value must be finite and valid"),
            Self::OutOfOrder => formatter.write_str("metric time is older than the last sample"),
        }
    }
}

impl Error for MetricError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_series_are_bounded_and_count_drops() {
        let mut metrics = MetricStore::new(2);
        for (time, value) in [(1, 1.0), (2, 2.0), (3, 3.0), (4, 4.0)] {
            metrics.set_gauge(Metric::QueueDepth, time, value).unwrap();
        }
        let series = metrics.series(Metric::QueueDepth);
        assert_eq!(series.len(), 2);
        assert_eq!(series.dropped(), 2);
        assert_eq!(
            series.iter().map(|point| point.value).collect::<Vec<_>>(),
            [3.0, 4.0]
        );
    }

    #[test]
    fn histogram_percentiles_use_the_retained_window() {
        let mut metrics = MetricStore::new(100);
        let mut sample = 1.0;
        for time in 1..=100 {
            metrics
                .observe_histogram(Metric::LatencyMilliseconds, time, sample)
                .unwrap();
            sample += 1.0;
        }
        let summary = metrics
            .series(Metric::LatencyMilliseconds)
            .histogram_summary()
            .unwrap();
        assert_eq!(summary.p50, Some(50.0));
        assert_eq!(summary.p95, Some(95.0));
        assert_eq!(summary.p99, Some(99.0));
        assert_eq!(summary.mean, Some(50.5));
    }

    #[test]
    fn gpu_time_is_a_distinct_histogram_with_a_stable_name() {
        assert_eq!(Metric::GpuTimeMilliseconds.as_str(), "gpu_time_ms");
        assert_eq!(Metric::GpuTimeMilliseconds.kind(), MetricKind::Histogram);
        let mut metrics = MetricStore::new(2);
        metrics
            .observe_histogram(Metric::GpuTimeMilliseconds, 1, 0.25)
            .unwrap();
        assert_eq!(
            metrics
                .series(Metric::GpuTimeMilliseconds)
                .histogram_summary()
                .unwrap()
                .p50,
            Some(0.25)
        );
    }

    #[test]
    fn counter_retains_total_when_points_are_evicted() {
        let mut metrics = MetricStore::new(1);
        metrics
            .increment_counter(Metric::DroppedItems, 1, 2.0)
            .unwrap();
        metrics
            .increment_counter(Metric::DroppedItems, 2, 3.0)
            .unwrap();
        let summary = metrics
            .series(Metric::DroppedItems)
            .counter_summary()
            .unwrap();
        assert!((summary.total - 5.0).abs() < f64::EPSILON);
        assert_eq!(summary.dropped_samples, 1);
    }
}
