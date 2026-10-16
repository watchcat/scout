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
use std::time::{Duration, Instant};

/// The most label sets one counter keeps. A tool name comes from the
/// model's call, and a model can name a tool that does not exist; past
/// this many, a new set is counted with its first label — the tool, the
/// provider — as `other`, keeping the rest, which are a handful of words
/// this code chose. Those `other` sets may take the page a few lines past
/// the cap, and no further.
pub const MAX_SERIES: usize = 256;

/// The longest a label value is kept. A name the model invented can be any
/// length; a real one is a few words.
const MAX_VALUE: usize = 64;

/// Upper bounds, in seconds, for a run's length. The budget is 300 s; a
/// wrap-up after a cut-short run can add a minute on top.
const RUN_BUCKETS: &[f64] = &[1.0, 2.5, 5.0, 10.0, 20.0, 30.0, 60.0, 90.0, 120.0, 180.0, 240.0, 300.0, 600.0];

/// Upper bounds, in seconds, for a wait on the store's lock: from an
/// uncontended take, well under a tenth of a millisecond, up to a backup
/// holding it for seconds.
const LOCK_BUCKETS: &[f64] = &[0.0001, 0.0005, 0.001, 0.005, SLOW_WAIT, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0];

/// A wait on the store's lock the summary calls slow. One of the bounds
/// above, by name, so the count and its label cannot drift apart.
const SLOW_WAIT: f64 = 0.01;

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
        self.add_n(values, 1);
    }

    /// `n` at once; a zero still names the series, so a page says "none"
    /// rather than nothing.
    fn add_n(&self, values: &[&str], n: u64) {
        let mut map = self.values.lock().unwrap_or_else(|e| e.into_inner());
        let mut key: Vec<String> = values.iter().map(|v| v.chars().take(MAX_VALUE).collect()).collect();
        if !map.contains_key(&key) && map.len() >= MAX_SERIES {
            key[0] = "other".to_string();
        }
        *map.entry(key).or_insert(0) += n;
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
    /// When this set began counting. The process's set is made when
    /// `Store::open` first asks for it, inside `Core::start` — so this is
    /// the restart the summary counts from.
    started: Instant,
    runs: Counter,
    run_duration: Histogram,
    tool_calls: Counter,
    provider_errors: Counter,
    provider_retries: Counter,
    lock_wait: Histogram,
    search_calls: Counter,
    search_results: Counter,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            runs: Counter::new(&["outcome"]),
            run_duration: Histogram::new(RUN_BUCKETS),
            tool_calls: Counter::new(&["tool", "status"]),
            provider_errors: Counter::new(&["provider", "kind"]),
            provider_retries: Counter::new(&["provider"]),
            lock_wait: Histogram::new(LOCK_BUCKETS),
            search_calls: Counter::new(&["queries"]),
            search_results: Counter::new(&["source", "outcome"]),
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

    /// A `search_web` call, by how many Kagi queries it spent.
    pub fn search_call(&self, queries: usize) {
        self.search_calls.add(&[&queries.to_string()]);
    }

    /// What became of one source's hits in a `search_web` call: `kept`,
    /// `duplicate` or `cut`, `n` of them. The source is the Kagi query's
    /// position — `kagi_1` is the user's own phrasing — or `perplexity`.
    pub fn search_results(&self, source: &str, outcome: &str, n: usize) {
        self.search_results.add_n(&[source, outcome], n as u64);
    }

    /// The whole set in the Prometheus text format.
    pub fn render(&self) -> String {
        let mut out = String::new();
        self.runs.render(&mut out, "scout_runs_total", "Runs, by how they ended: answered, cut_short or failed once a run has a row; busy or overloaded when it never got one.");
        self.run_duration.render(&mut out, "scout_run_duration_seconds", "How long a run took, from its row to its last word; the wait for a slot is not in it.");
        self.tool_calls.render(&mut out, "scout_tool_calls_total", "Tool calls the agents made, by tool and whether the tool answered.");
        self.provider_errors.render(&mut out, "scout_provider_errors_total", "Requests that failed after their retries: paid APIs as they answered (client includes a not-found), and the model during runs.");
        self.provider_retries.render(&mut out, "scout_provider_retries_total", "Requests to a provider sent again after a 429, a 5xx or a refused connection.");
        self.lock_wait.render(&mut out, "scout_store_lock_wait_seconds", "Time a store call waited for the database's lock.");
        self.search_calls.render(&mut out, "scout_search_calls_total", "search_web calls, by how many Kagi queries each spent.");
        self.search_results.render(&mut out, "scout_search_results_total", "Hits per search_web call by source (kagi_N is the Nth query, kagi_1 the user's own) and what became of them: kept for the model, a duplicate of an earlier list, or cut by the cap.");
        out
    }
}

/// The upper bound of the bucket the `q`-th share of a histogram's
/// observations falls in, or `None` past the last bound — what the summary
/// can honestly say about a quantile is "under this".
fn quantile_bound(bounds: &[f64], counts: &[u64], q: f64) -> Option<f64> {
    let total: u64 = counts.iter().sum();
    let rank = ((q * total as f64).ceil() as u64).max(1);
    let mut running = 0;
    for (i, n) in counts.iter().enumerate() {
        running += n;
        if running >= rank {
            return bounds.get(i).copied();
        }
    }
    None
}

/// A length as a number and its unit: milliseconds under a second,
/// minutes from one, seconds between. One decimal at most, none when whole.
/// Meant for bucket bounds, which sit well inside a unit: the unit is
/// chosen before rounding, so 0.99996 s would read `1000 ms`.
fn span_parts(secs: f64) -> (String, &'static str) {
    let (value, unit) = match secs {
        s if s < 1.0 => (s * 1000.0, "ms"),
        s if s < 60.0 => (s, "s"),
        s => (s / 60.0, "min"),
    };
    let rounded = (value * 10.0).round() / 10.0;
    let number = if rounded.fract() == 0.0 { format!("{}", rounded as i64) } else { format!("{rounded}") };
    (number, unit)
}

fn span(secs: f64) -> String {
    let (number, unit) = span_parts(secs);
    format!("{number} {unit}")
}

/// `10–50 ms`, `500 ms–1 s`: the unit once when both ends share it.
fn span_range(low: f64, high: f64) -> String {
    let ((lo, lo_unit), (hi, hi_unit)) = (span_parts(low), span_parts(high));
    if lo_unit == hi_unit {
        format!("{lo}–{hi} {hi_unit}")
    } else {
        format!("{lo} {lo_unit}–{hi} {hi_unit}")
    }
}

/// `1,202`: digits grouped in threes, as a count is read aloud.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{} {}", grouped(n), if n == 1 { one } else { many })
}

/// `14h 5m`, `3d 2h`, `12m`: the two largest units, enough to place a
/// restart.
fn ago(took: Duration) -> String {
    let secs = took.as_secs();
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// How many tools the summary names before counting the rest.
const TOOLS_NAMED: usize = 8;

impl Histogram {
    fn snapshot(&self) -> Vec<u64> {
        self.counts.iter().map(|c| c.load(Ordering::Relaxed)).collect()
    }
}

impl Counter {
    fn snapshot(&self) -> BTreeMap<Vec<String>, u64> {
        self.values.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl Metrics {
    /// The set in a few lines a person reads on a phone, for the admin's
    /// `/metrics`. The full page is `render`; this one says what it adds up
    /// to, and a quantile only as the bucket it falls under.
    pub fn summary(&self) -> String {
        self.summary_after(self.started.elapsed())
    }

    /// The summary for a set that has been counting for `uptime`, apart
    /// from the clock so a test can say how long that was.
    fn summary_after(&self, uptime: Duration) -> String {
        [
            format!("Since restart {} ago\n", ago(uptime)),
            self.runs_line(),
            self.tools_line(),
            self.providers_line(),
            self.lock_line(),
        ]
        .join("\n")
    }

    fn runs_line(&self) -> String {
        let runs = self.runs.snapshot();
        if runs.is_empty() {
            return "Runs: none yet".to_string();
        }
        // In the order a run's life goes, then anything else as it came.
        let order = ["answered", "cut_short", "failed", "busy", "overloaded"];
        let rank = |k: &str| order.iter().position(|o| *o == k).unwrap_or(order.len());
        let mut outcomes: Vec<(&str, u64)> = runs.iter().map(|(k, n)| (k[0].as_str(), *n)).collect();
        outcomes.sort_by_key(|(k, _)| rank(k));
        let said: Vec<String> = outcomes.iter().map(|(k, n)| format!("{} {}", grouped(*n), k.replace('_', " "))).collect();
        let mut line = format!("Runs: {}", said.join(", "));
        let counts = self.run_duration.snapshot();
        if counts.iter().sum::<u64>() > 0 {
            let under = |q| match quantile_bound(RUN_BUCKETS, &counts, q) {
                Some(b) => format!("under {}", span(b)),
                None => format!("over {}", span(RUN_BUCKETS[RUN_BUCKETS.len() - 1])),
            };
            line.push_str(&format!(" · half {}, 9 in 10 {}", under(0.5), under(0.9)));
        }
        line
    }

    fn tools_line(&self) -> String {
        let mut tools: BTreeMap<String, (u64, u64)> = BTreeMap::new();
        for (k, n) in self.tool_calls.snapshot() {
            // A name the model invented is shown, not obeyed: a newline in
            // it would forge a line of this summary.
            let tool: String = k[0].chars().map(|c| if c.is_control() { '?' } else { c }).collect();
            let tool = if tool.trim().is_empty() { "(unnamed)".to_string() } else { tool };
            let entry = tools.entry(tool).or_default();
            entry.0 += n;
            if k[1] == "failed" {
                entry.1 += n;
            }
        }
        if tools.is_empty() {
            return "Tools: none yet".to_string();
        }
        let mut busiest: Vec<(String, (u64, u64))> = tools.into_iter().collect();
        // Stable, over a map already in name order: a tie reads alphabetically.
        busiest.sort_by_key(|(_, (all, _))| std::cmp::Reverse(*all));
        let mut said: Vec<String> = busiest
            .iter()
            .take(TOOLS_NAMED)
            .map(|(tool, (all, failed))| match failed {
                0 => format!("{tool} {}", grouped(*all)),
                f => format!("{tool} {} ({} failed)", grouped(*all), grouped(*f)),
            })
            .collect();
        if busiest.len() > TOOLS_NAMED {
            said.push(format!("and {} more", busiest.len() - TOOLS_NAMED));
        }
        format!("Tools: {}", said.join(", "))
    }

    fn providers_line(&self) -> String {
        let mut providers: BTreeMap<String, (Vec<String>, u64)> = BTreeMap::new();
        for (k, n) in self.provider_errors.snapshot() {
            providers.entry(k[0].clone()).or_default().0.push(format!("{} {}", grouped(n), k[1].replace('_', " ")));
        }
        for (k, n) in self.provider_retries.snapshot() {
            providers.entry(k[0].clone()).or_default().1 += n;
        }
        if providers.is_empty() {
            return "Providers: no errors, no retries".to_string();
        }
        let said: Vec<String> = providers
            .into_iter()
            .map(|(provider, (mut parts, retries))| {
                if retries > 0 {
                    parts.push(plural(retries, "retry", "retries"));
                }
                format!("{provider} {}", parts.join(", "))
            })
            .collect();
        format!("Providers: {}", said.join("; "))
    }

    fn lock_line(&self) -> String {
        let counts = self.lock_wait.snapshot();
        let total: u64 = counts.iter().sum();
        if total == 0 {
            return "Store lock: no takes yet".to_string();
        }
        // Past the bucket that ends at the threshold: a wait of exactly
        // 10 ms is within it, since buckets are `le`.
        let within = LOCK_BUCKETS.iter().position(|&b| b == SLOW_WAIT).expect("SLOW_WAIT is one of the bounds");
        let slow: u64 = counts[within + 1..].iter().sum();
        let slow = match slow {
            0 => format!("none waited over {}", span(SLOW_WAIT)),
            n => format!("{} waited over {}", grouped(n), span(SLOW_WAIT)),
        };
        let last = counts.iter().rposition(|&n| n > 0).expect("total is not zero");
        let longest = match last {
            0 => format!("under {}", span(LOCK_BUCKETS[0])),
            i if i == LOCK_BUCKETS.len() => format!("over {}", span(LOCK_BUCKETS[i - 1])),
            i => span_range(LOCK_BUCKETS[i - 1], LOCK_BUCKETS[i]),
        };
        format!("Store lock: {}, {slow}, longest {longest}", plural(total, "take", "takes"))
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
            "scout_search_calls_total",
            "scout_search_results_total",
        ] {
            assert!(page.contains(&format!("# TYPE {name} ")), "{name} is missing:\n{page}");
        }
        assert_eq!(line(&page, "scout_store_lock_wait_seconds_count"), Some("scout_store_lock_wait_seconds_count 0"));
    }

    #[test]
    fn search_tallies_are_counted_by_source_and_outcome() {
        let m = Metrics::default();
        m.search_call(3);
        m.search_results("kagi_3", "cut", 4);
        m.search_results("kagi_3", "cut", 2);
        m.search_results("perplexity", "kept", 0);
        let page = m.render();
        assert!(page.contains("scout_search_calls_total{queries=\"3\"} 1\n"), "{page}");
        assert!(page.contains("scout_search_results_total{source=\"kagi_3\",outcome=\"cut\"} 6\n"), "{page}");
        assert!(page.contains("scout_search_results_total{source=\"perplexity\",outcome=\"kept\"} 0\n"), "a source with nothing kept is still named:\n{page}");
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

    const FOURTEEN_HOURS: Duration = Duration::from_secs(14 * 3600 + 5 * 60);

    #[test]
    fn a_quiet_process_says_so_line_by_line() {
        let text = Metrics::default().summary_after(Duration::from_secs(12 * 60));
        assert_eq!(
            text,
            "Since restart 12m ago\n\n\
             Runs: none yet\n\
             Tools: none yet\n\
             Providers: no errors, no retries\n\
             Store lock: no takes yet"
        );
    }

    #[test]
    fn the_summary_reads_the_counts_as_a_person_would_say_them() {
        let m = Metrics::default();
        for secs in [3, 8, 15, 18, 19, 25, 40, 70, 100, 110] {
            m.run_finished("answered", Duration::from_secs(secs));
        }
        m.run_finished("cut_short", Duration::from_secs(300));
        m.run_refused("busy");
        for _ in 0..3 {
            m.tool_call("search_flights", "ok");
        }
        m.tool_call("search_flights", "failed");
        for _ in 0..5 {
            m.tool_call("fetch_page", "ok");
        }
        m.tool_call("search_web", "failed");
        m.provider_error("duffel", "rate_limited");
        m.provider_error("duffel", "rate_limited");
        m.provider_retry("duffel");
        m.provider_retry("duffel");
        m.provider_retry("kagi");
        m.provider_error("model", "stalled");
        for _ in 0..1200 {
            m.lock_waited(Duration::from_micros(20));
        }
        m.lock_waited(Duration::from_millis(30));
        m.lock_waited(Duration::from_millis(700));

        let text = m.summary_after(FOURTEEN_HOURS);

        assert_eq!(
            text,
            "Since restart 14h 5m ago\n\n\
             Runs: 10 answered, 1 cut short, 1 busy · half under 30 s, 9 in 10 under 2 min\n\
             Tools: fetch_page 5, search_flights 4 (1 failed), search_web 1 (1 failed)\n\
             Providers: duffel 2 rate limited, 2 retries; kagi 1 retry; model 1 stalled\n\
             Store lock: 1,202 takes, 2 waited over 10 ms, longest 500 ms–1 s"
        );
    }

    #[test]
    fn the_tool_line_names_the_busiest_and_counts_the_rest() {
        let m = Metrics::default();
        for i in 0..10 {
            for _ in 0..=i {
                m.tool_call(&format!("tool_{i}"), "ok");
            }
        }
        let text = m.summary_after(FOURTEEN_HOURS);
        let tools = text.lines().find(|l| l.starts_with("Tools:")).unwrap();
        assert_eq!(tools, "Tools: tool_9 10, tool_8 9, tool_7 8, tool_6 7, tool_5 6, tool_4 5, tool_3 4, tool_2 3, and 2 more");
    }

    #[test]
    fn a_wait_past_the_last_bound_and_a_run_past_the_last_bound_say_over() {
        let m = Metrics::default();
        m.run_finished("failed", Duration::from_secs(900));
        m.lock_waited(Duration::from_secs(20));
        let text = m.summary_after(Duration::from_secs(3 * 86400 + 2 * 3600));
        assert!(text.starts_with("Since restart 3d 2h ago"), "{text}");
        assert!(text.contains("Runs: 1 failed · half over 10 min, 9 in 10 over 10 min"), "{text}");
        assert!(text.contains("longest over 10 s"), "{text}");
    }

    #[test]
    fn an_uncontended_lock_says_how_little_it_waited() {
        let m = Metrics::default();
        m.lock_waited(Duration::from_micros(20));
        let text = m.summary_after(FOURTEEN_HOURS);
        assert!(text.ends_with("Store lock: 1 take, none waited over 10 ms, longest under 0.1 ms"), "{text}");
    }

    #[test]
    fn a_wait_of_exactly_ten_ms_is_not_slow_and_one_just_past_it_is() {
        let m = Metrics::default();
        m.lock_waited(Duration::from_millis(7));
        m.lock_waited(Duration::from_millis(10));
        let text = m.summary_after(FOURTEEN_HOURS);
        assert!(text.ends_with("Store lock: 2 takes, none waited over 10 ms, longest 5–10 ms"), "{text}");
        m.lock_waited(Duration::from_micros(10_001));
        let text = m.summary_after(FOURTEEN_HOURS);
        assert!(text.ends_with("Store lock: 3 takes, 1 waited over 10 ms, longest 10–50 ms"), "{text}");
    }

    #[test]
    fn runs_that_were_only_turned_away_have_no_lengths_to_report() {
        let m = Metrics::default();
        m.run_refused("overloaded");
        m.run_refused("busy");
        let text = m.summary_after(FOURTEEN_HOURS);
        let runs = text.lines().find(|l| l.starts_with("Runs:")).unwrap();
        assert_eq!(runs, "Runs: 1 busy, 1 overloaded");
    }

    #[test]
    fn eight_tools_are_all_named_and_a_ninth_is_counted() {
        let m = Metrics::default();
        for i in 0..8 {
            m.tool_call(&format!("t{i}"), "ok");
        }
        let line = |m: &Metrics| m.summary_after(FOURTEEN_HOURS).lines().find(|l| l.starts_with("Tools:")).unwrap().to_string();
        assert_eq!(line(&m), "Tools: t0 1, t1 1, t2 1, t3 1, t4 1, t5 1, t6 1, t7 1");
        m.tool_call("t8", "ok");
        assert_eq!(line(&m), "Tools: t0 1, t1 1, t2 1, t3 1, t4 1, t5 1, t6 1, t7 1, and 1 more");
    }

    #[test]
    fn a_tool_name_cannot_forge_a_line_of_the_summary() {
        let m = Metrics::default();
        m.tool_call("x\nStore lock: 0 takes", "failed");
        m.tool_call("", "ok");
        let text = m.summary_after(Duration::from_secs(45));
        assert!(text.starts_with("Since restart 45s ago"), "{text}");
        assert_eq!(text.lines().filter(|l| l.starts_with("Store lock:")).count(), 1, "{text}");
        assert!(text.contains("x?Store lock: 0 takes 1 (1 failed)"), "{text}");
        assert!(text.contains("(unnamed) 1"), "{text}");
    }

    #[test]
    fn a_long_label_value_is_cut_to_a_length_a_name_could_have() {
        let m = Metrics::default();
        m.tool_call(&"x".repeat(1000), "failed");
        assert!(m.render().contains(&format!("tool=\"{}\"", "x".repeat(MAX_VALUE))));
        assert!(!m.render().contains(&"x".repeat(MAX_VALUE + 1)));
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
        assert!(page.contains("scout_tool_calls_total{tool=\"other\",status=\"failed\"} 2\n"), "the outcome is kept");
        assert!(page.contains("scout_tool_calls_total{tool=\"tool_0\",status=\"ok\"} 2\n"));
        assert!(!page.contains("invented_by_the_model"));
    }
}
