//! What `/metrics` reports: runs, tool calls, provider errors and the
//! store's lock.
//!
//! Written by hand rather than with a metrics crate. Six series in the
//! Prometheus text format is a page of code, and the one path that is hot —
//! the store's lock, taken on every call — is two clock reads and three
//! atomic adds here, with nothing behind them that could allocate or lock.
//!
//! One set for the process, reached through `global()`, because the places
//! that count — the store, the retry, the observer — are far apart and
//! share nothing else. Tests that look at the global set look at a label
//! only they use, since every test in the binary writes to it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// The most label sets one counter keeps. A tool name comes from the
/// model's call, and a model can name a tool that does not exist; past
/// this many, new names are counted under `other` instead of each growing
/// the page by a line.
pub const MAX_SERIES: usize = 256;

/// Upper bounds, in seconds, for a run's length. The budget is 300 s; a
/// wrap-up after a cut-short run can add a minute on top.
const RUN_BUCKETS: &[f64] = &[1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 90.0, 120.0, 180.0, 240.0, 300.0, 600.0];

/// Upper bounds, in seconds, for a wait on the store's lock: from an
/// uncontended take, well under a tenth of a millisecond, up to a backup
/// holding it for seconds.
const LOCK_BUCKETS: &[f64] = &[0.0001, 0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0];

/// A distribution, kept as a count per bucket plus one past the last.
/// Cumulative only when written out, so an observation is one add.
struct Histogram {
    bounds: &'static [f64],
    counts: Vec<AtomicU64>,
    sum_nanos: AtomicU64,
}

impl Histogram {
    fn new(bounds: &'static [f64]) -> Self {
        Self {
            bounds,
            counts: (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect(),
            sum_nanos: AtomicU64::new(0),
        }
    }

    fn observe(&self, took: Duration) {
        let secs = took.as_secs_f64();
        let slot = self.bounds.iter().position(|&le| secs <= le).unwrap_or(self.bounds.len());
        self.counts[slot].fetch_add(1, Ordering::Relaxed);
        self.sum_nanos.fetch_add(took.as_nanos().min(u64::MAX as u128) as u64, Ordering::Relaxed);
    }

    fn render(&self, out: &mut String, name: &str, help: &str) {
        header(out, name, help, "histogram");
        let mut running = 0;
        for (i, count) in self.counts.iter().enumerate() {
            running += count.load(Ordering::Relaxed);
            let le = self.bounds.get(i).map_or("+Inf".to_string(), |b| b.to_string());
            out.push_str(&format!("{name}_bucket{{le=\"{le}\"}} {running}\n"));
        }
        let sum = self.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9;
        out.push_str(&format!("{name}_sum {sum}\n{name}_count {running}\n"));
    }
}

/// A count per combination of label values.
struct Counter {
    labels: &'static [&'static str],
    values: Mutex<BTreeMap<Vec<String>, u64>>,
}

impl Counter {
    fn new(labels: &'static [&'static str]) -> Self {
        Self { labels, values: Mutex::new(BTreeMap::new()) }
    }

    fn add(&self, values: &[&str]) {
        let mut map = self.values.lock().unwrap_or_else(|e| e.into_inner());
        let mut key: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        if !map.contains_key(&key) && map.len() >= MAX_SERIES {
            key = vec!["other".to_string(); values.len()];
        }
        *map.entry(key).or_insert(0) += 1;
    }

    fn render(&self, out: &mut String, name: &str, help: &str) {
        header(out, name, help, "counter");
        for (values, n) in self.values.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            let pairs: Vec<String> = self
                .labels
                .iter()
                .zip(values)
                .map(|(label, value)| format!("{label}=\"{}\"", escape(value)))
                .collect();
            out.push_str(&format!("{name}{{{}}} {n}\n", pairs.join(",")));
        }
    }
}

fn header(out: &mut String, name: &str, help: &str, kind: &str) {
    out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
}

/// A label value as the text format wants it: backslash, quote and newline
/// escaped, everything else as it is.
fn escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

pub struct Metrics {
    runs: Counter,
    run_duration: Histogram,
    tool_calls: Counter,
    provider_errors: Counter,
    provider_retries: Counter,
    lock_wait: Histogram,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            runs: Counter::new(&["outcome"]),
            run_duration: Histogram::new(RUN_BUCKETS),
            tool_calls: Counter::new(&["tool", "status"]),
            provider_errors: Counter::new(&["provider", "kind"]),
            provider_retries: Counter::new(&["provider"]),
            lock_wait: Histogram::new(LOCK_BUCKETS),
        }
    }
}

impl Metrics {
    /// A run that had a row ended: `answered`, `cut_short` or `failed`.
    pub fn run_finished(&self, outcome: &str, took: Duration) {
        self.runs.add(&[outcome]);
        self.run_duration.observe(took);
    }

    /// A run that never started: `busy` or `overloaded`. Counted with the
    /// others and given no length, since nothing ran.
    pub fn run_refused(&self, outcome: &str) {
        self.runs.add(&[outcome]);
    }

    pub fn tool_call(&self, tool: &str, status: &str) {
        self.tool_calls.add(&[tool, status]);
    }

    /// A provider's request that failed for good, retries spent.
    pub fn provider_error(&self, provider: &str, kind: &str) {
        self.provider_errors.add(&[provider, kind]);
    }

    /// A provider's request sent again after a 429, a 5xx or a refused
    /// connection — counted whether or not the next try worked, since a
    /// retry that rescued the call is still a 429 the provider sent.
    pub fn provider_retry(&self, provider: &str) {
        self.provider_retries.add(&[provider]);
    }

    pub fn lock_waited(&self, took: Duration) {
        self.lock_wait.observe(took);
    }

    /// The whole set in the Prometheus text format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.runs.render(&mut out, "scout_runs_total", "Runs, by how they ended.");
        self.run_duration.render(&mut out, "scout_run_duration_seconds", "How long a run took, from its row to its last word.");
        self.tool_calls.render(&mut out, "scout_tool_calls_total", "Tool calls the agents made, by tool and whether the tool answered.");
        self.provider_errors.render(&mut out, "scout_provider_errors_total", "Requests to a provider that failed after their retries, by provider and kind.");
        self.provider_retries.render(&mut out, "scout_provider_retries_total", "Requests to a provider sent again after a 429, a 5xx or a refused connection.");
        self.lock_wait.render(&mut out, "scout_store_lock_wait_seconds", "Time a store call waited for the database's lock.");
        out
    }
}

/// The process's one set.
pub fn global() -> &'static Metrics {
    static METRICS: OnceLock<Metrics> = OnceLock::new();
    METRICS.get_or_init(Metrics::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line<'a>(page: &'a str, start: &str) -> Option<&'a str> {
        page.lines().find(|l| l.starts_with(start))
    }

    #[test]
    fn an_empty_set_still_names_every_series() {
        let page = Metrics::default().render();
        for name in [
            "scout_runs_total",
            "scout_run_duration_seconds",
            "scout_tool_calls_total",
            "scout_provider_errors_total",
            "scout_provider_retries_total",
            "scout_store_lock_wait_seconds",
        ] {
            assert!(page.contains(&format!("# TYPE {name} ")), "{name} is missing:\n{page}");
        }
        assert_eq!(line(&page, "scout_store_lock_wait_seconds_count"), Some("scout_store_lock_wait_seconds_count 0"));
    }

    #[test]
    fn counters_are_kept_per_label_set() {
        let m = Metrics::default();
        m.run_finished("answered", Duration::from_secs(3));
        m.run_finished("answered", Duration::from_secs(4));
        m.run_refused("busy");
        m.tool_call("search_flights", "ok");
        m.tool_call("search_flights", "failed");
        m.tool_call("search_flights", "ok");
        m.provider_error("duffel", "rate_limited");
        m.provider_retry("duffel");
        let page = m.render();
        assert_eq!(line(&page, "scout_runs_total{outcome=\"answered\"}"), Some("scout_runs_total{outcome=\"answered\"} 2"));
        assert_eq!(line(&page, "scout_runs_total{outcome=\"busy\"}"), Some("scout_runs_total{outcome=\"busy\"} 1"));
        assert_eq!(
            line(&page, "scout_tool_calls_total{tool=\"search_flights\",status=\"ok\"}"),
            Some("scout_tool_calls_total{tool=\"search_flights\",status=\"ok\"} 2")
        );
        assert_eq!(
            line(&page, "scout_tool_calls_total{tool=\"search_flights\",status=\"failed\"}"),
            Some("scout_tool_calls_total{tool=\"search_flights\",status=\"failed\"} 1")
        );
        assert!(page.contains("scout_provider_errors_total{provider=\"duffel\",kind=\"rate_limited\"} 1\n"));
        assert!(page.contains("scout_provider_retries_total{provider=\"duffel\"} 1\n"));
    }

    #[test]
    fn a_histogram_is_cumulative_with_a_sum_and_a_count() {
        let m = Metrics::default();
        m.run_finished("answered", Duration::from_millis(1500));
        m.run_finished("answered", Duration::from_secs(25));
        m.run_finished("failed", Duration::from_secs(900));
        let page = m.render();
        assert_eq!(line(&page, "scout_run_duration_seconds_bucket{le=\"1\"}"), Some("scout_run_duration_seconds_bucket{le=\"1\"} 0"));
        assert_eq!(line(&page, "scout_run_duration_seconds_bucket{le=\"2.5\"}"), Some("scout_run_duration_seconds_bucket{le=\"2.5\"} 1"));
        assert_eq!(line(&page, "scout_run_duration_seconds_bucket{le=\"30\"}"), Some("scout_run_duration_seconds_bucket{le=\"30\"} 2"));
        assert_eq!(line(&page, "scout_run_duration_seconds_bucket{le=\"600\"}"), Some("scout_run_duration_seconds_bucket{le=\"600\"} 2"));
        assert_eq!(line(&page, "scout_run_duration_seconds_bucket{le=\"+Inf\"}"), Some("scout_run_duration_seconds_bucket{le=\"+Inf\"} 3"));
        assert_eq!(line(&page, "scout_run_duration_seconds_sum"), Some("scout_run_duration_seconds_sum 926.5"));
        assert_eq!(line(&page, "scout_run_duration_seconds_count"), Some("scout_run_duration_seconds_count 3"));
    }

    #[test]
    fn a_value_on_a_bound_lands_in_that_bucket() {
        // `le` is "less than or equal": a wait of exactly a millisecond is
        // inside the 0.001 bucket, not the next one.
        let m = Metrics::default();
        m.lock_waited(Duration::from_millis(1));
        let page = m.render();
        assert_eq!(line(&page, "scout_store_lock_wait_seconds_bucket{le=\"0.0005\"}"), Some("scout_store_lock_wait_seconds_bucket{le=\"0.0005\"} 0"));
        assert_eq!(line(&page, "scout_store_lock_wait_seconds_bucket{le=\"0.001\"}"), Some("scout_store_lock_wait_seconds_bucket{le=\"0.001\"} 1"));
    }

    #[test]
    fn a_label_value_is_escaped() {
        let m = Metrics::default();
        m.tool_call("say \"hi\"\\\nnow", "failed");
        assert!(m.render().contains("tool=\"say \\\"hi\\\"\\\\\\nnow\""));
    }

    #[test]
    fn a_counter_stops_growing_at_its_cap_and_counts_the_rest_as_other() {
        let m = Metrics::default();
        for i in 0..MAX_SERIES {
            m.tool_call(&format!("tool_{i}"), "ok");
        }
        m.tool_call("invented_by_the_model", "failed");
        m.tool_call("another_invention", "failed");
        // A name already kept is still counted as itself.
        m.tool_call("tool_0", "ok");
        let page = m.render();
        assert_eq!(page.matches("scout_tool_calls_total{").count(), MAX_SERIES + 1);
        assert!(page.contains("scout_tool_calls_total{tool=\"other\",status=\"other\"} 2\n"));
        assert!(page.contains("scout_tool_calls_total{tool=\"tool_0\",status=\"ok\"} 2\n"));
        assert!(!page.contains("invented_by_the_model"));
    }
}
