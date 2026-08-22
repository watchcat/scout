# W1 — The Front Door Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Scout reachable from the internet for the first time — a public page that says accurately what Scout does and whether the current invite round has room, served over TLS.

**Architecture:** A new `scout-web` crate holds axum routes and the page, embedded in the binary. It runs as a task inside the existing process, because DuckDB is single-writer and a second process cannot open the file. Caddy is a new compose service, terminates TLS, and is the only thing publishing ports. Round state is cached in memory and refreshed on a timer, so no request ever touches the database.

**Tech Stack:** Rust 2021, axum, tower (test only), Caddy. No JavaScript on the page.

---

## Why the database stays off the request path

The store is behind one mutex, shared with the agent. A public endpoint that
takes that mutex on every request is a lever a stranger can pull to make Scout
slow for everyone — no exploit needed, only traffic. The cache is not an
optimisation, it is the security design.

## Verified before writing this plan

Measured against `9eee045`, not assumed:

| Claim | How it was checked | Result |
|---|---|---|
| Nothing is currently exposed | `grep -iE "ports\|expose" compose.yaml` | no matches — outbound only |
| `scout-web` cannot reach the store | `crates/scout-core/src/lib.rs` | `mod store;` is private; `Core::store()` is `pub(crate)` |
| Several rounds can be open at once | `store.rs:1187` `rounds()` | returns `Vec<RoundStatus>`, oldest first, no "current" concept |
| `RoundStatus` fields | `store.rs:307` | `code`, `capacity`, `used`, `open` |
| Core already knows the bot's web address | `AgentDeps.return_url` | `https://t.me/{username}`, from `getMe` |
| tower and hyper already in the tree | `Cargo.lock` | tower 0.5.3, hyper 1.11.0 — axum must not add a second hyper major |
| Baseline to preserve | `TZ=UTC cargo test --workspace` | 446 passed, 3 ignored |

**Two decisions the spec left open, settled here:**

- **"The current round" is the most recently created round that is open and has
  room.** `rounds()` returns every round ever created, oldest first, and several
  can be open simultaneously. Taking the newest with room means opening a new
  round supersedes an older one on the page without anyone closing it.
- **Core builds the join link, not `scout-web`.** The link is
  `{return_url}?start={code}`, and `return_url` is the bot's own address which
  core already holds. Building it in the web crate would put Telegram knowledge
  on the wrong side of a boundary the last phase spent eight tasks drawing.

## File Structure

```
scout/
  Caddyfile                       new — TLS and the reverse proxy
  compose.yaml                    gains a caddy service and an internal network
  crates/
    scout-core/src/core.rs        gains Admission and Core::admission()
    scout-web/
      Cargo.toml
      src/lib.rs                  serve(), router, state
      src/cache.rs                the refresh task and the read
      src/page.rs                 the template and its substitution
      src/index.html              the page, embedded with include_str!
    scout-telegram/src/main.rs    spawns the server
```

---

### Task 1: Core decides who is being let in

**Files:**
- Modify: `crates/scout-core/src/core.rs`

The web crate cannot see `RoundStatus` — it comes from a private module — and
should not be picking a round out of a list anyway. That is core's judgement.

- [ ] **Step 1: Write the failing test**

In `crates/scout-core/src/core.rs`, `mod tests`:

```rust
    #[tokio::test]
    async fn the_newest_round_with_room_is_the_one_a_stranger_can_join() {
        // Several rounds can be open at once — nothing closes the old one
        // when a new one opens. The page has room for one answer, so core
        // picks: the newest round that is open and not yet full.
        let dir = tempfile::tempdir().unwrap();
        let core = Core::start(
            Config::for_test(dir.path().join("doors.duckdb").to_str().unwrap()),
            Some("https://t.me/scoutbot".to_string()),
        )
        .unwrap();
        let store = core.store();

        assert_eq!(core.admission().await.unwrap(), Admission::Full,
            "no rounds at all is not an invitation");

        store.create_round("spring", 1).unwrap();
        store.create_round("autumn", 1).unwrap();
        assert_eq!(
            core.admission().await.unwrap(),
            Admission::Open { join_url: Some("https://t.me/scoutbot?start=autumn".to_string()) },
            "the newer round supersedes the older one without anyone closing it"
        );

        // Fill autumn. Spring is still open with room, so the door is not shut.
        let joiner = store.account_for_telegram(7).unwrap();
        assert_eq!(store.claim_seat(joiner, 7, "autumn").unwrap(), Claim::Admitted);
        assert_eq!(
            core.admission().await.unwrap(),
            Admission::Open { join_url: Some("https://t.me/scoutbot?start=spring".to_string()) }
        );

        store.set_round_open("spring", false).unwrap();
        assert_eq!(core.admission().await.unwrap(), Admission::Full,
            "a closed round and a full round look the same from outside");
    }
```

- [ ] **Step 2: Run it to watch it fail**

Run: `TZ=UTC cargo test -p scout-core the_newest_round_with_room`
Expected: FAIL — `cannot find type 'Admission' in this scope`.

- [ ] **Step 3: Implement it**

```rust
/// Whether someone arriving right now can get in.
///
/// Deliberately not a count. How many seats are left is nobody's business
/// outside this process; whether it is worth turning up is everybody's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// A round is open and has room. `join_url` is `None` only when the bot
    /// could not read its own address at start-up, which disables the link
    /// rather than sending anyone somewhere that is not ours.
    Open { join_url: Option<String> },
    /// Full, closed, or no round at all. One answer on purpose: which it is
    /// tells a stranger about rounds they were not invited to.
    Full,
}

impl Core {
    /// Who may walk in, as the public page reports it.
    ///
    /// Reads the database. Callers on a request path must cache it — see
    /// `scout-web`, where a public endpoint that took the store's mutex
    /// would be a way for a stranger to slow the agent down.
    pub async fn admission(&self) -> anyhow::Result<Admission> {
        let store = self.store();
        let return_url = self.deps.return_url.clone();
        blocking(move || {
            let newest_with_room = store
                .rounds()?
                .into_iter()
                .filter(|r| r.open && r.used < r.capacity)
                .next_back();
            Ok(match newest_with_room {
                Some(r) => Admission::Open {
                    join_url: return_url.map(|u| format!("{u}?start={}", r.code)),
                },
                None => Admission::Full,
            })
        })
        .await
    }
}
```

`rounds()` returns oldest first, so `next_back()` is the newest. Add
`use crate::store::Claim;` to the test module if it is not already imported.

- [ ] **Step 4: Run it to watch it pass**

Run: `TZ=UTC cargo test -p scout-core the_newest_round_with_room`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: core says whether a stranger can walk in"
```

---

### Task 2: The crate, and something that answers

**Files:**
- Create: `crates/scout-web/Cargo.toml`, `crates/scout-web/src/lib.rs`
- Modify: root `Cargo.toml` (workspace members)

- [ ] **Step 1: Add the crate and its dependencies**

```bash
mkdir -p crates/scout-web/src
```

Add `"crates/scout-web"` to `members` in the root `Cargo.toml`.

`crates/scout-web/Cargo.toml`:

```toml
[package]
name = "scout-web"
version = "0.1.0"
edition = "2021"
license = "MIT"
description = "Scout's public face"

[dependencies]
scout-core = { path = "../scout-core" }
tokio = { version = "1", features = ["macros", "net", "rt-multi-thread", "time"] }
anyhow = "1"
tracing = "0.1"

[dev-dependencies]
tower = { version = "0.5", features = ["util"] }
tempfile = "3"
```

Then add axum, letting cargo pick the version rather than guessing one. Write
Step 2's `src/lib.rs` first — `cargo add` refuses a manifest with no targets:

```bash
cargo add axum -p scout-web
```

**Then check it did not fork the dependency graph:**

```bash
grep -c 'name = "hyper"' Cargo.lock
```

Expected: `2` — hyper 0.14 and 1.x already coexist in this tree. A `3` means
axum pulled a third and the version needs pinning to one built on hyper 1.

- [ ] **Step 2: Write the failing test**

`crates/scout-web/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn the_proxy_can_tell_whether_we_are_alive() {
        // Driven through the router rather than a socket: no port to bind,
        // nothing to conflict with, and it runs in the same suite as
        // everything else.
        let app = health_router();
        let res = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
```

- [ ] **Step 3: Run it to watch it fail**

Run: `TZ=UTC cargo test -p scout-web`
Expected: FAIL — `cannot find function 'health_router'`.

- [ ] **Step 4: Implement the smallest thing that answers**

Above the test module in `crates/scout-web/src/lib.rs`:

```rust
//! Scout's public face: one page, and a liveness check.
//!
//! Runs inside the Telegram binary today because DuckDB is single-writer and
//! a second process cannot open the file. In W4 the same crate is spawned by
//! the core binary instead; nothing here changes when it moves.

use axum::routing::get;
use axum::Router;

/// Liveness only. Deliberately says nothing about the database: a health
/// check that fails when DuckDB is busy would take the site down for a
/// reason the site does not have.
fn health_router() -> Router {
    Router::new().route("/healthz", get(|| async { "ok" }))
}
```

- [ ] **Step 5: Run it to watch it pass**

Run: `TZ=UTC cargo test -p scout-web`
Expected: PASS — 1 test.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: a crate for the part of scout that faces outward"
```

---

### Task 3: The cache that keeps the database off the request path

**Files:**
- Create: `crates/scout-web/src/cache.rs`
- Modify: `crates/scout-web/src/lib.rs` (add `mod cache;`)

- [ ] **Step 1: Write the failing test**

`crates/scout-web/src/cache.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use scout_core::core::Admission;

    #[test]
    fn a_read_never_waits_and_a_refresh_is_what_changes_it() {
        // The whole point: a request reads a value. Nothing on the request
        // path can block on the store's mutex, because that mutex is shared
        // with the agent and this endpoint is open to anyone.
        let cache = AdmissionCache::new(Admission::Full);
        assert_eq!(cache.get(), Admission::Full);

        cache.put(Admission::Open { join_url: Some("https://t.me/x?start=autumn".into()) });
        assert_eq!(
            cache.get(),
            Admission::Open { join_url: Some("https://t.me/x?start=autumn".into()) }
        );
    }
}
```

- [ ] **Step 2: Run it to watch it fail**

Run: `TZ=UTC cargo test -p scout-web a_read_never_waits`
Expected: FAIL — `cannot find type 'AdmissionCache'`.

- [ ] **Step 3: Implement the cache and its refresher**

```rust
//! What the page says about admission, held in memory.
//!
//! The store is behind one mutex, shared with the agent. A public endpoint
//! that took it on every request would let a stranger slow Scout down with
//! nothing but traffic. So the value is read on a timer and requests read
//! the value.

use scout_core::core::{Admission, Core};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How stale the page may be. Opening a round takes this long to show up,
/// which nobody notices, and it makes every request free.
pub const REFRESH: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct AdmissionCache(Arc<RwLock<Admission>>);

impl AdmissionCache {
    pub fn new(initial: Admission) -> Self {
        Self(Arc::new(RwLock::new(initial)))
    }

    /// Never blocks on anything slower than another reader.
    pub fn get(&self) -> Admission {
        self.0.read().unwrap().clone()
    }

    pub fn put(&self, next: Admission) {
        *self.0.write().unwrap() = next;
    }
}

/// Refreshes forever. A failed read leaves the last known value in place:
/// the page saying "open" for another thirty seconds is a better failure
/// than the page not loading.
pub async fn refresh_forever(core: Arc<Core>, cache: AdmissionCache) {
    let mut ticker = tokio::time::interval(REFRESH);
    loop {
        ticker.tick().await;
        match core.admission().await {
            Ok(next) => cache.put(next),
            Err(e) => tracing::warn!(error = %e, "could not refresh admission; keeping the last value"),
        }
    }
}
```

- [ ] **Step 4: Run it to watch it pass**

Run: `TZ=UTC cargo test -p scout-web`
Expected: PASS — 2 tests.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "feat: the page reads memory, never the database"
```

---

### Task 4: The page

**Files:**
- Create: `crates/scout-web/src/index.html`, `crates/scout-web/src/page.rs`
- Modify: `crates/scout-web/src/lib.rs`

- [ ] **Step 1: Bring the approved mockup in**

The design was approved as a mockup, committed at a stable path so this step
does not depend on a scratch directory surviving:

```bash
cp docs/design/w1-landing-mockup.html crates/scout-web/src/index.html
```

Then make it the real page, not a mockup — three edits, all deletions plus one
insertion:

1. **Delete the yellow `<div class="note">` banner** at the top and its `.note`
   CSS rules. It explains the mockup to a reviewer.
2. **Delete the entire `<div class="variants">` block** — the "status strip in
   both states" demo — and its `.variants` CSS. The real page shows one state.
3. **Replace the `<div class="gate">…</div>` in the header** with the single
   token `<!--STATUS-->` on its own line.

Fill in the two placeholder links while you are here: the footer's "Source on
GitHub" points at `https://github.com/watchcat/scout`, and "Honest limitations"
points at `https://github.com/watchcat/scout#honest-limitations`.

- [ ] **Step 2: Write the failing test**

`crates/scout-web/src/page.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_round_offers_a_way_in_and_a_full_one_offers_none() {
        let open = render(&Admission::Open {
            join_url: Some("https://t.me/scoutbot?start=autumn".to_string()),
        });
        assert!(open.contains("https://t.me/scoutbot?start=autumn"));
        assert!(open.contains("Invites open"));
        assert!(!open.contains("<!--STATUS-->"), "the token survived into the page");

        let full = render(&Admission::Full);
        assert!(full.contains("Currently full"));
        assert!(!full.contains("t.me"), "a full round must not hand out a join link");
    }

    #[test]
    fn a_round_with_no_known_join_link_still_says_it_is_open() {
        // return_url is None when getMe failed at start-up. Saying "open"
        // with no button is honest; inventing a link is not.
        let page = render(&Admission::Open { join_url: None });
        assert!(page.contains("Invites open"));
        assert!(!page.contains("t.me"));
    }

    #[test]
    fn the_page_never_says_how_many_seats_are_left() {
        // The design says state, not numbers. This is the test that keeps
        // someone from "helpfully" adding a count later.
        for a in [Admission::Full, Admission::Open { join_url: None }] {
            let page = render(&a);
            assert!(!page.contains("seats"), "{page:?} leaked capacity");
            assert!(!page.contains("remaining"));
        }
    }
}
```

- [ ] **Step 3: Run it to watch it fail**

Run: `TZ=UTC cargo test -p scout-web an_open_round_offers`
Expected: FAIL — `cannot find function 'render'`.

- [ ] **Step 4: Implement the substitution**

```rust
//! The page, and the one thing about it that changes.

use scout_core::core::Admission;

const TEMPLATE: &str = include_str!("index.html");
const TOKEN: &str = "<!--STATUS-->";

/// The page with its status strip filled in.
///
/// Substitution rather than a template engine: there is exactly one variable
/// on this page, and a dependency to interpolate one string would be a
/// dependency to keep patched forever.
pub fn render(admission: &Admission) -> String {
    TEMPLATE.replace(TOKEN, &status_strip(admission))
}

fn status_strip(admission: &Admission) -> String {
    match admission {
        Admission::Open { join_url: Some(url) } => format!(
            r#"<div class="gate">
    <span class="pill open"><span class="dot"></span>Invites open</span>
    <p>There is room in the current round.</p>
    <a class="btn" href="{url}">Start on Telegram</a>
  </div>"#
        ),
        Admission::Open { join_url: None } => r#"<div class="gate">
    <span class="pill open"><span class="dot"></span>Invites open</span>
    <p>There is room in the current round. Message the bot on Telegram to start.</p>
  </div>"#
            .to_string(),
        Admission::Full => r#"<div class="gate">
    <span class="pill full"><span class="dot"></span>Currently full</span>
    <p>This round is closed. Come back soon — new rounds open regularly.</p>
    <a class="btn ghost" href="https://github.com/watchcat/scout">Read the source</a>
  </div>"#
            .to_string(),
    }
}
```

The join URL is built by core from a round code that only ever matches
`^[a-z0-9-]{1,64}$` (`check_round_name` in `invites.rs` enforces it), so there
is nothing here that needs escaping. If that ever stops being true, this is the
line that has to change.

- [ ] **Step 5: Run it to watch it pass**

Run: `TZ=UTC cargo test -p scout-web`
Expected: PASS — 5 tests.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: the page says what scout is and whether there is room"
```

---

### Task 5: Serving it

**Files:**
- Modify: `crates/scout-web/src/lib.rs`
- Modify: `crates/scout-telegram/src/main.rs`

- [ ] **Step 1: Write the failing test**

In `crates/scout-web/src/lib.rs`'s test module:

```rust
    #[tokio::test]
    async fn the_root_serves_the_page_and_an_unknown_path_does_not() {
        let cache = crate::cache::AdmissionCache::new(scout_core::core::Admission::Full);
        let app = router(cache);

        let res = app.clone()
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["content-type"], "text/html; charset=utf-8");

        // Deleting Task 2's test would otherwise leave /healthz uncovered,
        // and it is the route the proxy depends on.
        let health = app.clone()
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let missing = app
            .oneshot(Request::builder().uri("/wp-login.php").body(Body::empty()).unwrap())
            .await.unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }
```

- [ ] **Step 2: Run it to watch it fail**

Run: `TZ=UTC cargo test -p scout-web the_root_serves`
Expected: FAIL — `cannot find function 'router'`.

- [ ] **Step 3: Implement the router and `serve`**

Replace `health_router` with:

```rust
// Narrow this back from the `pub mod` Task 3 needed. That was to stop
// dead-code warnings while nothing called into it — `pub(crate)` does not
// silence those, since dead-code analysis is about reachability. Now that
// `serve` calls it, `mod` is enough, and leaving it public would give the
// same items two paths in from outside.
mod cache;
mod page;

pub use cache::{refresh_forever, AdmissionCache, REFRESH};

use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use scout_core::core::Core;
use std::sync::Arc;

fn router(cache: AdmissionCache) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(cache)
}

async fn index(State(cache): State<AdmissionCache>) -> Html<String> {
    Html(page::render(&cache.get()))
}

/// Serves until the process ends.
///
/// Takes the first reading before binding, so the page is never briefly
/// wrong on start-up, and so a database that will not open is a start-up
/// failure rather than a page that lies for thirty seconds.
pub async fn serve(core: Arc<Core>, bind: &str) -> anyhow::Result<()> {
    let cache = AdmissionCache::new(core.admission().await?);
    tokio::spawn(refresh_forever(core, cache.clone()));

    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(bind, "the front door is open");
    axum::serve(listener, router(cache)).await?;
    Ok(())
}
```

Delete Task 2's `the_proxy_can_tell_whether_we_are_alive` and its
`#[cfg(test)] fn health_router` — Step 1's test now covers `/healthz` through
the real router, and two tests for one route is one too many. Check the
`#[cfg(test)]` attributes on the imports come off with it.

While here: drop `tempfile` from `crates/scout-web/Cargo.toml`'s
dev-dependencies if nothing in the crate uses it by now. It was added in
anticipation of a test that reaches for a real `Core`, and no task in this
plan turned out to need one.

- [ ] **Step 4: Spawn it from main**

In `crates/scout-telegram/src/main.rs`, after the `Core` is built and before
`bot::run`:

```rust
    // The web front door. Same process as the bot because DuckDB is
    // single-writer; W4 is where it moves out. A failure here must not stop
    // the bot: the page going dark is worse than nothing, but a bot that
    // will not start because a port is taken is worse than that.
    let web_core = core.clone();
    let bind = std::env::var("SCOUT_WEB_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    tokio::spawn(async move {
        if let Err(e) = scout_web::serve(web_core, &bind).await {
            tracing::error!(error = %e, "the front door did not open");
        }
    });
```

Add `scout-web = { path = "../scout-web" }` to `crates/scout-telegram/Cargo.toml`.

- [ ] **Step 5: Run everything**

Run: `TZ=UTC cargo test --workspace`
Expected: 452 passed, 3 ignored.

Then see it for real:

```bash
cargo run -p scout-telegram
# in another shell:
curl -si localhost:8080/healthz | head -1
curl -s localhost:8080/ | grep -E "Invites open|Currently full"
```

Expected: `HTTP/1.1 200 OK`, and one of the two status lines. Stop it again.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: scout answers on a port for the first time"
```

---

### Task 6: TLS, and the only door that is open

**Files:**
- Create: `Caddyfile`
- Modify: `compose.yaml`, `scripts/deploy.sh`, `.dockerignore` (check only)

- [ ] **Step 1: Write the Caddyfile**

```
{$SCOUT_DOMAIN} {
	encode gzip
	reverse_proxy scout:8080

	header {
		# The page loads nothing from anywhere else and runs no script, so
		# the policy can be as tight as a policy gets.
		Content-Security-Policy "default-src 'none'; style-src 'unsafe-inline'; img-src 'self'"
		Strict-Transport-Security "max-age=31536000; includeSubDomains"
		X-Content-Type-Options "nosniff"
		X-Frame-Options "DENY"
		Referrer-Policy "no-referrer"
		-Server
	}
}
```

`style-src 'unsafe-inline'` is required because the page's CSS is in a `<style>`
block, which is what keeps it a single embedded file. There is no script
anywhere on the page, so the usual reason to fear inline sources does not apply.

- [ ] **Step 2: Add the proxy to compose**

```yaml
services:
  scout:
    # ... everything as it is now, plus:
    networks: [internal]
    # No `ports:` — after this, the only way in is through caddy.

  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports:
      - "80:80"
      - "443:443"
    environment:
      SCOUT_DOMAIN: ${SCOUT_DOMAIN}
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy-data:/data
      - caddy-config:/config
    networks: [internal]
    depends_on: [scout]

networks:
  internal:

volumes:
  scout-data:
  caddy-data:
  caddy-config:
```

`caddy-data` holds the issued certificates. Losing it means re-issuing on the
next start, which Let's Encrypt rate-limits — so it is a named volume, not a
bind mount, and not something to prune casually.

Add `SCOUT_DOMAIN=scout.example.com` to **`.env.example`** only.

`.env` holds real credentials, is gitignored, and is not yours to edit — the
human running the deploy sets `SCOUT_DOMAIN` there. Task 7 Step 1 fails loudly
if they have not, which is the right place to find out.

- [ ] **Step 3: Update the deploy script**

`scripts/deploy.sh`'s dirty-path list gains `Caddyfile`:

```bash
git status --porcelain -- crates Cargo.toml Cargo.lock compose.yaml Dockerfile Caddyfile
```

Both the check on line 41 and the display on line 43.

- [ ] **Step 4: Verify locally that the app is not exposed**

```bash
docker compose config | grep -A3 "scout:" | grep -i ports || echo "scout publishes nothing — correct"
```

Expected: the "publishes nothing" line.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "build: a proxy in front, and nothing else facing outward"
```

---

### Task 7: Deploy, and check it from outside

- [ ] **Step 1: Confirm DNS before anything else**

```bash
dig +short $SCOUT_DOMAIN
```

Expected: the host's public IP. **If this is wrong or empty, stop.** Caddy will
attempt certificate issuance, fail, and retry into Let's Encrypt's rate limit,
which is measured in hours.

- [ ] **Step 2: Back up the database from inside the container**

```bash
docker compose exec -T scout cp /data/scout.duckdb /data/scout.duckdb.pre-w1
```

Never from the host: the host's DuckDB is older than the one that wrote the
file. This phase runs no migration, so the backup is precaution — take it anyway.

- [ ] **Step 3: Deploy**

Run: `scripts/deploy.sh`
Expected: `scout is up`, plus the new `the front door is open` line.

- [ ] **Step 4: Watch the certificate issue**

```bash
docker compose logs caddy --tail 40 | grep -iE "certificate|obtain|error"
```

Expected: a successful obtain. This is the step that can fail in a way nothing
in this project has failed before, so watch it rather than assuming.

- [ ] **Step 5: Check it from outside, not from the host**

```bash
curl -sI https://$SCOUT_DOMAIN | head -3
curl -s https://$SCOUT_DOMAIN | grep -E "Invites open|Currently full"
curl -sI http://$SCOUT_DOMAIN | grep -i location     # should redirect to https
```

Then confirm the application itself is **not** reachable directly — from
another machine, `curl http://<host-ip>:8080/` must fail to connect.

- [ ] **Step 6: Confirm the bot is untouched**

```bash
docker compose logs --since 5m | grep -E "ERROR|WARN"
docker inspect --format 'restarts={{.RestartCount}}' $(docker compose ps -q scout)
```

Then send the bot an ordinary question. The whole phase is meant to be
invisible from a chat window.

- [ ] **Step 7: Finish the branch**

REQUIRED SUB-SKILL: superpowers:finishing-a-development-branch

---

## What this phase deliberately does not do

- **No sign-in, and therefore no queueing.** Someone arriving at a full round is
  told so and offered nothing else. Known dead end, closed by W2, and the reason
  is in the design: the queue keys on `account_id`, so an email with no account
  behind it would be a second list invisible to `waitlist_to_invite`.
- **No JSON API.** Server-side substitution until W3 has a client that needs
  one.
- **No rate limiting at the proxy.** The cache keeps the database off the
  request path, which is the consideration that matters. Bandwidth limits can
  wait for evidence anyone is pointing traffic here.
- **No analytics.** Not an oversight — nothing on this page needs to know who
  read it, and adding a third-party script would break the content security
  policy that is currently strong enough to be worth having.
