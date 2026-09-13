//! The debug switch and the trace behind an answer, as the web sees them.
//! The only door through `Store` for either, kept narrow on purpose.

use crate::core::{blocking, Core};

/// Turns the account's debug switch on or off. Whether the account may is
/// the caller's question (`Core::is_admin_account`), not this one's.
pub async fn set(core: &Core, account_id: i64, on: bool) -> anyhow::Result<()> {
    let store = core.store();
    blocking(move || store.set_debug(account_id, on)).await
}

/// Whether the account's debug switch is on. Off for an account that has
/// never touched it.
pub async fn is_on(core: &Core, account_id: i64) -> anyhow::Result<bool> {
    let store = core.store();
    blocking(move || store.debug_of(account_id)).await
}

/// The run and its rows, or `None` when it is not this account's or no
/// longer kept. Does not check the flag: the route does, so the reason can
/// be told apart.
pub async fn trace(core: &Core, run_id: i64, account_id: i64) -> anyhow::Result<Option<(scout_api::RunRow, Vec<scout_api::TraceRow>)>> {
    let store = core.store();
    blocking(move || store.trace_of(run_id, account_id)).await
}

/// A finished run with rows, for the web crate's tests, which cannot reach
/// `Store`. Returns the run id.
#[doc(hidden)]
pub async fn seed_run_for_tests(core: &Core, account_id: i64, rows: Vec<scout_api::TraceRow>) -> anyhow::Result<i64> {
    let store = core.store();
    blocking(move || {
        let conv = store.start_conversation(account_id, "direct")?;
        let id = store.open_run(account_id, conv)?;
        store.append_traces(id, &rows)?;
        store.close_run(id, "answered", None)?;
        Ok(id)
    })
    .await
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn the_switch_is_per_account_and_off_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.duckdb").to_str().unwrap().to_string();
        let core = crate::core::Core::start(crate::config::Config::for_test(&p), None).unwrap();
        let a = core.store().account_for_telegram(1).unwrap();
        assert!(!super::is_on(&core, a).await.unwrap());
        super::set(&core, a, true).await.unwrap();
        assert!(super::is_on(&core, a).await.unwrap());
    }
}
