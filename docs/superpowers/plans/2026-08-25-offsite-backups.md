# Off-site Backups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The consistent copies Scout takes of itself end up somewhere that is not this server, encrypted, with a way to notice when they stop.

**Architecture:** A daily `CronJob` runs `restic` against Cloudflare R2, uploading `/data/backups/` — never the live database. On success it touches a marker, which `/stat` reads so an admin sees how stale the off-site copy is without core ever learning what an object store is.

**Tech Stack:** restic, Cloudflare R2 (S3-compatible), a Kubernetes CronJob. One Rust change, in `stats.rs`.

---

## Tasks 1–2 need no credentials

The `/stat` change and the manifest are writable and reviewable now. Tasks 3–5
need an R2 bucket and its token, which a human creates.

## Verified before writing this plan

| Claim | How it was checked | Result |
|---|---|---|
| Backups are real databases | pulled the live one, opened it | schema 5; 9 purchases, 11 facts, 4 trips, 7 accounts, 106 messages |
| A backup is smaller than the source | `ls -l` on both | 9,187,328 vs 15,478,784 — `COPY FROM DATABASE` compacts |
| The maintenance loop works unattended | pod log, first tick after deploy | `nightly backup path=/data/backups/scout-2026-08-25T010611Z-nightly.duckdb` |
| Where backups live on the host | `ls` on the node | `/var/lib/rancher/k3s/storage/pvc-…_scout_scout-data/backups/` |
| Two pods can share the PVC | k3s is one node; RWO permits it per-node | the CronJob can mount what the Deployment mounts |

**The number worth remembering:** a backup is *smaller* than the live file, and
that is correct rather than alarming. `COPY FROM DATABASE` writes a fresh
compacted database instead of copying accumulated free space.

---

### Task 1: `/stat` says how old the copies are

**Files:**
- Modify: `crates/scout-core/src/stats.rs`, `crates/scout-core/src/backup.rs`

The point of this task is that a silent failure stops being silent. `/stat` is
where an admin already looks.

- [ ] **Step 1: Write the failing test**

In `backup.rs`'s `mod tests`:

```rust
    #[test]
    fn freshness_reports_both_copies_and_says_when_there_are_none() {
        let dir = tempfile::tempdir().unwrap();
        let none = freshness(dir.path());
        assert_eq!(none.newest_local, None, "no local backups yet");
        assert_eq!(none.newest_uploaded, None, "nothing has been uploaded");

        std::fs::write(dir.path().join("scout-2026-08-05T000000Z-nightly.duckdb"), b"x").unwrap();
        let local_only = freshness(dir.path());
        assert!(local_only.newest_local.is_some());
        assert_eq!(local_only.newest_uploaded, None,
            "a local backup is not an uploaded one, and the difference is the whole point");

        std::fs::write(dir.path().join(UPLOADED_MARKER), b"").unwrap();
        let both = freshness(dir.path());
        assert!(both.newest_uploaded.is_some());
    }
```

- [ ] **Step 2: Run it to watch it fail**

Run: `TZ=UTC cargo test -p scout-core freshness_reports_both`
Expected: FAIL — `cannot find function 'freshness'`.

- [ ] **Step 3: Implement**

In `backup.rs`:

```rust
/// Touched by whatever ships backups off the box, only after it has
/// succeeded. Its age is how stale the off-site copy is.
///
/// Deliberately a file rather than core asking the destination: core does not
/// know what an object store is, and should not have to in order to say when
/// something last worked.
pub const UPLOADED_MARKER: &str = ".uploaded";

/// How old each copy is. `None` means there is not one at all, which reads
/// very differently from "old" and should never be collapsed into it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freshness {
    pub newest_local: Option<Duration>,
    pub newest_uploaded: Option<Duration>,
}

fn age_of(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Never fails: a report about backups must not be the thing that breaks the
/// admin's view of everything else.
pub fn freshness(dir: &Path) -> Freshness {
    Freshness {
        newest_local: existing(dir).ok().and_then(|mut v| v.pop()).and_then(|p| age_of(&p)),
        newest_uploaded: age_of(&dir.join(UPLOADED_MARKER)),
    }
}
```

- [ ] **Step 4: Put it in `/stat`**

`stats.rs::report` gains a line for admins only — the same audience that gets
everyone's numbers. Read how `report` decides that before writing it.

```rust
    // Where an admin already looks, so a backup that quietly stopped is
    // visible without anyone reading logs.
    let f = crate::backup::freshness(&crate::backup::dir_for(std::path::Path::new(&db_path)));
    out.push_str(&format!(
        "\nbackups: local {}, off-site {}\n",
        describe_age(f.newest_local),
        describe_age(f.newest_uploaded),
    ));
```

with

```rust
/// "never" is not "a long time ago". A backup that has never been taken and
/// one that is a month stale need different reactions, and a duration cannot
/// express the first.
fn describe_age(age: Option<std::time::Duration>) -> String {
    match age {
        None => "never".to_string(),
        Some(d) if d.as_secs() < 3600 => format!("{}m ago", d.as_secs() / 60),
        Some(d) if d.as_secs() < 86400 => format!("{}h ago", d.as_secs() / 3600),
        Some(d) => format!("{}d ago", d.as_secs() / 86400),
    }
}
```

`report` will need the database path; check whether it already has access to
`Core` and take it from `cfg` rather than threading a new argument if so.

- [ ] **Step 5: Test and commit**

Run: `TZ=UTC cargo test --workspace`

```bash
git add crates/scout-core/src
git commit -m "feat: /stat says how old both copies are, and says never when there are none"
```

---

### Task 2: The CronJob

**Files:**
- Create: `deploy/k8s/backup-cronjob.yaml`

- [ ] **Step 1: Write it**

```yaml
# Ships the consistent copies Scout takes of itself to Cloudflare R2.
#
# A CronJob rather than another loop inside the bot, deliberately: uploading
# is not something only the database's owner can do — unlike taking the copy,
# which is. Keeping it out here means restic's credentials never enter the
# bot's process, a stuck upload cannot block the agent, and the schedule is
# visible to kubectl instead of buried in a Rust loop.
apiVersion: batch/v1
kind: CronJob
metadata:
  name: scout-offsite-backup
  namespace: scout
spec:
  # After the bot's own nightly copy has had time to land.
  schedule: "30 3 * * *"
  concurrencyPolicy: Forbid
  # The evidence of failure is the point of this job, so keep more failures
  # than successes.
  successfulJobsHistoryLimit: 3
  failedJobsHistoryLimit: 10
  jobTemplate:
    spec:
      backoffLimit: 2
      template:
        spec:
          restartPolicy: Never
          containers:
            - name: restic
              image: restic/restic:0.17.3
              envFrom:
                - secretRef:
                    name: scout-offsite
              command: ["/bin/sh", "-c"]
              args:
                - |
                  set -e
                  # First run only. `init` on an existing repository fails,
                  # and that failure is not interesting, so it is swallowed —
                  # but only this one, which is why `set -e` is on above.
                  restic init || true
                  # /data/backups, never /data/scout.duckdb. The live file is
                  # open and being written; these were produced by COPY FROM
                  # DATABASE with the write-ahead log folded in.
                  restic backup --host scout /data/backups
                  restic forget --keep-daily 14 --prune
                  # Only after everything above succeeded. /stat reads this
                  # file's age as "how stale is the off-site copy".
                  touch /data/backups/.uploaded
              volumeMounts:
                # Read-write only so the marker can be written. Pruning local
                # backups stays with the process that created them.
                - { name: data, mountPath: /data }
          volumes:
            - name: data
              persistentVolumeClaim: { claimName: scout-data }
```

- [ ] **Step 2: Check it parses and substitutes**

```bash
python3 -c 'import sys,yaml; yaml.safe_load(open("deploy/k8s/backup-cronjob.yaml")); print("ok")'
grep -c '\${' deploy/k8s/backup-cronjob.yaml   # expect 0 — nothing to substitute
```

- [ ] **Step 3: Commit**

```bash
git add deploy/k8s/backup-cronjob.yaml
git commit -m "feat: a nightly job that puts the copies somewhere else"
```

---

### Task 3: The bucket and the secret *(needs a human)*

- [ ] **Step 1: Create the bucket and token**

In the Cloudflare dashboard: R2 → create a bucket, e.g. `scout-backups`. Then
an R2 API token with **Object Read & Write**, scoped to that bucket. Note the
account ID, the access key ID and the secret.

- [ ] **Step 2: Choose a repository password and record it somewhere else**

```bash
openssl rand -base64 32
```

**Put it in a password manager before continuing.** Everything uploaded is
encrypted with it. Lose it and every backup is unreadable — that is the
encryption working, not failing.

- [ ] **Step 3: Create the Secret, without writing it to disk**

```bash
export SCOUT_SSH=root@169.58.231.116
read -rs -p "R2 access key id: " R2_KEY;    echo
read -rs -p "R2 secret:        " R2_SECRET; echo
read -rs -p "restic password:  " R_PASS;    echo
read -r  -p "R2 account id:    " R2_ACCT
read -r  -p "bucket:           " R2_BUCKET

printf 'AWS_ACCESS_KEY_ID=%s\nAWS_SECRET_ACCESS_KEY=%s\nRESTIC_PASSWORD=%s\nRESTIC_REPOSITORY=s3:https://%s.r2.cloudflarestorage.com/%s\n' \
  "$R2_KEY" "$R2_SECRET" "$R_PASS" "$R2_ACCT" "$R2_BUCKET" \
| ssh -o BatchMode=yes "$SCOUT_SSH" \
    'kubectl -n scout create secret generic scout-offsite --from-env-file=/dev/stdin --dry-run=client -o yaml | kubectl apply -f -'

unset R2_KEY R2_SECRET R_PASS
```

Piped, never written to a file, and `read -rs` keeps the values off the
terminal and out of shell history.

restic reads R2 through its S3 backend, so the credentials use the `AWS_*`
names — that is not a copy-and-paste error.

- [ ] **Step 4: Confirm the keys landed, without printing values**

```bash
ssh $SCOUT_SSH 'kubectl -n scout get secret scout-offsite -o jsonpath="{.data}" | tr "," "\n" | grep -oE "\"[A-Z_]+\"" | tr -d "\""'
```

Expected: the four names, no values.

---

### Task 4: Prove it, including the restore *(needs the box)*

- [ ] **Step 1: Apply and run it by hand rather than waiting for 03:30**

```bash
ssh $SCOUT_SSH 'kubectl apply -f -' < deploy/k8s/backup-cronjob.yaml
ssh $SCOUT_SSH 'kubectl -n scout create job --from=cronjob/scout-offsite-backup offsite-manual-1'
ssh $SCOUT_SSH 'kubectl -n scout wait --for=condition=complete job/offsite-manual-1 --timeout=600s'
ssh $SCOUT_SSH 'kubectl -n scout logs job/offsite-manual-1'
```

Expected: `restic init` on the first run, a snapshot summary, and a `forget`
that removes nothing yet.

- [ ] **Step 2: The test that actually matters — restore it**

An untested backup is a hypothesis. This is the step people skip and then
regret.

```bash
ssh $SCOUT_SSH 'kubectl -n scout run restic-restore --rm -i --restart=Never \
  --image=restic/restic:0.17.3 --overrides="{\"spec\":{\"containers\":[{\"name\":\"restic-restore\",\"image\":\"restic/restic:0.17.3\",\"stdin\":true,\"tty\":false,\"envFrom\":[{\"secretRef\":{\"name\":\"scout-offsite\"}}],\"command\":[\"/bin/sh\",\"-c\"],\"args\":[\"restic snapshots && restic restore latest --target /tmp/r && ls -l /tmp/r/data/backups/\"]}]}}"'
```

Expected: a snapshot list, and the restored `.duckdb` files at roughly the
sizes the source has.

Then verify a restored file is a working database with the right contents, the
same way the local backup was verified: copy it into the bot's volume under a
scratch name and open it. The counts to expect, from the verification already
run against the live data:

```
schema 5 · purchases 9 · user_facts 11 · trips 4 · reminders 1
accounts 7 · identities 7 · messages 106
```

Anything that opens but reports different counts is a failure, not a variation.

- [ ] **Step 3: Prove a broken credential fails loudly**

```bash
ssh $SCOUT_SSH 'kubectl -n scout create job --from=cronjob/scout-offsite-backup offsite-broken-1 --dry-run=client -o yaml \
  | sed "s/name: scout-offsite/name: scout-offsite-does-not-exist/" | kubectl apply -f -'
ssh $SCOUT_SSH 'kubectl -n scout get jobs'
```

Expected: the job fails and stays visible. A backup system that fails quietly
is worse than none, because it turns a known risk into an assumed safety.

- [ ] **Step 4: Check `/stat`**

Send `/stat` to the bot as an admin. Expect a line reading roughly
`backups: local 2h ago, off-site 5m ago`.

Then confirm it is telling the truth rather than echoing a constant: the
off-site age should move after the next successful run, and reading `never`
before the first one is correct.

- [ ] **Step 5: Deploy and finish the branch**

```bash
./scripts/deploy-k3s.sh
```

REQUIRED SUB-SKILL: superpowers:finishing-a-development-branch

---

## What this deliberately does not do

- **`restic check` on a schedule.** Verifying repository integrity is worth
  automating once somebody is reading the result. Until then it is a job that
  succeeds unread and proves nothing.
- **A second destination.** One off-site copy is the large step.
- **Alerting beyond `/stat`.** Telegram messages on failure would make the
  backup path depend on the chat channel, which is the coupling core has spent
  three phases removing.
- **Restoring automatically.** Restore stays a deliberate human act. The
  procedure is scale to zero, put the file in place, scale up — the same one
  that populated this server.
