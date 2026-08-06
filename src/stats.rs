use chrono::{Duration, NaiveDate};
use std::collections::BTreeMap;

const BAR_MAX_WIDTH: usize = 20;

/// How wide the display-name column is before it gets truncated. Telegram
/// renders this inside <pre>, so the whole line has to stay narrow enough
/// not to wrap on a phone.
const NAME_WIDTH: usize = 16;

/// Render the /stat report: per-user totals with daily averages plus a
/// per-day Unicode bar chart. `rows` are (user_id, "YYYY-MM-DD", count) —
/// from `Store::usage_stats_for` for an ordinary caller, or
/// `Store::usage_stats_all` when an admin asks. `caller` is the requesting
/// user id, marked `(you)` in the listing; `names` maps ids to Telegram
/// display names, and an id missing from it is shown on its own. The window
/// is the `days` days ending `today`.
pub fn format_stats(
    rows: &[(i64, String, i64)],
    days: u32,
    today: NaiveDate,
    caller: i64,
    names: &BTreeMap<i64, String>,
    flights: &BTreeMap<i64, i64>,
) -> String {
    let total: i64 = rows.iter().map(|(_, _, c)| c).sum();
    if total == 0 {
        return format!("No requests in the last {days} days.");
    }

    let mut per_user: BTreeMap<i64, i64> = BTreeMap::new();
    let mut per_day: BTreeMap<String, i64> = BTreeMap::new();
    for (user, day, count) in rows {
        *per_user.entry(*user).or_default() += count;
        *per_day.entry(day.clone()).or_default() += count;
    }

    let mut out = format!(
        "Requests, last {days} days — total {total}, avg {:.1}/day\n\n",
        total as f64 / days as f64
    );

    // Whether this render covers one person or the whole bot, which only
    // changes how the trailing chart is labelled.
    let just_me = per_user.keys().all(|u| *u == caller);
    let mut users: Vec<(i64, i64)> = per_user.into_iter().collect();
    users.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (user, count) in users {
        out.push_str(&format!(
            "{:<NAME_WIDTH$}  {user:>10}  {count:>4}  ({:.1}/day)\n",
            display_label(user, caller, names),
            count as f64 / days as f64,
        ));
    }

    out.push_str(&format!(
        "\nPer day ({}):\n",
        if just_me { "you" } else { "all users" }
    ));
    let max = per_day.values().copied().max().unwrap_or(1).max(1);
    for offset in (0..days as i64).rev() {
        let date = today - Duration::days(offset);
        let key = date.format("%Y-%m-%d").to_string();
        let count = per_day.get(&key).copied().unwrap_or(0);
        let width = ((count as f64 / max as f64) * BAR_MAX_WIDTH as f64).round() as usize;
        out.push_str(&format!(
            "{}  {}{}  {count}\n",
            date.format("%m-%d"),
            "▇".repeat(width.max(usize::from(count > 0))),
            " ".repeat(BAR_MAX_WIDTH.saturating_sub(width)),
        ));
    }
    out.push_str(&flight_section(flights, caller, names));
    out.trim_end().to_string()
}

/// Flight searches, listed apart from ordinary requests because Duffel
/// charges for each one. Omitted entirely when there are none — most people
/// never ask about a flight, and a permanent zero would be noise on a
/// phone. Users with none are left out for the same reason.
fn flight_section(
    flights: &BTreeMap<i64, i64>,
    caller: i64,
    names: &BTreeMap<i64, String>,
) -> String {
    let mut users: Vec<(i64, i64)> = flights.iter().map(|(u, c)| (*u, *c)).filter(|(_, c)| *c > 0).collect();
    if users.is_empty() {
        return String::new();
    }
    users.sort_by_key(|(user, count)| (std::cmp::Reverse(*count), *user));

    let total: i64 = users.iter().map(|(_, c)| c).sum();
    let mut out = format!("\n\nFlight searches: {total}\n");
    for (user, count) in users {
        out.push_str(&format!(
            "{:<NAME_WIDTH$}  {user:>10}  {count:>4}\n",
            display_label(user, caller, names),
        ));
    }
    out
}

/// `Alice (you)`, `Alice`, or `—` when nobody has recorded a name for that
/// id yet. Truncation counts characters, not bytes: plenty of these names
/// are Cyrillic.
fn display_label(user: i64, caller: i64, names: &BTreeMap<i64, String>) -> String {
    let suffix = if user == caller { " (you)" } else { "" };
    let budget = NAME_WIDTH.saturating_sub(suffix.chars().count());
    let name = names.get(&user).map(String::as_str).unwrap_or("—");
    let mut label: String = name.chars().take(budget).collect();
    if name.chars().count() > budget && !label.is_empty() {
        label.pop();
        label.push('…');
    }
    format!("{label}{suffix}")
}

/// Parse the /stat argument: empty → 7 days, otherwise 1..=90.
pub fn parse_days(arg: &str) -> Result<u32, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(7);
    }
    match arg.parse::<u32>() {
        Ok(d @ 1..=90) => Ok(d),
        Ok(_) => Err("days must be between 1 and 90".to_string()),
        Err(_) => Err(format!("not a number of days: {arg:?}")),
    }
}

#[cfg(test)]
mod flight_tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn names(pairs: &[(i64, &str)]) -> BTreeMap<i64, String> {
        pairs.iter().map(|(id, n)| (*id, n.to_string())).collect()
    }

    fn counts(pairs: &[(i64, i64)]) -> BTreeMap<i64, i64> {
        pairs.iter().copied().collect()
    }

    #[test]
    fn flight_searches_get_their_own_section_because_duffel_bills_for_them() {
        let rows = vec![
            (1, "2026-07-27".to_string(), 40),
            (2, "2026-07-27".to_string(), 10),
        ];
        let out = format_stats(
            &rows,
            7,
            date("2026-07-27"),
            1,
            &names(&[(1, "Alice"), (2, "Bob")]),
            &counts(&[(1, 9), (2, 3)]),
        );

        assert!(out.contains("Flight searches"), "got:\n{out}");
        // The total is what maps to the bill.
        assert!(out.contains("12"), "expected a total of 12, got:\n{out}");
        // Per user, most first, labelled like the request table.
        let alice = out.find("Alice (you)").expect("caller labelled");
        let bob = out.find("Bob").expect("other user listed");
        assert!(alice < bob, "busiest user should come first, got:\n{out}");
    }

    #[test]
    fn no_flight_searches_means_no_section_at_all() {
        // Most people never ask about a flight; an always-present "0" line
        // would be noise in a report read on a phone.
        let rows = vec![(1, "2026-07-27".to_string(), 40)];
        let out = format_stats(&rows, 7, date("2026-07-27"), 1, &no_names(), &counts(&[]));
        assert!(!out.contains("Flight"), "got:\n{out}");
    }

    #[test]
    fn a_user_with_no_flight_searches_is_left_out_of_that_section() {
        let rows = vec![
            (1, "2026-07-27".to_string(), 40),
            (2, "2026-07-27".to_string(), 10),
        ];
        let out = format_stats(
            &rows,
            7,
            date("2026-07-27"),
            1,
            &names(&[(1, "Alice"), (2, "Bob")]),
            &counts(&[(1, 9)]),
        );
        let section = out.split("Flight searches").nth(1).expect("section present");
        assert!(section.contains("Alice"), "got:\n{out}");
        assert!(!section.contains("Bob"), "a zero user should not be listed, got:\n{out}");
    }

    fn no_names() -> BTreeMap<i64, String> {
        BTreeMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    fn no_names() -> BTreeMap<i64, String> {
        BTreeMap::new()
    }

    fn named(pairs: &[(i64, &str)]) -> BTreeMap<i64, String> {
        pairs.iter().map(|(id, n)| (*id, n.to_string())).collect()
    }

    #[test]
    fn empty_window_reports_no_requests() {
        assert_eq!(
            format_stats(&[], 7, date("2026-07-27"), 1, &no_names(), &BTreeMap::new()),
            "No requests in the last 7 days."
        );
    }

    #[test]
    fn totals_averages_and_ordering() {
        let rows = vec![
            (111, "2026-07-26".to_string(), 2),
            (222, "2026-07-26".to_string(), 5),
            (111, "2026-07-27".to_string(), 1),
        ];
        let out = format_stats(&rows, 2, date("2026-07-27"), 111, &no_names(), &BTreeMap::new());
        assert!(out.starts_with("Requests, last 2 days — total 8, avg 4.0/day"), "got: {out}");
        // busiest user first
        let pos_222 = out.find("222").unwrap();
        let pos_111 = out.find("111").unwrap();
        assert!(pos_222 < pos_111);
        assert!(out.contains("(1.5/day)")); // user 111: 3 requests / 2 days
        // more than the caller in view, so the chart covers everyone
        assert!(out.contains("Per day (all users):"), "got: {out}");
    }

    #[test]
    fn each_row_carries_the_name_and_the_id() {
        let rows = vec![
            (111, "2026-07-27".to_string(), 3),
            (222, "2026-07-27".to_string(), 9),
        ];
        let out = format_stats(
            &rows,
            1,
            date("2026-07-27"),
            111,
            &named(&[(111, "Alice"), (222, "Bob")]), &BTreeMap::new());
        let alice = out.lines().find(|l| l.contains("111")).unwrap();
        // name, then the raw id, then the count — the id is the identity,
        // the name is only a label.
        assert!(alice.starts_with("Alice (you)"), "got: {alice}");
        assert!(alice.contains("111"), "got: {alice}");
        let bob = out.lines().find(|l| l.contains("222")).unwrap();
        assert!(bob.starts_with("Bob "), "got: {bob}");
        assert!(!bob.contains("(you)"), "only the caller is marked: {bob}");
    }

    #[test]
    fn caller_alone_is_marked_you_and_keeps_their_id() {
        // The ordinary (non-admin) render: one user, their own row.
        let rows = vec![(42, "2026-07-27".to_string(), 7)];
        let out = format_stats(&rows, 1, date("2026-07-27"), 42, &named(&[(42, "Alice")]), &BTreeMap::new());
        assert!(out.contains("Alice (you)"), "got: {out}");
        assert!(out.contains("42"), "the id is shown alongside the name: {out}");
        assert!(out.contains("Per day (you):"), "got: {out}");
    }

    #[test]
    fn an_unknown_id_still_renders() {
        // Someone who was allowlisted but has not messaged since the names
        // table existed has no display name.
        let rows = vec![(999, "2026-07-27".to_string(), 2)];
        let out = format_stats(&rows, 1, date("2026-07-27"), 1, &no_names(), &BTreeMap::new());
        let line = out.lines().find(|l| l.contains("999")).unwrap();
        assert!(line.starts_with("—"), "got: {line}");
    }

    #[test]
    fn long_and_non_ascii_names_are_truncated_by_character() {
        let long = "Александра-Валентиновна";
        let out = format_stats(
            &[(7, "2026-07-27".to_string(), 1)],
            1,
            date("2026-07-27"),
            7,
            &named(&[(7, long)]), &BTreeMap::new());
        let line = out.lines().find(|l| l.contains("(you)")).unwrap();
        assert!(line.contains('…'), "long names get an ellipsis: {line}");
        // Truncation must not slice a multi-byte char in half; if it did,
        // building the String above would already have panicked. Check the
        // column still lines up with a short name.
        let short = format_stats(
            &[(7, "2026-07-27".to_string(), 1)],
            1,
            date("2026-07-27"),
            7,
            &named(&[(7, "Al")]), &BTreeMap::new());
        let short_line = short.lines().find(|l| l.contains("(you)")).unwrap();
        assert_eq!(
            line.chars().count(),
            short_line.chars().count(),
            "columns must align regardless of name length"
        );
    }

    #[test]
    fn day_chart_zero_fills_and_scales() {
        let rows = vec![(1, "2026-07-27".to_string(), 10)];
        let out = format_stats(&rows, 3, date("2026-07-27"), 1, &no_names(), &BTreeMap::new());
        // three day lines, oldest first, zero days present with 0
        assert!(out.contains("07-25"), "got: {out}");
        assert!(out.contains("07-26"));
        let full_bar = "▇".repeat(20);
        assert!(out.contains(&format!("07-27  {full_bar}  10")));
    }

    #[test]
    fn tiny_counts_still_show_a_bar() {
        let rows = vec![
            (1, "2026-07-26".to_string(), 100),
            (1, "2026-07-27".to_string(), 1),
        ];
        let out = format_stats(&rows, 2, date("2026-07-27"), 1, &no_names(), &BTreeMap::new());
        // 1/100 rounds to zero width but must still render one block
        let line = out.lines().find(|l| l.starts_with("07-27")).unwrap();
        assert!(line.contains('▇'), "got: {line}");
    }

    #[test]
    fn parse_days_defaults_and_bounds() {
        assert_eq!(parse_days(""), Ok(7));
        assert_eq!(parse_days(" 30 "), Ok(30));
        assert!(parse_days("0").is_err());
        assert!(parse_days("365").is_err());
        assert!(parse_days("tomorrow").is_err());
    }
}

