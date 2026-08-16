# Scout — Invite Links: Implementation Plan

Date: 2026-08-16
Design: `docs/superpowers/specs/2026-08-08-invite-links-design.md` (Approved)

Eight tasks, each with its tests. Order matters: the store and the pure
functions come first because everything above them is wiring.

Verified before starting:

- The container runs UTC (`docker compose exec scout date +%Z` → `UTC`), so
  DuckDB's `current_date` is the midnight-UTC boundary the design promises to
  users.
- `dashmap 6` exports `DashSet`, so the membership set needs no new dependency.
- `teloxide-core 0.13` has `ApiError::{BotBlocked, UserDeactivated,
  ChatNotFound}`, so "they have opted out" can be told apart from "the send
  failed" when pruning the waitlist.

---

## Task 1 — `INVITE_DAILY_REQUESTS`

`src/config.rs`. `pub invite_daily_requests: i64`, default 20.

Tests: defaults to 20; parses an override; rejects a non-number; rejects zero
and negatives (a cap of zero would silently mute every invited member, which
is a typo rather than a policy).

## Task 2 — Store

`src/store.rs`. The three tables from the design, appended to `MIGRATIONS`.

```rust
pub enum Claim { Admitted, AlreadyIn, Revoked, NoRoom }
```

`NoRoom` covers the three cases that give one reply: unknown code, closed
round, full round. Collapsing them in the *type* rather than at the call site
means no caller can accidentally tell a stranger which one it was.

Methods:

- `active_members() -> Vec<i64>` — startup load, `revoked_at IS NULL`
- `claim_seat(user_id, chat_id, code) -> Claim` — one lock, check-and-insert
- `create_round(code, capacity) -> Result<bool>` — false if the name is taken
- `set_round_open(code, open) -> Result<bool>` — false if no such round
- `rounds() -> Vec<RoundStatus>` — code, seats used, capacity, open
- `waiting_count() -> i64`
- `revoke(user_id) -> Result<bool>` / `restore(user_id) -> Result<bool>`
- `waitlist_to_invite() -> Vec<(i64, i64)>` — `invited_at IS NULL`, oldest first
- `mark_invited(user_id)` / `forget_waitlist(user_id)`
- `requests_today(user_id) -> i64` — text + photo since `current_date`

Tests, from the design's list: a round of N admits exactly N and refuses the
N+1th; concurrent claims never oversell (threads against one `Store`); a
repeat claim is a no-op and spends no seat; a revoked user is refused through
their old code and through a new one; revoking does not return the seat and
unkicking does not consume one; an unknown code and a closed round are both
refused; a reopened round admits again; a turned-away user lands on the
waitlist and a successful claim clears it; `invited_at` is stamped only on
rows an announce reached; `requests_today` counts text and photo but not
reactions or flight searches, and only today's.

## Task 3 — Pure functions

`src/bot.rs`. `is_start`, `join_code`, `check_round_name`, `parse_invite`.

Tests: the design's list — `/start x`, `/start@bot x`, bare `/start`,
`/start@bot`, surrounding whitespace, and a message merely beginning with the
word "start"; the name alphabet, the 64-character limit, and empty; capacity
parsing including the default and a rejected non-number.

## Task 4 — The gate

`App.members: DashSet<i64>`, `is_member`, the `/start` branch ahead of it,
and `handle_start` with the design's five cases. `Command::Start` leaves the
enum. `handle_reaction`'s own allowlist check moves to `is_member` too —
otherwise a member's 👍 is silently dropped, which is the same gate bug in a
second place.

`note_sender` is called only for people who are *in* (admitted or already a
member). A turned-away stranger must not land in `user_chats`, because that
table is `/advert`'s address book and an announcement is for users, not for
everyone who ever pressed START.

## Task 5 — Admin commands

`/invite new|status|close|open|announce`, `/kick`, `/unkick`. Gated on
`admin_user_ids`, the same check `/advert` uses. Appended to `ADMIN_HELP`.

`get_me` failure when minting a round: keep the round, say the link could not
be built, print the code.

## Task 6 — `broadcast`

One helper for `/advert` and `/invite announce`: sequential sends, a delay
between them to stay under Telegram's ~30/second bulk limit, `RetryAfter`
honoured rather than counted as a failure, and per-recipient outcomes so the
announce can stamp `invited_at` on the ones that landed and drop the ones
that can never land.

## Task 7 — Daily cap

Checked in `handle_text` and `handle_photo`, before `log_request`, skipped
for `allowed_user_ids`.

## Task 8 — Wiring and docs

`main.rs` loads the membership set; `.env.example`, `README.md`.

---

## Corrections made during implementation

1. **The announce sent a link that could not work.** The design says the
   announce "sends the new round's link", and three paragraphs earlier
   explains why that cannot reach anybody on the waitlist: only START
   delivers a payload, START only appears in an empty chat, and being turned
   away gives you chat history. Every recipient would have tapped a link
   into a chat that carried nothing. The announce now sends `/start <code>`
   in a `<code>` span (tap-to-copy in Telegram), and `/invite new`'s reply
   carries the link *and* the command, saying which is for what. The design
   doc is amended.

2. **The announce refuses a closed or full round.** Not in the design. A
   round nobody can join through spends the queue's one notification on a
   dead end and stamps `invited_at` as though they had been told something
   real — and `invited_at` is exactly what stops a re-run from reaching
   them.

3. **`handle_reaction` had its own copy of the gate.** The design's routing
   section covers the message branches; reactions arrive on a different
   update type and carried an `allowed_user_ids` check of their own. Left
   alone, every invited member's 👍 would have been dropped while their
   messages worked — the same gate bug in a second place.

4. **`note_sender` is only called for people who are in.** Not stated in the
   design, and getting it wrong would have been quiet: `user_chats` is
   `/advert`'s address book, so recording a turned-away stranger there would
   put everyone who ever pressed START on the receiving end of the next
   announcement.

5. **`/invite` refuses a trailing word.** `new autumn 100 seats` parsed as a
   round of 100 with the extra word dropped. Silently ignoring input is how
   an admin opens a round they did not mean to.

6. **`INVITE_DAILY_REQUESTS` refuses zero.** A cap of zero mutes every
   invited member without saying so anywhere. Closing a round is how you
   stop admitting people; revoking is how you remove one.

## Verified by mutation

Each of these was broken deliberately and the named test went red, then the
break was reverted:

| Break | Test that caught it |
|---|---|
| `used <= capacity` in the seat check | `a_round_admits_its_capacity_and_not_one_more` |
| revoked users fall through to the round check | `a_revoked_member_cannot_rejoin_through_any_link` |
| daily cap counts every `request_log` kind | `the_daily_cap_counts_messages_and_only_todays` |
| a successful claim leaves the waitlist row | `being_turned_away_queues_you_and_getting_in_clears_it` |
| `/started` parsed as `/start` | `start_is_recognised_across_every_form_telegram_sends` |
