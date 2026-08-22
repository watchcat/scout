# Scout on k3s — Hosting Design

## Purpose

Production Scout runs in Docker Desktop on a laptop. That was defensible while
everything was outbound-only: the bot dialled Telegram and nothing dialled
back. W1 makes Scout reachable from the internet, which turns the laptop into a
published host with a residential IP, an uptime bounded by the lid, and an
address the ISP may rotate.

This document designs the move: a single Hetzner Cloud server running k3s, with
Scout as its first workload and room for others.

## Why Kubernetes, given that Scout cannot use it

Stated plainly, because the reasoning constrains everything below.

**Scout can never run more than one replica.** DuckDB is single-writer — one
process holds the file and a second gets a lock error — and two clients
long-polling one Telegram bot token get 409 Conflict. So Kubernetes offers this
application no scaling, no rolling deploys and no failover. On the merits of
Scout alone, docker compose is the better answer and it already works.

The reason is the second service, and the third. k3s makes those cheap: one
ingress, one certificate story, one place secrets live, one deploy verb. That
is a real benefit and it arrives later, so this design pays a cost now for it.
It should be honest about that rather than pretending the cost is zero.

## Two things that break silently in the move

Both are things `compose.yaml` currently gets right, and neither has an
automatic Kubernetes equivalent.

**A rolling update collides with itself.** Kubernetes' default strategy starts
the new pod before terminating the old one. For Scout that means two processes
briefly alive: DuckDB refuses the second, and Telegram answers 409 to both. The
Deployment therefore specifies `replicas: 1` and `strategy: Recreate`, with the
reason in a comment — "scale it up" is the obvious thing for a future reader to
try, and the failure would look like a database problem rather than a
scheduling one.

**PID 1 stops reaping.** `compose.yaml` sets `init: true`, and records why:

> Chromium's helper processes (renderer, zygote, crashpad) outlive the process
> we spawn and are reparented to PID 1. Without an init there, scout is PID 1
> and never reaps them: one render left 35 zombies, which ends in an exhausted
> PID table.

`init: true` is a compose feature. In Kubernetes the container's command is PID
1 with nothing reaping behind it, and the exhausted PID table returns. The fix
belongs in the image rather than the orchestrator: `tini` as `ENTRYPOINT`,
which fixes it everywhere at once and lets `init: true` become redundant rather
than load-bearing.

## The server

One Hetzner Cloud **CX32** — 4 vCPU, 8 GB, 80 GB — Ubuntu 24.04, in Nuremberg
or Falkenstein.

8 GB is not padding. The Dockerfile records that DuckDB's C++ compile hangs the
build at 10 jobs against ~8 GB and that `CARGO_BUILD_JOBS=4` fits; a 4 GB box
saves €3 a month and buys an out-of-memory kill in a ten-minute compile. k3s
itself wants roughly 500 MB, Chromium peaks around 1 GB during a render, and
the rest is headroom for the services this exists to make room for.

**Hetzner's automatic backups should be enabled** — 20% of the server price, and
the only thing standing between a disk failure and the purchase history.

## Architecture

```
internet ──► Hetzner firewall (22, 80, 443 only)
                └─► k3s node
                      ├─ traefik        ingress, bundled with k3s
                      ├─ cert-manager   Let's Encrypt HTTP-01
                      └─ scout Deployment (replicas: 1, Recreate)
                            ├─ scout-telegram, one process
                            │    ├─ telegram long poll (outbound)
                            │    └─ scout-web on :8080
                            └─ PVC /data ──► local-path on the node disk
```

**k3s, not kubeadm.** One binary, control plane and agent in one process, and
it does not taint the control-plane node — so workloads schedule there without
the untainting step a kubeadm cluster needs. The Hetzner tutorial that prompted
this uses Terraform to provision separate master and worker servers, which is a
different arrangement and not the one wanted here.

**Traefik as the shared ingress.** It ships with k3s and is already running.
Replacing it with ingress-nginx would be a preference, not an improvement.

**cert-manager rather than Traefik's built-in ACME.** Traefik can get
certificates itself, but cert-manager is the piece the later services will
share, it stores certificates as Secrets that survive a Traefik restart, and
its `Certificate` resources make expiry visible to `kubectl`.

**Storage: k3s's bundled local-path provisioner.** A PVC bound to a directory
on the node's disk. This is honest about what it is — there is one node, so
there is no such thing as a network volume here, and pretending otherwise with
Longhorn on a single machine would add a distributed storage system to protect
against a failure mode (node loss) that also loses the cluster. Backups are
Hetzner's snapshot plus the manual dump procedure below, not a storage layer.

**Secrets: a Kubernetes Secret created from `.env`.** `kubectl create secret
generic scout --from-env-file=.env` keeps `.env` the source of truth, keeps it
out of git, and keeps twelve keys from being retyped. Kubernetes Secrets are
base64, not encryption — anyone with cluster access can read them, which is the
same trust boundary `.env` on the host already has.

The twelve keys: `TELEGRAM_BOT_TOKEN`, `ALLOWED_TELEGRAM_USER_IDS`,
`SCOUT_ADMIN_USER_IDS`, `MINIMAX_API_KEY`, `KAGI_API_KEY`,
`PERPLEXITY_API_KEY`, `DUFFEL_API_KEY`, `IGNAV_API_KEY`, `EBAY_CLIENT_ID`,
`EBAY_CLIENT_SECRET`, `EBAY_MARKETPLACE`, `SCOUT_DOMAIN`.

**The image is built on the box and imported into containerd.** No registry, no
credentials, no second service — and the box is sized for the build anyway.
`docker build`, then `docker save | k3s ctr images import -`, tagged with the
git SHA so a rollback is a tag change rather than a rebuild. The alternative,
pushing to ghcr.io, is cleaner for multi-node and pointless for one.

## What W1's work becomes

**Kept, untouched:** everything in Rust. `scout-web`, the page, the admission
cache, `Core::admission`, the graceful shutdown on SIGTERM. That is the part
that mattered and none of it knows what an orchestrator is.

**Superseded:** the `caddy` compose service and the `Caddyfile`. Traefik and
cert-manager do that job. They stay in git history.

**Kept and still useful:** `compose.yaml` remains the way to run Scout locally.
Its `init: true` becomes redundant once `tini` is the image's entrypoint, but
harmless, and removing it would make local runs depend on the image being
rebuilt.

**Rewritten:** `scripts/deploy.sh`. It currently builds, drains and hands over
via compose, and checks that every service came up. The k3s equivalent builds,
imports, applies, and waits for the rollout — and must keep the property it
gained last: a deploy that reports success has to mean the whole thing is
running, not just that one line appeared in one log.

## Failure handling

**A certificate that will not issue.** cert-manager retries with backoff and
the `Certificate` resource says why. Scout is unaffected — it does not know
about TLS. The site is unreachable; the bot keeps working. Let's Encrypt's rate
limit is the reason DNS is checked before the first apply and not after.

**The node reboots.** k3s restarts as a systemd service, the Deployment
reschedules, the PVC rebinds to the same directory. Nothing is lost, and the
bot reconnects to Telegram on its own.

**The disk fills.** Chromium and containerd both write; 80 GB is generous but a
runaway log would take the database's disk with it. Container logs are capped
by k3s's containerd defaults; this is called out as something to watch, not
solved here.

**A bad deploy.** `kubectl rollout undo` reverts to the previous ReplicaSet,
whose image tag is still in containerd because tags are git SHAs. The database
is not versioned with it — a deploy that migrated the schema is not undone by
rolling back the image, which is exactly the situation the pre-deploy backup
exists for.

## Migration

Order matters, and one step is unforgiving.

1. Create the server, enable backups, apply a Hetzner firewall allowing only
   22, 80 and 443. **k3s serves its API on 6443 and binds it to all
   interfaces** — leaving that reachable would publish cluster admin.
2. Install k3s, cert-manager, and the Scout manifests, with the Deployment
   scaled to zero. Nothing is running yet, so nothing conflicts.
3. **Stop the laptop's container.** Two processes long-polling one bot token
   get 409 and neither works reliably. This is the unforgiving step: it must
   happen before the cluster's copy starts, and it means a few minutes with no
   bot at all.
4. Copy the database out **without starting the bot again**. It has to be read
   from inside a container — the host's DuckDB is older than the one that wrote
   the file and must not open it — but starting the service would resume
   polling. `docker compose run --rm --entrypoint sh scout` mounts the same
   volume and runs a shell instead of the bot, so the file can be read with
   nothing connecting to Telegram. Copying before stopping would also work and
   would lose any message that arrived in between; this loses nothing.
5. Load it into the PVC, scale the Deployment to one, and confirm the bot
   answers.
6. Point `buzz-bot.top` at the new IP, grey cloud in Cloudflare so the ACME
   HTTP-01 challenge reaches Traefik directly. The proxy can be turned on
   afterwards, once a certificate exists.

## Testing

There is no unit test for a cluster, so verification is a checklist run against
the real thing, and it is written as one in the plan:

- the site answers on HTTPS with a real certificate, and HTTP redirects to it;
- port 6443 is not reachable from off-host;
- the bot answers an ordinary question, `/stat`, and `/invite status`;
- the row counts in the migrated database match the laptop's, table by table;
- `kubectl delete pod` reschedules and the data is still there;
- a deploy of an unchanged image reports success, and a deliberately broken one
  reports failure rather than exiting 0.

## Deferred

- **Anything for the second service.** The ingress and cert-manager are set up
  to be shared, but no second workload is designed here.
- **Off-box backups.** Hetzner's snapshots protect against disk failure, not
  against deleting the wrong namespace. A periodic dump to somewhere else is
  wanted and is not in this scope.
- **Monitoring.** No Prometheus, no alerting. `kubectl` and `docker compose
  logs`' successor are the whole story for now.
- **GitOps.** Manifests are applied by the deploy script from a checkout, not
  reconciled by Flux or Argo. That is the right size for one service and the
  wrong size for ten; revisit when the count is closer to ten.
