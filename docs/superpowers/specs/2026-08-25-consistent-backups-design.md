# Consistent Backups — Design

## Purpose

Scout's database now lives on one disk on one VPS. The nine migrated tables —
purchase history, trips, profiles, conversations — exist nowhere else except a
frozen copy on a laptop that staled the moment the move finished.

This designs the part only Scout can do: producing a *consistent* copy of its
own database, without downtime, so that something else can ship it off the box.

## The constraint everything follows from

**DuckDB is single-writer, and Scout holds the only connection.** No external
process can open `/data/scout.duckdb` while the bot runs. So every backup taken
from outside — `cp`, a volume snapshot, a provider's block-level backup — is
*crash-consistent* at best: it captures whatever was on disk mid-flight and
relies on WAL replay to recover, exactly like a power cut.

That is not theoretical here. The live database has a **66 KB write-ahead log**
sitting beside it. A copy of `scout.duckdb` alone loses everything in it, and a
copy of both files taken with `cp` can still tear, because `cp` is not atomic
against a writer.

The only thing that can take a consistent copy is the process holding the
connection. Hence this.

## Verified before designing

`ATTACH … ; COPY FROM DATABASE … TO … ; DETACH` was run against the DuckDB this
project bundles, from a connection with the source database open:

```
PROBE: COPY FROM DATABASE accepted
PROBE: backup opened independently, rows = 2
```

So the mechanism exists, needs no checkpoint, and produces a file that opens on
its own. The design rests on a measurement rather than on documentation.

## What it does

Three triggers, one mechanism.

**Before every migration.** The migration runner takes a backup before applying
any pending step. This is the failure that has actually happened repeatedly: the
`.pre-accounts`, `.pre-2a`, `.pre-2b1` and `.pre-2b2a` copies were all taken by
hand, one at a time, and remembering is the only thing that stood between a
schema change and an unrecoverable mistake. A migration cannot be undone.

**Nightly.** A maintenance loop notices the newest backup is over a day old and
takes one. Routine damage — a bad delete, a corrupted row — then costs a day
rather than everything since the last schema change.

**On demand.** An admin-only `/backup` command, so a risky manual operation can
be preceded by a known-good copy. It also makes the feature exercisable from a
chat window rather than only by waiting for midnight.

## Where the code goes

```
crates/scout-core/src/
  store.rs        backup_to(path)  — the three statements, nothing else
  backup.rs       naming, retention, "is one due", the Core methods
  config.rs       (unchanged; the directory derives from db_path)
crates/scout-telegram/src/
  bot.rs          the /backup command, admin-gated
  main.rs         spawns the maintenance loop
```

**`Core` owns the schedule, not the channel.** The existing 15-minute scheduler
lives in the Telegram adapter, and putting backups there would make them depend
on a chat client existing — which stops being true in W4, when core becomes its
own process. Instead `Core` exposes `run_maintenance(self: Arc<Core>)`, an
async loop that `main` spawns alongside the scheduler today and the core binary
spawns tomorrow, unchanged.

## Mechanics

**Location.** `/data/backups/`, beside the database on the same PVC, created if
absent. Same disk as the original, which protects against corruption and
mistakes but not against losing the volume — that is what shipping them off-box
is for, and it is deliberately not in this scope.

**Naming.** `scout-<RFC3339 UTC>-<reason>.duckdb`, for example
`scout-2026-08-25T021500Z-nightly.duckdb`. The reason is part of the name
because the question at restore time is "which one, and why was it taken" —
`-migration-v5-to-v6` is worth more than a timestamp alone. ISO timestamps sort
lexically, so ordering never needs to parse anything.

**Atomicity.** Written as `.partial` and renamed on success. A crash mid-copy
leaves a file that is obviously incomplete rather than one that looks restorable
and is not.

**The database name is discovered, not assumed.** `COPY FROM DATABASE <name>`
needs the source's identifier, which DuckDB derives from the filename — `scout`
in production, a random temp name under test. The implementation asks
`SELECT current_database()` rather than hardcoding, or every test breaks and
production quietly depends on a filename.

**Retention: keep the newest 14, delete the rest.** About 220 MB at today's
size against 84 GB free. Two weeks is long enough to notice damage that is not
immediately obvious — a bad migration often only surfaces when someone runs
`/stat`. Pruning only ever considers files matching the naming pattern, so
nothing else in the directory can be deleted by a bug in a glob.

**Cost.** The copy holds the store's mutex for its duration, blocking the agent.
At 15 MB the probe completed imperceptibly. This is called out because it scales
with the database: at a gigabyte it would be a visible pause, and the answer
then is to take backups from a read-only replica, which DuckDB cannot do — so
the real answer would be a different database.

## Failure handling, and one tension worth naming

**A failed backup logs at ERROR and the bot carries on.** A backup that fails is
bad; a bot that stops answering because a backup failed is worse.

**That decision has a sharp edge on the migration path, and it should be said
plainly.** If the pre-migration backup fails and the migration proceeds anyway,
an irreversible schema change runs unprotected — which is the exact scenario the
pre-migration backup exists to prevent. The alternative, refusing to start when
the backup fails, trades that for a bot that will not boot on a full disk, at
the worst possible moment.

This design takes the chosen path — log and continue — and records the
consequence rather than hiding it. Changing it later is one `?` in the migration
runner. What makes the risk tolerable is that the migration failure it guards
against is rare and deliberate (it only happens when a new schema step ships),
while a full disk is the common cause of a failed backup and would otherwise
take the bot down for something it could have survived.

**Visibility is the real weakness.** Nobody reads logs, so a persistently
failing backup is invisible until it is needed — which is how backups usually
fail. Surfacing the age of the newest backup in `/stat` was considered and left
out of this scope; it is the first thing to add if this proves untrustworthy.

## Testing

- A backup taken from a live connection opens independently and has the same row
  counts, table by table.
- The source database keeps working during and after — the connection is not
  disturbed by the attach.
- Retention keeps exactly the newest N and deletes the oldest, and leaves files
  that do not match the naming pattern alone.
- Opening a database with a pending migration produces a backup *before* the
  schema changes: the backup, opened afterwards, is still at the old version.
- A backup failure is logged and does not propagate — the caller carries on.

The migration test is the one that matters most, because it is the case with no
second chance.

## Deferred

- **Getting copies off the box.** A CronJob running something like `restic` to
  object storage, encrypted with retention. That is the layer that survives
  losing the server, and it is straightforward once a consistent file exists to
  ship — which is what this provides.
- **Restore tooling.** Restoring is `kubectl scale --replicas=0`, copy the
  chosen file over `scout.duckdb`, scale back up — the same procedure the move
  to this server used, and it is documented in that plan. A command that
  automates it can wait until it has been needed once.
- **Backup age in `/stat`.** See above.
- **Compression.** DuckDB files compress well and 15 MB is not a problem.
  Whatever ships them off-box can compress in transit.
