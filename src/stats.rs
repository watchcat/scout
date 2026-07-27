use chrono::{Duration, NaiveDate};
use std::collections::BTreeMap;

const BAR_MAX_WIDTH: usize = 20;

/// Render the /stat report: per-user totals with daily averages plus a
/// per-day Unicode bar chart. `rows` are (user_id, "YYYY-MM-DD", count)
/// from `Store::usage_stats`; the window is the `days` days ending `today`.
pub fn format_stats(rows: &[(i64, String, i64)], days: u32, today: NaiveDate) -> String {
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

    let mut users: Vec<(i64, i64)> = per_user.into_iter().collect();
    users.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (user, count) in users {
        out.push_str(&format!(
            "{user:>12}  {count:>4}  ({:.1}/day)\n",
            count as f64 / days as f64
        ));
    }

    out.push_str("\nPer day (all users):\n");
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
    out.trim_end().to_string()
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
mod tests {
    use super::*;

    fn date(s: &str) -> NaiveDate {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn empty_window_reports_no_requests() {
        assert_eq!(format_stats(&[], 7, date("2026-07-27")), "No requests in the last 7 days.");
    }

    #[test]
    fn totals_averages_and_ordering() {
        let rows = vec![
            (111, "2026-07-26".to_string(), 2),
            (222, "2026-07-26".to_string(), 5),
            (111, "2026-07-27".to_string(), 1),
        ];
        let out = format_stats(&rows, 2, date("2026-07-27"));
        assert!(out.starts_with("Requests, last 2 days — total 8, avg 4.0/day"), "got: {out}");
        // busiest user first
        let pos_222 = out.find("222").unwrap();
        let pos_111 = out.find("111").unwrap();
        assert!(pos_222 < pos_111);
        assert!(out.contains("(1.5/day)")); // user 111: 3 requests / 2 days
    }

    #[test]
    fn day_chart_zero_fills_and_scales() {
        let rows = vec![(1, "2026-07-27".to_string(), 10)];
        let out = format_stats(&rows, 3, date("2026-07-27"));
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
        let out = format_stats(&rows, 2, date("2026-07-27"));
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
