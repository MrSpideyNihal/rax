//! Latency summaries for benchmark samples.

use serde::Serialize;

/// Distribution of a latency sample, in nanoseconds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    /// Number of samples.
    pub count: usize,
    /// Arithmetic mean.
    pub mean_ns: f64,
    /// Population standard deviation.
    pub stddev_ns: f64,
    /// Smallest sample.
    pub min_ns: u32,
    /// Median.
    pub p50_ns: u32,
    /// 99th percentile.
    pub p99_ns: u32,
    /// 99.9th percentile.
    pub p999_ns: u32,
    /// Largest sample.
    pub max_ns: u32,
}

/// Summarises `samples`, sorting them in place. Returns `None` if empty.
#[must_use]
pub fn summarize(samples: &mut [u32]) -> Option<Summary> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let count = samples.len();
    let mean = samples.iter().map(|&s| f64::from(s)).sum::<f64>() / count as f64;
    let variance = samples
        .iter()
        .map(|&s| (f64::from(s) - mean).powi(2))
        .sum::<f64>()
        / count as f64;
    Some(Summary {
        count,
        mean_ns: mean,
        stddev_ns: variance.sqrt(),
        min_ns: samples[0],
        p50_ns: quantile(samples, 500),
        p99_ns: quantile(samples, 990),
        p999_ns: quantile(samples, 999),
        max_ns: samples[count - 1],
    })
}

/// Nearest-rank quantile of an ascending, non-empty slice, in parts per
/// thousand. Integer arithmetic keeps the rank exact: in `f64`, 99.9% of 1000
/// rounds up to the wrong sample.
fn quantile(sorted: &[u32], per_mille: usize) -> u32 {
    let rank = (per_mille * sorted.len()).div_ceil(1000);
    sorted[rank.clamp(1, sorted.len()) - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_has_no_summary() {
        assert_eq!(summarize(&mut []), None);
    }

    #[test]
    fn single_sample_is_every_percentile() {
        let summary = summarize(&mut [42]).unwrap();
        assert_eq!(
            (
                summary.min_ns,
                summary.p50_ns,
                summary.p999_ns,
                summary.max_ns
            ),
            (42, 42, 42, 42)
        );
        assert_eq!(summary.stddev_ns, 0.0);
    }

    #[test]
    fn uses_nearest_rank_percentiles_on_unsorted_input() {
        let mut samples: Vec<u32> = (1..=1000).rev().collect();
        let summary = summarize(&mut samples).unwrap();
        assert_eq!(summary.p50_ns, 500);
        assert_eq!(summary.p99_ns, 990);
        assert_eq!(summary.p999_ns, 999);
        assert_eq!(summary.max_ns, 1000);
        assert_eq!(summary.mean_ns, 500.5);
    }

    #[test]
    fn computes_population_stddev() {
        let summary = summarize(&mut [2, 4, 4, 4, 5, 5, 7, 9]).unwrap();
        assert_eq!(summary.stddev_ns, 2.0);
    }
}
