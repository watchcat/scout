# Chromium in a Pod of Its Own — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A second Deployment, `scout-browser`, renders pages for Scout and holds nothing of Scout's: no secret, no service-account token, no database. A NetworkPolicy lets in Scout's pod only, and lets out DNS to CoreDNS and TCP to the public internet only. Production renders go through it; a local run keeps starting its own Chrome.

**Architecture:** The `scout-browser` binary, already in the workspace, gains `POST /render` and `GET /healthz`. The render is `scout_core`'s `Renderer` unchanged, behind the same guard and the same two slots. Scout's `Renderer` gets a remote form, chosen when `SCOUT_BROWSER_URL` is set, and treats what the pod answers as untrusted: capped, no redirects followed. The deploy reads the node's address and the pod range from the cluster, applies and proves the fence before Scout is pointed at the pod, and fails when the fence is not what the manifest says.

**Tech Stack:** Rust (axum 0.8, tokio, reqwest), Debian's Chromium (already in the image), Kubernetes NetworkPolicy on k3s (kube-router's controller, inside the k3s process), shell.

**Spec:** `docs/superpowers/specs/2026-11-17-price-watch-and-shop-browsing-design.md`, Part 2. Board card: "Chromium in a pod of its own".

This replaces a plan written on 2026-11-20 by another session, never committed. Two of its tasks have landed since: the crate is a workspace member, and the render writes its preferences into the profile. Its measurements on the node are kept below.

## What is known about the node

Measured 2026-11-20 by the earlier plan's session, in a scratch namespace that was deleted afterwards:

- NetworkPolicy is enforced. k3s runs with default flags, and kube-router's policy controller runs inside the k3s process.
- An ipBlock on kube-dns's ClusterIP does not let DNS through: the ClusterIP is translated to CoreDNS's pod address before the policy is read. An ipBlock on CoreDNS's pod address does.
- The node's own address is a public one, and the k3s API and SSH answer there. A rule that allows "the public internet, private ranges excepted" therefore reaches both. With the node's address excepted as well, neither is reached.
- An egress rule written `- to:` with nothing under it allows everything.
- Chromium renders from inside a fenced pod.

Checked again 2026-11-24, read-only: k3s v1.36.3, no `--disable-network-policy`, kube-router's chains present. The node has a public InternalIP and no ExternalIP. The node's pod range is 10.42.0.0/24, and kube-dns sits on 10.43.0.10. The node has 4 vCPUs, 7.9 GB of memory and about 6 GB available. No NetworkPolicy exists in `scout`.

The node's address appears in no file of this repository. The deploy reads it from the cluster each time.

## Decisions

- **DNS goes to CoreDNS by selector**: `namespaceSelector` kube-system plus `podSelector` `k8s-app: kube-dns`, on UDP and TCP 53. A selector follows the pod when it is rescheduled, and the address translation happens before the rule is read, which is why an ipBlock on the ClusterIP failed. Not measured yet: the fence script proves it, and if it fails, the fallback is port 53 to the node's pod range, read from the cluster.
- **The public internet is TCP only.** Chrome reaches the network only through the guard's SOCKS proxy, which carries TCP. WebRTC is already told to send no UDP of its own. A rule that allows no UDP at all means a Chrome that ignores both tellings still sends no datagram out of the pod. This is "the network policy covering UDP" from the "Before anything calls scout-browser" card.
- **The excepted ranges** are 10/8, 172.16/12, 192.168/16, 169.254/16 (the cloud's metadata service among them), 100.64/10, 127/8, 0/8, 224/4, 240/4 and the node's own address.
- **The pod refuses what is not a web address on the public internet, before a Chrome starts.** Scout checks this already, but the pod is reached by HTTP and must not take a caller's word for it. A `file:` address handed to Chrome's `--dump-dom` prints the file. This check lives in `Renderer::render`, so a local render has it too, and the error is `BrowserError::Refused`.
- **Scout reads the pod's answer as untrusted.** If the pod were taken over, its answer would be the attacker's. The body is capped at 16 MB, no redirect is followed (a redirect would turn Scout's own client into a way into the cluster), and no environment proxy is used.
- **`SCOUT_BROWSER_URL` is read in `Config`**, and an address that is not http(s) stops the process at start. When it is set, Scout never starts a Chrome of its own for a render: a misconfigured pod must not quietly put Chromium back in Scout's pod.
- **`scout-browser` listens beyond loopback only when told what fences it**: `SCOUT_BROWSER_BIND` not on loopback is refused unless `SCOUT_BROWSER_FENCE` names the fence. Only the pod's manifest sets it, and it is logged at start. A laptop that sets a bind address by mistake still refuses.
- **SIGTERM ends the server as Ctrl-C does**, so a rollout lets the renders in flight finish (`terminationGracePeriodSeconds: 60`, above the 45-second render ceiling). The rollout starts the new pod before stopping the old one, so there is no gap in rendering.
- **The sessions endpoints are served in the pod as well.** Nothing calls them, and only Scout's pod can reach them. The work listed on "Before anything calls scout-browser" is still owed before anything does.
- **`probe`, a mode of the binary**, is what the fence script asks with, since the image has no curl and no nc. `probe tcp <host:port>`, `probe dns <name>`, `probe udp <ip:port>` (a DNS question sent to that address, with the answer awaited) and `probe render <url>` (a render asked of `SCOUT_BROWSER_URL`). Each check is asked from the browser pod and, as a control, from Scout's pod, which has no policy. A check that must fail counts only when its control succeeds. Otherwise a probe that can never succeed would prove the fence by proving nothing.
- **`--no-sandbox` stays.** Whether Chromium's own sandbox works in this pod is one measurement, not a condition of this card.

## File map

| File | What changes |
|---|---|
| `crates/scout-core/src/tools/browser.rs` | `Renderer` gets two forms: local, and remote over HTTP. `BrowserError::Refused`. A render refuses a non-web or non-public address before its Chrome starts |
| `crates/scout-core/src/config.rs` | `browser_url: Option<Url>` from `SCOUT_BROWSER_URL` |
| `crates/scout-core/src/core.rs` | the renderer is chosen from the config |
| `crates/scout-core/src/lib.rs` | `pub mod render`: the renderer and nothing else of `tools` |
| `crates/scout-browser/src/server.rs` | `POST /render`, `GET /healthz` |
| `crates/scout-browser/src/main.rs` | the bind rule, SIGTERM, the render wired in, `probe` |
| `crates/scout-browser/src/probe.rs` (new) | the probes |
| `crates/scout-browser/src/lib.rs` | the crate doc says what is deployed now |
| `Dockerfile` | builds and copies `scout-browser` too |
| `deploy/k8s/browser.yaml` (new) | the pod, its Service, its NetworkPolicy |
| `deploy/k8s/deployment.yaml` | `SCOUT_BROWSER_URL` |
| `scripts/deploy-k3s.sh` | reads the node's address, applies the browser first, proves the fence, then Scout |
| `scripts/check-browser-fence.sh` (new) | the fence, proved |
| `crates/scout-browser/tests/manifests.rs` (new) | the manifests say what the fence needs |
| `docs/BOARD.md`, `README.md`, the spec | what was built and measured |

## Gate (before every commit that touches code)

From the worktree, with this tree's own cargo config:

```bash
SCOUT_REQUIRE_CHROME=1 sh <scratchpad>/team/gate.sh "$PWD" <scratchpad>/team/cargo-browser-pod.toml <scratchpad>/team/pod-gateN
```

Never run `cargo fmt`. Commit trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Branch `feat/browser-pod`.

---

### Task 1: A render refuses what is not a public web address

**Files:** `crates/scout-core/src/tools/browser.rs`

- [ ] Tests first, in `tools::browser::tests`, with a renderer whose Chrome path does not exist, so that a test passing proves no Chrome was asked:
  - `file:///etc/passwd`, `chrome://version`, `data:text/html,x`, `javascript:1`, `about:blank` and `not a url` are each `BrowserError::Refused`, with a reason naming the scheme or saying it is not an address.
  - `http://127.0.0.1:9090/metrics`, `http://169.254.169.254/` and `http://[::1]/` are each `Refused` with `net_guard::NOT_PUBLIC`.
  - A live test (`live` module, real Chrome, `#[ignore]`d like the others): a renderer `reaching_only` a mock still renders that mock, so the guard used is the renderer's own.
- [ ] Implementation: `BrowserError::Refused(String)`. At the top of `Renderer::render`, after the canned answer and before the slot is taken: parse with `url::Url`, require `http` or `https`, then `self.guard.check(&url)`.
- [ ] `fetch_page` logs a render error and moves on, as before. A `Refused` there would mean `fetch_page`'s own check let something through, and nothing else changes.

### Task 2: Scout's renderer goes remote

**Files:** `crates/scout-core/src/tools/browser.rs`, `crates/scout-core/src/config.rs`, `crates/scout-core/src/core.rs`

- [ ] Tests first, against a wiremock server standing in for the pod:
  - 200 with a body: the render is that body, and the request was `POST /render` with `{"url": …}`.
  - 503 → `Busy`, 504 → `Timeout`, 422 with `{"error": "…"}` → `Refused` carrying that sentence, any other status → `Launch` naming the status.
  - A 302 to another address is not followed, and the other address is not asked: `Launch`.
  - A body over the cap → `Launch`, and only the cap is read.
  - A pod that does not answer within the deadline → `Timeout`, at `RENDER_TIMEOUT` (paused time).
  - The remote form still refuses `file:` and loopback before asking the pod (Task 1's check runs for both forms). The pod is the second check, not the only one.
  - Config: `SCOUT_BROWSER_URL` unset or blank → `None`; `http://scout-browser:8091` → `Some`; `ftp://x` and `scout-browser:8091` → an error at start.
- [ ] Implementation: `Renderer { how: How, slots, guard, profiles, … }` with `enum How { Local(PathBuf), Remote { base: Url, client: reqwest::Client } }`. `Renderer::remote(base)` builds a client with `redirect::Policy::none()`, `no_proxy()` and the render's timeout. The slots stay in both forms. In Scout they are backpressure, and the pod has slots of its own, which are the cap. The body is read in chunks up to `MAX_REMOTE_PAGE` (16 MB).
- [ ] `core.rs`: `cfg.browser_url.map(Renderer::remote).or_else(|| find_chrome().map(Renderer::new))`, and the log line says which form.

### Task 3: The narrow public surface

**Files:** `crates/scout-core/src/lib.rs`

- [ ] `pub mod render { pub use crate::tools::browser::{find_chrome, BrowserError, Renderer, RENDER_TIMEOUT}; }`, with the reason: `tools` stays private because a channel that can name `Store` can query it, and the pod needs to render and nothing else.

### Task 4: `POST /render` and `GET /healthz`

**Files:** `crates/scout-browser/src/server.rs`, `crates/scout-browser/src/main.rs`

- [ ] A trait the router renders through, `Render`, implemented for `scout_core::render::Renderer` and by a scripted stand-in in the tests. `router(sessions, renders: Option<Arc<R>>)`.
- [ ] Tests first (`tower::ServiceExt::oneshot`, as the session tests do):
  - A render comes back as `200`, `text/html; charset=utf-8`, with the page as the body.
  - `Refused` → 422 `{"error"}`, `Busy` → 503, `Timeout` → 504, `Launch` → 502.
  - No renderer → 404. A body that is not `{"url"}` → 422. `GET /render` → 405.
  - `GET /healthz` → 200 `{"ok": true}`, even when no renderer is configured.
  - The existing test that asserted `/render` is a 404 is changed to assert what remains turned away.
- [ ] `main.rs`: `scout_core::render::find_chrome().map(Renderer::new)`, the renderer wired into the router, a log line naming both halves.

### Task 5: The bind rule, SIGTERM, `probe`

**Files:** `crates/scout-browser/src/main.rs`, `crates/scout-browser/src/probe.rs`

- [ ] Bind tests: loopback by default, as now. `0.0.0.0:8091` is refused without `SCOUT_BROWSER_FENCE` and allowed with it. A blank fence is no fence.
- [ ] SIGTERM and Ctrl-C both start graceful shutdown (`tokio::signal::unix`).
- [ ] `probe`. Each subcommand prints `OK …` or `FAIL …` and exits 0 or 1, within a 5-second limit:
  - `probe tcp <host:port>`: name resolved, TCP connection opened.
  - `probe dns <name>`: resolved through the pod's resolver.
  - `probe udp <ip:port>`: a DNS question for `example.com` sent there, OK when an answer comes back.
  - `probe render <url>`: `POST $SCOUT_BROWSER_URL/render`, OK with the byte count and the page's `<title>`.
  - Tests: tcp against a listener the test opens and against a port it has just closed; udp against a UDP socket the test answers from and one that never answers; the argument errors.

### Task 6: The image carries the browser

**Files:** `Dockerfile`

- [ ] `cargo build --release -p scout-telegram -p scout-browser`, both copied out, both in `/usr/local/bin`. The comment that says `scout-browser` is left out on purpose now says why it is in.
- [ ] Built on the node by the deploy.

### Task 7: The manifests

**Files:** `deploy/k8s/browser.yaml` (new), `deploy/k8s/deployment.yaml`, `crates/scout-browser/tests/manifests.rs` (new)

- [ ] `browser.yaml` holds three documents:
  - **The Deployment:** `replicas: 1`, a rolling update with `maxUnavailable: 0`, `terminationGracePeriodSeconds: 60`, `automountServiceAccountToken: false`, `enableServiceLinks: false`, uid and gid 1000, `runAsNonRoot`, seccomp `RuntimeDefault`. Command `scout-browser`; the entrypoint stays tini. Env: `SCOUT_BROWSER_BIND=0.0.0.0:8091` and `SCOUT_BROWSER_FENCE=the scout-browser NetworkPolicy`. Probes on `/healthz`. Requests 100m CPU and 256Mi memory, limit 1536Mi. `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`, all capabilities dropped. Memory-backed `emptyDir`s for `/tmp` (512Mi) and `/home/scout` (256Mi). The image is `scout:${SCOUT_SHA}`.
  - **The Service:** ClusterIP on 8091.
  - **The NetworkPolicy:** as decided above, with `${SCOUT_NODE_IP}/32` among the exceptions.
- [ ] `deployment.yaml`: `SCOUT_BROWSER_URL=http://scout-browser:8091`.
- [ ] `tests/manifests.rs` reads the files as text and asserts what the fence depends on:
  - no `envFrom`, `secretRef` or `secretKeyRef` in `browser.yaml`;
  - the token off, the root read-only, the capabilities dropped;
  - ingress from `app: scout` only;
  - no egress rule without a `to:`;
  - the public rule is TCP;
  - the node's address excepted;
  - Scout's deployment names the pod.

### Task 8: The deploy and the fence

**Files:** `scripts/deploy-k3s.sh`, `scripts/check-browser-fence.sh` (new)

- [ ] `deploy-k3s.sh`, after the import and the secrets:
  - Read the node's InternalIP from the cluster. Refuse when it is empty or not an IPv4 address.
  - Apply `browser.yaml` through `envsubst` with an explicit list (`'${SCOUT_NODE_IP} ${SCOUT_SHA}'`), so that nothing else in it is substituted.
  - Wait for the browser's rollout and run the fence script. Then apply Scout's manifests and set its image, as now.
  - A fence that fails stops the deploy before Scout is pointed at the pod.
- [ ] `check-browser-fence.sh` runs on this machine, over the same SSH, and runs `kubectl exec` on the node. Each check runs in the browser pod and, as its control, in Scout's pod:

  | check | browser pod | Scout's pod (control) |
  |---|---|---|
  | `tcp scout:80` (Scout's Service) | must fail | must succeed |
  | `tcp <Scout pod>:9090` (metrics) | must fail | must succeed |
  | `tcp <node>:6443` (k3s API) | must fail | must succeed |
  | `tcp <node>:22` (SSH) | must fail | must succeed |
  | `tcp 169.254.169.254:80` (metadata) | must fail | must succeed, or the check is reported as unproved |
  | `udp 1.1.1.1:53` (a datagram out) | must fail | must succeed |
  | `dns example.com` | must succeed | |
  | `tcp example.com:443` | must succeed | |
  | `render https://example.com` from Scout's pod | | must succeed, title "Example Domain" |

  Ingress: a pod with no `app: scout` label is started from the same image (`kubectl run`, removed afterwards) and must fail `tcp scout-browser:8091`, while Scout's pod succeeds. The script prints each line, and it names no address on the way out (`<node>`, not the address).
- [ ] The script has a dry run, like the deploy's.

### Task 9: Deploy and measure

- [ ] Deploy from the main checkout after the merge. The fence script runs inside the deploy.
- [ ] The pod holds nothing: no variable named like a key, token, secret or password in its environment, no `/var/run/secrets/kubernetes.io`, and the root read-only (a write to `/usr` fails).
- [ ] A render through the pod is what it was: Scout's log shows `fetch_page` retrying with the browser and the pod answering. A page that answers a plain request with 403 and renders in a browser renders through the pod. (That page is in the earlier render measurement; its host is not written here.)
- [ ] One measurement, not a condition: does Chromium's own sandbox start in this pod?
- [ ] Memory: the browser pod's VmRSS idle and during a render.

### Task 10: The board, the README, the spec

- [ ] The spec's Part 2 gets what was decided here: DNS by selector, TCP only out, the node's address read and excepted, the check in the render, the untrusted answer, the bind rule.
- [ ] README: one paragraph on the browser pod, next to the render's description.
- [ ] `docs/BOARD.md` and the board artifact: the card done, with what the fence script proved. On "Before anything calls scout-browser", the items this card did (UDP in the policy, tini as init, SIGTERM) are marked done, and the rest are left.

## What this card does not do

- No NetworkPolicy for Scout's own pod. That is the separate card.
- No shop desk. The sessions are reachable from Scout and called by nothing.
- Chromium stays in Scout's image and pod: the trip PDF prints Scout's own page with it, and a local run renders with it.
- The sessions' own tests are not run against the image's Chromium, and a navigation between two hosts of one shop is not handled. Both stay on "Before anything calls scout-browser".
