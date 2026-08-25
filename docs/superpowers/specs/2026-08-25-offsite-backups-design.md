# Off-site Backups — Design

## Purpose

Scout now takes consistent copies of its own database, and they land in
`/data/backups/` — the same PVC, the same disk, the same server as the
database they protect. That covers a bad migration, corruption and a mistaken
delete. It covers nothing about losing the disk, the server, or the account.

This gets copies off the box.

## Where they go

**Cloudflare R2**, an S3-compatible object store.

Google Drive was considered first and rejected on one specific ground:
unattended Drive access from a server means an OAuth refresh token, because a
personal account has no Shared Drive and a service account has no storage quota
of its own. Those tokens can be revoked and can expire, and when they do the
uploads stop silently — which is the failure mode this whole design is trying
to avoid. R2 uses API tokens that do not expire on a schedule, has no
rate-limit surprises for many small objects, and charges no egress, which
matters on the day the backups are actually needed.

## What does the copying

**restic**, writing directly to R2 over its S3 backend.

restic was chosen over `rclone` + `crypt` because on real object storage it
gives three things in one tool that would otherwise be three:

- **Client-side encryption**, always on. The database holds purchase history,
  delivery addresses and whole conversations; R2 must never hold that
  readable. restic encrypts before anything leaves the server, so the bucket
  contains opaque blobs.
- **Deduplication.** Fourteen daily copies of a 9 MB database that changes a
  little each day cost far less than fourteen times 9 MB.
- **Retention.** `restic forget --keep-daily 14 --prune` expresses the policy
  the local copies already follow, in one command.

The trade is a repository password that must be recorded somewhere durable.
Lose it and every backup is unreadable — permanently, by design. That is the
cost of the encryption being real.

## What is uploaded

The **consistent copies in `/data/backups/`**, not the live database.

This matters and is easy to get backwards. `/data/scout.duckdb` is open and
being written; an upload of it would be crash-consistent at best, which is
exactly what the previous phase existed to stop settling for. The files in
`/data/backups/` were produced by `COPY FROM DATABASE` from the live
connection, with the write-ahead log folded in. They are the trustworthy
artefact, and they are what gets shipped.

## Shape

```
CronJob (daily)
  ├─ mounts the same PVC the bot uses          (RWO, same node, so this works)
  ├─ restic backup /data/backups
  ├─ restic forget --keep-daily 14 --prune
  └─ on success: touch /data/backups/.uploaded
```

A `CronJob` rather than another loop inside Scout, deliberately. Uploading is
not something only the database's owner can do — unlike taking the copy, which
is. Keeping it outside means restic's credentials never enter the bot's
process, a stuck upload cannot block the agent, and the schedule is visible
to `kubectl` rather than buried in a Rust loop.

**The PVC is mounted read-write, not read-only**, solely so the job can write
its `.uploaded` marker. It has no reason to touch anything else, and the risk
of it doing so is the reason that marker is a file rather than the job
deleting local backups too — pruning stays with the process that created them.

## Detecting silent failure

This is the part the design exists around, because a backup system that fails
quietly is worse than none: it converts a known risk into an assumed safety.

**The Job fails rather than exiting 0.** restic's exit code propagates, so a
failed upload leaves a failed Job that `kubectl get jobs -n scout` shows, and
`failedJobsHistoryLimit` keeps the evidence.

**`/stat` reports the age of both copies.** The admin view already exists and
is already looked at. It gains two lines: how old the newest local backup is,
and how old the newest *uploaded* one is.

The second comes from the `.uploaded` marker rather than from asking R2. That
keeps core ignorant of where backups go — it reports a fact about its own
filesystem, and the CronJob is what asserts that fact by touching the file only
after restic succeeds. Core never learns what an object store is, which is the
same boundary the last three phases were spent drawing.

## What has to exist before this can run

Created by a human, because they cost money and carry credentials:

- An R2 bucket.
- An R2 API token with object read and write on that bucket.
- A restic repository password, recorded somewhere durable that is **not** this
  server.

These become one Kubernetes Secret. As with the bot's own secret, it is created
from values piped in rather than committed, and nothing in `deploy/k8s/` holds
a credential.

## Failure handling

**R2 unreachable.** The Job fails, the evidence stays, `/stat` shows the
uploaded age growing. Local backups are unaffected — they are what the bot
takes for itself and do not depend on this.

**The repository is corrupted or the password is wrong.** restic says so and
fails. `restic check` is worth running by hand occasionally; automating it is
deferred, because a check that runs unattended and is never read adds nothing.

**The bucket fills or billing lapses.** Same as unreachable: visible failure,
local copies unaffected.

**The marker lies.** If someone touches `.uploaded` by hand, `/stat` reports a
freshness that is not real. That is accepted: the marker is a convenience for
the common case, and the Job's own history is the authority.

## Testing

A cluster cannot be unit-tested, so verification is a checklist against the
real thing, in the plan:

- A manual Job run uploads and the bucket shows objects.
- Restoring into a scratch location produces a database with the same row
  counts as the source — the only test that matters, and the one nobody runs
  until it is too late.
- A deliberately broken credential makes the Job fail rather than exit 0.
- `/stat` shows both ages, and the uploaded age moves after a successful run.

The restore test is the point. An untested backup is a hypothesis.

## Deferred

- **`restic check` on a schedule.** Worth it once someone is reading the
  output.
- **A second destination.** One off-site copy is the large step; a second is a
  refinement.
- **Alerting beyond `/stat`.** Telegram messages on failure were considered
  and rejected: it would make the backup path depend on the chat channel, which
  is the coupling core has spent three phases removing.
