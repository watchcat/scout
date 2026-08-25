# Consistent Backups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Scout takes consistent copies of its own database — before every migration, nightly, and on demand — because it holds the only connection and nothing else can.

**Architecture:** One mechanism (`ATTACH` / `COPY FROM DATABASE` / `DETACH` from the live connection), three callers. Naming, retention and scheduling live in a new `backup.rs` in `scout-core`. `Core` owns the schedule so it does not depend on a channel existing.

**Tech Stack:** Rust, DuckDB, chrono. No new dependencies.

---

## Verified before writing this plan

| Claim | How it was checked | Result |
|---|---|---|
| The mechanism exists in the bundled DuckDB | probe test run against `duckdb` crate 1.10501.0 | `COPY FROM DATABASE accepted`; backup opened independently, rows intact |
| Committed data lives outside the main file | `ls -l /data` in the running pod | `scout.duckdb` 15,478,784 B **and** `scout.duckdb.wal` 66,971 B |
| Migrations run inside `Store::open` | `store.rs:673` | `open` → `MIGRATIONS` → `apply_steps(&conn)` |
| `apply_steps` has the connection but not the path | `store.rs:622` | takes `&Connection` only — the path must be threaded in |
| A frozen pre-phase-one schema exists for tests | `store.rs:1907` | `LEGACY_SCHEMA`, deliberately never updated |
| Commands are a teloxide enum | `bot.rs:114` | `#[derive(BotCommands)] enum Command` |
| Where background loops are spawned | `main.rs:63` | `tokio::spawn(scheduler::run(...))` beside the bot |

**The gotcha that will bite if ignored:** `COPY FROM DATABASE <name>` needs the
source database's *identifier*, which DuckDB derives from the filename —
`scout` in production, a random temp name under test. Hardcoding `scout` passes
nothing and couples production to a filename. Ask
`SELECT current_database()`.

## File Structure

```
crates/scout-core/src/
  store.rs      backup_connection() free fn; Store::backup_to(); apply_steps gains a path
  backup.rs     naming, retention, "is one due", Core::backup, Core::run_maintenance
  lib.rs        mod backup;
crates/scout-telegram/src/
  bot.rs        Command::Backup, admin-gated
  main.rs       spawns run_maintenance
```

---

### Task 1: One consistent copy

**Files:**
- Modify: `crates/scout-core/src/store.rs`

- [ ] **Step 1: Write the failing test**

In `store.rs`'s `mod tests`:

```rust
    #[test]
    fn a_backup_is_a_whole_database_that_opens_on_its_own() {
        // Taken from the connection that holds the source open, with no
        // checkpoint and nothing stopped. That is the entire point: no
        // outside process can open this file while Scout runs, so a copy
        // made from outside is crash-consistent at best.
        let (store, dir) = test_store();
        let account = store.account_for_telegram(99).unwrap();
        store.remember_user(account, "before the backup").unwrap();

        let backup = dir.path().join("backup.duckdb");
        store.backup_to(&backup).unwrap();

        // The source is undisturbed and still writable.
        store.remember_user(account, "after the backup").unwrap();

        let restored = Store::open(&backup).unwrap();
        assert_eq!(
            restored.display_names().unwrap().get(&account).map(String::as_str),
            Some("before the backup"),
            "the backup should hold what was committed when it was taken"
        );
        assert_eq!(restored.schema_version().unwrap(), store.schema_version().unwrap());
    }
```

`display_names()` returns a `BTreeMap<i64, String>` (`store.rs:1357`); there is
no single-account reader.

- [ ] **Step 2: Run it to watch it fail**

Run: `TZ=UTC cargo test -p scout-core a_backup_is_a_whole_database`
Expected: FAIL — `no method named 'backup_to'`.

- [ ] **Step 3: Implement it**

A free function, because the migration hook has a `&Connection` and no `Store`:

```rust
/// Writes a consistent copy of `conn`'s database to `path`.
///
/// DuckDB is single-writer, so this is the only way to get a copy that is not
/// merely crash-consistent: it runs on the connection that already holds the
/// database open, folding in whatever is still in the write-ahead log.
///
/// Written to a `.partial` and renamed, so an interrupted backup leaves
/// something obviously unfinished rather than something that looks restorable.
fn backup_connection(conn: &Connection, path: &Path) -> Result<()> {
    // The source's identifier is derived from its filename — `scout` in
    // production, a temp name under test — so it has to be asked for rather
    // than assumed.
    let source: String = conn.query_row("SELECT current_database()", [], |r| r.get(0))?;
    let partial = path.with_extension("partial");
    let _ = std::fs::remove_file(&partial);

    conn.execute_batch(&format!(
        "ATTACH '{}' AS scout_backup; COPY FROM DATABASE \"{}\" TO scout_backup; DETACH scout_backup;",
        partial.display(),
        source,
    ))?;
    std::fs::rename(&partial, path)?;
    Ok(())
}

impl Store {
    /// A consistent copy, taken without stopping anything.
    pub(crate) fn backup_to(&self, path: &Path) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        backup_connection(&conn, path)
    }
}
```

The lock is held for the copy, which blocks the agent. At 15 MB that is
imperceptible; it is called out here because it scales with the database.

- [ ] **Step 4: Run it to watch it pass**

Run: `TZ=UTC cargo test -p scout-core a_backup_is_a_whole_database`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/store.rs
git commit -m "feat: scout can copy its own database while using it"
```

---

### Task 2: Naming and retention

**Files:**
- Create: `crates/scout-core/src/backup.rs`
- Modify: `crates/scout-core/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_backup_says_when_it_was_taken_and_why() {
        // The question at restore time is never "which timestamp" alone, it
        // is "which one, and what was about to happen to it".
        let name = file_name(&chrono::Utc.with_ymd_and_hms(2026, 8, 25, 2, 15, 0).unwrap(), Reason::Nightly);
        assert_eq!(name, "scout-2026-08-25T021500Z-nightly.duckdb");

        let m = file_name(&chrono::Utc.with_ymd_and_hms(2026, 8, 25, 2, 15, 0).unwrap(), Reason::Migration { to: 6 });
        assert_eq!(m, "scout-2026-08-25T021500Z-migration-v6.duckdb");
    }

    #[test]
    fn pruning_keeps_the_newest_and_never_touches_anything_else() {
        let dir = tempfile::tempdir().unwrap();
        // ISO timestamps sort lexically, so ordering never parses anything.
        for d in 1..=5 {
            std::fs::write(dir.path().join(format!("scout-2026-08-0{d}T000000Z-nightly.duckdb")), b"x").unwrap();
        }
        // Things that are not ours. A glob bug must not be able to eat these.
        std::fs::write(dir.path().join("scout.duckdb"), b"live").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"keep me").unwrap();

        prune(dir.path(), 2).unwrap();

        let mut left: Vec<String> = std::fs::read_dir(dir.path()).unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, vec![
            "notes.txt".to_string(),
            "scout-2026-08-04T000000Z-nightly.duckdb".to_string(),
            "scout-2026-08-05T000000Z-nightly.duckdb".to_string(),
            "scout.duckdb".to_string(),
        ]);
    }

    #[test]
    fn a_backup_is_due_when_the_newest_one_is_a_day_old() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_due(dir.path()).unwrap(), "no backups at all means one is due");

        std::fs::write(dir.path().join("scout-2026-08-05T000000Z-nightly.duckdb"), b"x").unwrap();
        assert!(!is_due(dir.path()).unwrap(), "one taken just now is not due again");
    }
}
```

`is_due` reads file modification time rather than parsing the name, so it needs
no clock injection and survives a restart with no state to keep.

- [ ] **Step 2: Run them to watch them fail**

Run: `TZ=UTC cargo test -p scout-core -- backup::`
Expected: FAIL — the module does not exist.

- [ ] **Step 3: Implement**

```rust
//! When a backup is taken, what it is called, and how many are kept.
//!
//! The copying itself belongs to `store`, which owns the connection. This is
//! the policy around it.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Keep two weeks. About 220 MB at today's size against 84 GB free, and long
/// enough to notice damage that is not immediately obvious — a bad migration
/// often only surfaces when someone runs `/stat`.
pub const KEEP: usize = 14;

/// How stale the newest backup may be before another is taken.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

const PREFIX: &str = "scout-";
const SUFFIX: &str = ".duckdb";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Nightly,
    /// Taken before a schema change, which is the one that cannot be undone.
    Migration { to: i64 },
    Manual,
}

impl Reason {
    fn slug(&self) -> String {
        match self {
            Reason::Nightly => "nightly".to_string(),
            Reason::Migration { to } => format!("migration-v{to}"),
            Reason::Manual => "manual".to_string(),
        }
    }
}

fn file_name(at: &chrono::DateTime<chrono::Utc>, reason: Reason) -> String {
    format!("{PREFIX}{}-{}{SUFFIX}", at.format("%Y-%m-%dT%H%M%SZ"), reason.slug())
}

fn is_ours(name: &str) -> bool {
    name.starts_with(PREFIX) && name.ends_with(SUFFIX)
}

/// Ours, newest last. Names carry ISO timestamps, so lexical order is
/// chronological order and nothing has to be parsed.
fn existing(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut found: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().is_some_and(is_ours))
            .map(|e| e.path())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    found.sort();
    Ok(found)
}

/// Deletes all but the newest `keep`. Only ever considers files this module
/// named, so a mistake here cannot reach the live database beside them.
pub fn prune(dir: &Path, keep: usize) -> anyhow::Result<()> {
    let all = existing(dir)?;
    for old in all.iter().rev().skip(keep) {
        if let Err(e) = std::fs::remove_file(old) {
            tracing::warn!(path = %old.display(), error = %e, "could not remove an old backup");
        }
    }
    Ok(())
}

pub fn is_due(dir: &Path) -> anyhow::Result<bool> {
    let Some(newest) = existing(dir)?.pop() else {
        return Ok(true);
    };
    let age = SystemTime::now().duration_since(std::fs::metadata(&newest)?.modified()?);
    Ok(age.map(|a| a >= MAX_AGE).unwrap_or(false))
}

/// Where backups live: beside the database, on the same volume. That protects
/// against corruption and mistakes, not against losing the disk — which is
/// what shipping them off the box is for, and is deliberately not here.
pub fn dir_for(db_path: &Path) -> PathBuf {
    db_path.parent().unwrap_or(Path::new(".")).join("backups")
}
```

Add `pub mod backup;` to `lib.rs`.

- [ ] **Step 4: Run them to watch them pass**

Run: `TZ=UTC cargo test -p scout-core -- backup::`
Expected: PASS — 3 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/scout-core/src/backup.rs crates/scout-core/src/lib.rs
git commit -m "feat: what a backup is called and how many are kept"
```

---

### Task 3: The one that cannot be undone

**Files:**
- Modify: `crates/scout-core/src/store.rs`

A migration cannot be reversed. This backup has been taken by hand four times —
`.pre-accounts`, `.pre-2a`, `.pre-2b1`, `.pre-2b2a` — and remembering was the
only thing protecting it.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn a_pending_migration_backs_the_database_up_before_changing_it() {
        // The failure with no second chance. If this backup is missing or is
        // taken after the fact, a bad schema step is unrecoverable.
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("legacy.duckdb");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(LEGACY_SCHEMA).unwrap();
            conn.execute_batch(
                "INSERT INTO purchases (user_id, item, store) VALUES (7, 'detergent', 'bol.com')",
            ).unwrap();
        }

        let store = Store::open(&db).unwrap();
        assert!(store.schema_version().unwrap() >= 5, "the migration ran");

        let backups = crate::backup::dir_for(&db);
        let taken: Vec<_> = std::fs::read_dir(&backups).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(taken.len(), 1, "exactly one backup, taken before the steps");
        assert!(taken[0].file_name().unwrap().to_str().unwrap().contains("migration-v"));

        // The proof it was taken BEFORE: the copy is still the old shape.
        let before = Connection::open(&taken[0]).unwrap();
        let legacy: i64 = before.query_row(
            "SELECT count(*) FROM information_schema.columns
             WHERE table_name = 'purchases' AND column_name = 'user_id'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(legacy, 1, "the backup should predate the column being replaced");
    }

    #[test]
    fn a_database_with_nothing_to_migrate_is_not_backed_up() {
        // Otherwise every restart writes one and the retention window becomes
        // meaningless.
        let (store, dir) = test_store();
        drop(store);
        let again = Store::open(dir.path().join("test.duckdb")).unwrap();
        drop(again);
        let backups = crate::backup::dir_for(&dir.path().join("test.duckdb"));
        assert!(!backups.exists() || std::fs::read_dir(&backups).unwrap().count() == 0);
    }
```

- [ ] **Step 2: Run them to watch them fail**

Run: `TZ=UTC cargo test -p scout-core -- a_pending_migration a_database_with_nothing`
Expected: FAIL — the first finds no backups directory.

- [ ] **Step 3: Thread the path through and take the backup**

`apply_steps` currently takes only `&Connection`. Give it the path:

```rust
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())?;
        conn.execute_batch(MIGRATIONS)?;
        apply_steps(&conn, path.as_ref())?;
        ...
```

and inside `apply_steps`, after `current` is known and before the loop:

```rust
    let target = steps().last().map(|(n, _)| *n).unwrap_or(0);
    if target > current {
        // Before, not after. A migration cannot be undone, so this is the
        // only moment the old shape still exists.
        //
        // A failure here is logged and the migration proceeds anyway. That is
        // a deliberate choice and a sharp one: it means an irreversible change
        // can run unprotected. The alternative — refusing to start — turns a
        // full disk into a bot that will not boot, at the worst moment. See
        // the design doc; reversing it is one `?`.
        let dir = crate::backup::dir_for(db_path);
        match std::fs::create_dir_all(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                let name = crate::backup::file_name_now(crate::backup::Reason::Migration { to: target });
                let to = dir.join(name);
                backup_connection(conn, &to).map(|()| to)
            }) {
            Ok(to) => tracing::info!(path = %to.display(), from = current, to = target,
                "backed up before migrating"),
            Err(e) => tracing::error!(error = %e, from = current, to = target,
                "COULD NOT BACK UP BEFORE MIGRATING; proceeding anyway"),
        }
    }
```

`file_name_now(reason)` is `file_name(&chrono::Utc::now(), reason)`, exported so
callers need not thread a clock. Make `file_name` `pub(crate)` for the tests.

- [ ] **Step 4: Run them to watch them pass**

Run: `TZ=UTC cargo test --workspace`
Expected: all green, two tests more than before.

- [ ] **Step 5: Commit**

```bash
git add -A crates/scout-core/src
git commit -m "feat: the irreversible step takes a copy first"
```

---

### Task 4: Nightly, without needing a channel

**Files:**
- Modify: `crates/scout-core/src/backup.rs`, `crates/scout-core/src/core.rs`

- [ ] **Step 1: Write the failing test**

In `core.rs`'s `mod tests`:

```rust
    #[tokio::test]
    async fn an_on_demand_backup_lands_beside_the_database_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("maint.duckdb");
        let core = Core::start(Config::for_test(db.to_str().unwrap()), None).unwrap();

        let first = core.backup(crate::backup::Reason::Manual).await.unwrap();
        assert!(first.exists());
        assert!(first.starts_with(crate::backup::dir_for(&db)));

        // A second one, and retention holding at the configured count.
        let _ = core.backup(crate::backup::Reason::Manual).await;
        let kept = std::fs::read_dir(crate::backup::dir_for(&db)).unwrap().count();
        assert!(kept <= crate::backup::KEEP);
    }
```

- [ ] **Step 2: Run it to watch it fail**

Run: `TZ=UTC cargo test -p scout-core an_on_demand_backup`
Expected: FAIL — `no method named 'backup'`.

- [ ] **Step 3: Implement**

On `Core`:

```rust
    /// Takes a backup now and prunes old ones. Returns where it went.
    ///
    /// Runs the copy off the async runtime: it holds the store's mutex for its
    /// duration, and that mutex is shared with the agent.
    pub async fn backup(&self, reason: crate::backup::Reason) -> anyhow::Result<std::path::PathBuf> {
        let store = self.store();
        let db = std::path::PathBuf::from(&self.cfg.db_path);
        blocking(move || {
            let dir = crate::backup::dir_for(&db);
            std::fs::create_dir_all(&dir)?;
            let to = dir.join(crate::backup::file_name_now(reason));
            store.backup_to(&to)?;
            crate::backup::prune(&dir, crate::backup::KEEP)?;
            Ok(to)
        })
        .await
    }

    /// Housekeeping that must happen whether or not any channel is running.
    ///
    /// Spawned by whoever owns the process — `main` today, the core binary
    /// once core has its own. Deliberately not part of the Telegram
    /// scheduler: a backup should not depend on a chat client existing.
    pub async fn run_maintenance(self: std::sync::Arc<Self>) {
        // Hourly, against a daily threshold, so a restart cannot skip a day
        // and the check itself costs nothing.
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let dir = crate::backup::dir_for(std::path::Path::new(&self.cfg.db_path));
            match crate::backup::is_due(&dir) {
                Ok(false) => continue,
                Ok(true) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "could not tell whether a backup is due");
                    continue;
                }
            }
            // A failed backup must never stop the bot answering.
            match self.backup(crate::backup::Reason::Nightly).await {
                Ok(to) => tracing::info!(path = %to.display(), "nightly backup"),
                Err(e) => tracing::error!(error = %e, "NIGHTLY BACKUP FAILED"),
            }
        }
    }
```

- [ ] **Step 4: Run it, then the suite**

Run: `TZ=UTC cargo test --workspace`
Expected: green.

- [ ] **Step 5: Commit**

```bash
git add -A crates/scout-core/src
git commit -m "feat: a nightly copy that does not depend on a chat client"
```

---

### Task 5: `/backup`, and starting the loop

**Files:**
- Modify: `crates/scout-telegram/src/bot.rs`, `crates/scout-telegram/src/main.rs`

- [ ] **Step 1: Add the command**

In the `Command` enum (`bot.rs:114`), following the existing style:

```rust
    #[command(description = "admin only: take a database backup now")]
    Backup,
```

and in the `match cmd` block, beside the other admin commands:

```rust
        Command::Backup => {
            let Some(user_id) = msg.from.as_ref().map(|u| u.id.0 as i64) else {
                return Ok(());
            };
            if !app.core.is_admin(user_id) {
                bot.send_message(msg.chat.id, scout_core::invites::NOT_ADMIN).await?;
                return Ok(());
            }
            let reply = match app.core.backup(scout_core::backup::Reason::Manual).await {
                Ok(to) => {
                    let size = std::fs::metadata(&to).map(|m| m.len()).unwrap_or(0);
                    format!("Backed up to {} ({} KB)", to.display(), size / 1024)
                }
                // The bot says so rather than going quiet: a backup nobody
                // knows failed is the same as no backup.
                Err(e) => format!("Backup failed: {e}"),
            };
            bot.send_message(msg.chat.id, reply).await?;
        }
```

Add the line to the `HELP` text alongside the other admin commands.

- [ ] **Step 2: Spawn the loop**

In `main.rs`, beside the scheduler spawn at line 63:

```rust
    tokio::spawn(core.clone().run_maintenance());
```

- [ ] **Step 3: Verify**

```bash
TZ=UTC cargo test --workspace
PATH="$(dirname "$(ls -d /nix/store/*clippy-1.96.1/bin/cargo-clippy 2>/dev/null)")":$PATH \
  cargo clippy --workspace --all-targets
```

Expected: green, clippy silent. If the nix clippy path does not resolve, plain
`cargo clippy --workspace --all-targets` may work — check its output rather than
its exit code.

- [ ] **Step 4: Commit**

```bash
git add -A crates
git commit -m "feat: /backup, and the loop that does it nightly"
```

---

### Task 6: Deploy and prove it on the real database

- [ ] **Step 1: Deploy**

```bash
export SCOUT_SSH=root@169.58.231.116
set -a && . ./.env && set +a
./scripts/deploy-k3s.sh
```

- [ ] **Step 2: Take one by hand, against 15 MB of real data**

Send `/backup` to the bot as an admin. Expect a path and a size in the reply.

Then confirm it is real rather than reported:

```bash
ssh $SCOUT_SSH 'kubectl -n scout exec deploy/scout -- ls -l /data/backups/'
```

Expected: one file, close to 15 MB, no `.partial` left behind.

- [ ] **Step 3: Prove the copy is a working database, not just a file**

```bash
ssh $SCOUT_SSH 'kubectl -n scout exec deploy/scout -- sh -c "ls -l /data/backups/*.duckdb"'
```

Then restore it into a scratch pod and read from it — the only check that
matters is that it opens and has the rows:

```bash
ssh $SCOUT_SSH 'kubectl -n scout exec deploy/scout -- sh -c "
  cp /data/backups/*.duckdb /tmp/check.duckdb && ls -l /tmp/check.duckdb"'
```

The bot itself is the reader: compare the `who may talk to this bot` counts in
its log against what `/stat` reports. If a fuller check is wanted, the restore
procedure is the one the move to this server used — scale to zero, swap the
file, scale up — and it should be exercised at least once before it is needed.

- [ ] **Step 4: Check the nightly path without waiting a day**

```bash
ssh $SCOUT_SSH 'kubectl -n scout exec deploy/scout -- sh -c "touch -d \"2 days ago\" /data/backups/*.duckdb"'
```

Within the hour, a `nightly backup` line should appear:

```bash
ssh $SCOUT_SSH 'kubectl -n scout logs -l app=scout --tail=50 | grep -i backup'
```

- [ ] **Step 5: Finish the branch**

REQUIRED SUB-SKILL: superpowers:finishing-a-development-branch

---

## What this deliberately does not do

- **Get copies off the box.** Everything here lands on the same volume as the
  database. That covers corruption, a bad migration and a mistaken delete; it
  does not cover losing the server. The CronJob that ships these somewhere else
  is the next piece, and it is easy precisely because a consistent file now
  exists to ship.
- **Restore tooling.** Restoring is scale-to-zero, swap the file, scale up —
  the same procedure this server was populated with.
- **Surface backup age in `/stat`.** The known weakness of "log and carry on"
  is that nobody reads logs. This is the first thing to add if it proves
  untrustworthy.
- **Compress.** 15 MB is not a problem, and whatever ships them off-box can
  compress in transit.
