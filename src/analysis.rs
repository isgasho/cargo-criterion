use crate::connection::Throughput;
use crate::estimate::{build_estimates, ConfidenceInterval, Estimate};
use crate::estimate::{Distributions, Estimates, Statistic};
use crate::report::BenchmarkId;
use crate::report::MeasurementData;
use crate::stats::bivariate::regression::Slope;
use crate::stats::bivariate::Data;
use crate::stats::univariate::outliers::tukey;
use crate::stats::univariate::Sample;
use crate::stats::{Distribution, Tails};
use std::collections::BTreeMap;
use std::time::Duration;

macro_rules! elapsed {
    ($msg:expr, $block:expr) => {{
        let start = ::std::time::Instant::now();
        let out = $block;
        let elapsed = &start.elapsed();

        info!(
            "{} took {}",
            $msg,
            crate::format::time(crate::DurationExt::to_nanos(elapsed) as f64)
        );

        out
    }};
}

#[derive(Debug, Clone)]
pub struct BenchmarkConfig {
    pub confidence_level: f64,
    pub measurement_time: Duration,
    pub noise_threshold: f64,
    pub nresamples: usize,
    pub sample_size: usize,
    pub significance_level: f64,
    pub warm_up_time: Duration,
}

pub struct MeasuredValues<'a> {
    pub iteration_count: &'a [f64],
    pub sample_values: &'a [f64],
    pub avg_values: &'a [f64],
}

// Common analysis procedure
pub(crate) fn analysis<'a>(
    id: &BenchmarkId,
    config: &BenchmarkConfig,
    throughput: Option<Throughput>,
    new_sample: MeasuredValues<'a>,
    old_sample: Option<(MeasuredValues<'a>, &'a Estimates)>,
) -> MeasurementData<'a> {
    let iters = new_sample.iteration_count;
    let values = new_sample.sample_values;

    let avg_values = Sample::new(&new_sample.avg_values);

    let data = Data::new(&iters, &values);
    let labeled_sample = tukey::classify(avg_values);
    let (distribution, slope) = regression(&data, config);
    let (mut distributions, mut estimates) = estimates(avg_values, config);

    estimates.insert(Statistic::Slope, slope);
    distributions.insert(Statistic::Slope, distribution);

    let compare_data = if let Some((old_sample, old_estimates)) = old_sample {
        let result = compare(id, avg_values, &old_sample, config);
        match result {
            (
                t_value,
                t_distribution,
                relative_estimates,
                relative_distributions,
                base_avg_times,
            ) => {
                let p_value = t_distribution.p_value(t_value, &Tails::Two);
                Some(crate::report::ComparisonData {
                    p_value,
                    t_distribution,
                    t_value,
                    relative_estimates,
                    relative_distributions,
                    significance_threshold: config.significance_level,
                    noise_threshold: config.noise_threshold,
                    base_iter_counts: old_sample.iteration_count.iter().copied().collect(),
                    base_sample_times: old_sample.sample_values.iter().copied().collect(),
                    base_avg_times,
                    base_estimates: old_estimates.clone(),
                })
            }
        }
    } else {
        None
    };

    MeasurementData {
        data: Data::new(&*iters, &*values),
        avg_times: labeled_sample,
        absolute_estimates: estimates,
        distributions,
        comparison: compare_data,
        throughput,
    }
}

// Performs a simple linear regression on the sample
fn regression(
    data: &Data<'_, f64, f64>,
    config: &BenchmarkConfig,
) -> (Distribution<f64>, Estimate) {
    let cl = config.confidence_level;

    let distribution = elapsed!(
        "Bootstrapped linear regression",
        data.bootstrap(config.nresamples, |d| (Slope::fit(&d).0,))
    )
    .0;

    let point = Slope::fit(&data);
    let (lb, ub) = distribution.confidence_interval(config.confidence_level);
    let se = distribution.std_dev(None);

    (
        distribution,
        Estimate {
            confidence_interval: ConfidenceInterval {
                confidence_level: cl,
                lower_bound: lb,
                upper_bound: ub,
            },
            point_estimate: point.0,
            standard_error: se,
        },
    )
}

// Estimates the statistics of the population from the sample
fn estimates(avg_times: &Sample<f64>, config: &BenchmarkConfig) -> (Distributions, Estimates) {
    fn stats(sample: &Sample<f64>) -> (f64, f64, f64, f64) {
        let mean = sample.mean();
        let std_dev = sample.std_dev(Some(mean));
        let median = sample.percentiles().median();
        let mad = sample.median_abs_dev(Some(median));

        (mean, std_dev, median, mad)
    }

    let cl = config.confidence_level;
    let nresamples = config.nresamples;

    let (mean, std_dev, median, mad) = stats(avg_times);
    let mut point_estimates = BTreeMap::new();
    point_estimates.insert(Statistic::Mean, mean);
    point_estimates.insert(Statistic::StdDev, std_dev);
    point_estimates.insert(Statistic::Median, median);
    point_estimates.insert(Statistic::MedianAbsDev, mad);

    let (dist_mean, dist_stddev, dist_median, dist_mad) = elapsed!(
        "Bootstrapping the absolute statistics.",
        avg_times.bootstrap(nresamples, stats)
    );

    let mut distributions = Distributions::new();
    distributions.insert(Statistic::Mean, dist_mean);
    distributions.insert(Statistic::StdDev, dist_stddev);
    distributions.insert(Statistic::Median, dist_median);
    distributions.insert(Statistic::MedianAbsDev, dist_mad);

    let estimates = build_estimates(&distributions, &point_estimates, cl);

    (distributions, estimates)
}

// Common comparison procedure
#[cfg_attr(feature = "cargo-clippy", allow(clippy::type_complexity))]
pub(crate) fn compare(
    id: &BenchmarkId,
    new_avg_times: &Sample<f64>,
    old_values: &MeasuredValues,
    config: &BenchmarkConfig,
) -> (f64, Distribution<f64>, Estimates, Distributions, Vec<f64>) {
    let iters = old_values.iteration_count;
    let values = old_values.sample_values;
    let base_avg_values: Vec<f64> = iters
        .iter()
        .zip(values.iter())
        .map(|(iters, elapsed)| elapsed / iters)
        .collect();
    let base_avg_value_sample = Sample::new(&base_avg_values);

    let (t_statistic, t_distribution) = t_test(new_avg_times, base_avg_value_sample, config);

    let (estimates, relative_distributions) =
        difference_estimates(id, new_avg_times, base_avg_value_sample, config);

    std::mem::drop(base_avg_value_sample);

    (
        t_statistic,
        t_distribution,
        estimates,
        relative_distributions,
        base_avg_values,
    )
}

// Performs a two sample t-test
fn t_test(
    avg_times: &Sample<f64>,
    base_avg_times: &Sample<f64>,
    config: &BenchmarkConfig,
) -> (f64, Distribution<f64>) {
    let nresamples = config.nresamples;

    let t_statistic = avg_times.t(base_avg_times);
    let t_distribution = elapsed!(
        "Bootstrapping the T distribution",
        crate::stats::univariate::mixed::bootstrap(
            avg_times,
            base_avg_times,
            nresamples,
            |a, b| (a.t(b),)
        )
    )
    .0;

    // HACK: Filter out non-finite numbers, which can happen sometimes when sample size is very small.
    // Downstream code doesn't like non-finite values here.
    let t_distribution = Distribution::from(
        t_distribution
            .iter()
            .filter(|a| a.is_finite())
            .cloned()
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    );

    (t_statistic, t_distribution)
}

// Estimates the relative change in the statistics of the population
fn difference_estimates(
    id: &BenchmarkId,
    avg_times: &Sample<f64>,
    base_avg_times: &Sample<f64>,
    config: &BenchmarkConfig,
) -> (Estimates, Distributions) {
    fn stats(a: &Sample<f64>, b: &Sample<f64>) -> (f64, f64) {
        (
            a.mean() / b.mean() - 1.,
            a.percentiles().median() / b.percentiles().median() - 1.,
        )
    }

    let cl = config.confidence_level;
    let nresamples = config.nresamples;

    let (dist_mean, dist_median) = elapsed!(
        "Bootstrapping the relative statistics",
        crate::stats::univariate::bootstrap(avg_times, base_avg_times, nresamples, stats)
    );

    let mut distributions = Distributions::new();
    distributions.insert(Statistic::Mean, dist_mean);
    distributions.insert(Statistic::Median, dist_median);

    let (mean, median) = stats(avg_times, base_avg_times);
    let mut point_estimates = BTreeMap::new();
    point_estimates.insert(Statistic::Mean, mean);
    point_estimates.insert(Statistic::Median, median);

    let estimates = build_estimates(&distributions, &point_estimates, cl);

    (estimates, distributions)
}
