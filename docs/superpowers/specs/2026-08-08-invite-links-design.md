# Scout — Invite Links: Design

Date: 2026-08-08
Status: Approved

## Purpose

Scout's access list is `ALLOWED_TELEGRAM_USER_IDS` in `.env`, read once at
startup and checked by `is_allowed` (`src/bot.rs:232`). Admitting one person
means editing a file and redeploying. That is fine for a household and
useless for distribution.

This design makes admission something the running bot can do: a Telegram
deep link that admits the first N people who press START, and then tells
everyone after them that the round is full. The admin opens rounds from
chat; nobody edits `.env` to let a friend in.

## Scope

**In scope:**
- A `members` table and a runtime membership set, so the gate can grow
- Rounds: a named code with a capacity, opened and closed by the admin
- A join path that reaches non-members, since today's gate drops them
- Revoking a member and undoing that, without either changing seat counts
- A waitlist of people turned away, and a command to reach them when the
  next round opens
- A daily request cap for invited members, so 100 strangers cannot run up
  an unbounded bill
- Pacing on the broadcast loop, which was written for a household

**Out of scope (deliberately):**
- Per-round daily caps — one number for everyone invited
- Expiring or trial access — membership is permanent until revoked
- Unique one-use codes per person — one code per round, shared freely
- Letting a revoked person back in through a later link
- Any change to what a member can *do*; this is only about who gets in

## What Telegram actually gives us

Three mechanics decide the shape of the rest.

**A click is not observable.** Telegram never reports that a link was
opened. The only countable event is `/start <code>` arriving, which happens
when someone presses START. That is the better number anyway: it counts
people who arrived, not people who scrolled past.

**The payload alphabet is narrow.** A start parameter is 1–64 characters of
`A-Za-z0-9_-`. Round names are validated against exactly that, so a name
that would produce a broken link is refused at the point the admin types it
rather than discovered when nobody can join.

**The START button only appears in an empty chat.** If someone has already
messaged the bot, opening a deep link just opens the chat — the payload is
never delivered. So a person turned away from a full round generally
*cannot* get in later by clicking a new link, because the act of being
turned away gave them chat history.

The same fact is what makes the waitlist work: anyone who reached us at all
has started a conversation with the bot, so the bot is allowed to message
them. Push is reliable where the second click is not.

## Data model

Three new tables, in the style of `user_chats` — new tables rather than
columns on `users`, which already holds live data.

```sql
CREATE TABLE IF NOT EXISTS invite_rounds (
    code       TEXT PRIMARY KEY,
    capacity   BIGINT NOT NULL,
    open       BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE IF NOT EXISTS members (
    user_id    BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    joined_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    revoked_at TIMESTAMP
);
CREATE TABLE IF NOT EXISTS waitlist (
    user_id    BIGINT PRIMARY KEY,
    chat_id    BIGINT NOT NULL,
    code       TEXT NOT NULL,
    seen_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    invited_at TIMESTAMP
);
```

Seats used by a round is `COUNT(*) FROM members WHERE code = ?`, counting
revoked rows. Revoking somebody does not hand their seat back — a round of
100 admits 100 people once, and moderation does not quietly reopen it.

`members.user_id` is the primary key, so a person belongs to one round. A
member who opens a later round's link is already in and nothing changes.

## The gate

`is_allowed` becomes a set lookup over founders and members:

```rust
fn is_member(app: &App, msg: &Message) -> bool {
    sender_id(msg).is_some_and(|id| {
        app.cfg.allowed_user_ids.contains(&id) || app.members.contains(&id)
    })
}
```

`app.members` is a `DashSet<i64>` loaded from `members` at startup
(`revoked_at IS NULL`), inserted into on a successful claim and removed on
revoke. The table is the durable record; the set is what the gate reads.

This is not premature caching. The gate runs on *every* update, including
every message from someone who was never invited. Reading DuckDB there would
take the `Arc<Mutex<Connection>>` lock on each one, so anybody who found the
bot could contend with real work by typing at it. Disk is touched only when
membership actually changes.

### Routing

The join path has to reach people the gate would reject, so `/start` becomes
a sibling branch ahead of the members branch:

```rust
Update::filter_message()
    .branch(dptree::filter(|msg: Message| is_start(&msg)).endpoint(handle_start))
    .branch(dptree::entry().filter(is_member).branch(commands).branch(photo).branch(text))
```

`is_start` and `join_code` are pure functions over the message text.
`is_start` is true for `/start` and `/start@<bot>` with or without a
payload; `join_code` returns `Some(code)` when there is one and `None` for a
bare start.

The branch owns the whole `/start` surface rather than only the
payload-carrying case. Splitting it — payload here, bare start behind the
gate — would leave a stranger's bare `/start` falling through to a gate that
drops it silently, which contradicts the reply promised below. One handler,
five cases, no message can take the wrong path.

`Command::Start` is removed from the command enum. `Command::descriptions()`
is never called anywhere in the codebase, so that variant existed purely to
route `/start` to `HELP`, and `handle_start` now does that. Removing it also
means `/start` cannot reach the text branch and be answered by the LLM,
which is what would happen if the enum kept a variant the router never fed.

## Claiming a seat

One `Store` method, one lock acquisition. The connection is behind a mutex,
so check-and-insert in a single method is atomic by construction: a round of
100 admits exactly 100 no matter how many people press START at once. There
is no separate transaction to get wrong and no counter that can drift from
the rows.

Rules, in order:

1. Already a member (not revoked) → "you're already in", show `HELP`.
2. **Revoked → refused.** Without this, revoking is theatre: the next link
   would let them straight back in.
3. Unknown code → treated as an invalid link.
4. Round closed, or seats used ≥ capacity → the round-full reply, and the
   sender is recorded on the waitlist.
5. Otherwise insert the member, add to the set, welcome them.

Cases 3 and 4 give the same reply. Distinguishing them tells a stranger
whether a code they guessed exists, which is information without a purpose.
It also means an unknown code puts its sender on the waitlist alongside
everyone who arrived through a real link. That is deliberate: they are a
person who tried to reach the bot, and sorting the typos from the genuinely
unlucky is not worth a second code path.

A successful claim deletes any waitlist row for that user, so a later
announce does not chase somebody who is already inside.

## Waitlist and announce

Case 4 records `(user_id, chat_id, code)` with `invited_at` null.

`/invite announce <name>` reaches every waitlist row with `invited_at IS
NULL`, oldest first, and stamps `invited_at` on each success. Sending in
claim order means that if the new round is smaller than the queue, the
people who waited longest hear first.

**Amended during implementation (2026-08-16): the announce sends the join
command, not the link.** This design's own reasoning rules the link out for
this audience and the first draft did not follow it through. START is the
only thing that delivers a payload, START only appears in a chat with no
history, and every person on the waitlist has history — being turned away is
itself a message. A link would have opened a chat and carried nothing, for
every single recipient. What the message carries instead is
`/start <code>` in a `<code>` span, which Telegram makes tap-to-copy;
sending it by hand delivers the same payload and always works. The link
still belongs in `/invite new`'s reply, because a public post is read by
people with empty chats, and that reply now carries both.

The announce refuses a round that is closed or already full. Announcing one
spends the whole queue's single notification on a dead end, and would stamp
`invited_at` as though those people had been told about something real.

A send that fails because the person blocked the bot or deleted the chat
deletes their waitlist row: they have opted out, and carrying them forward
would mean retrying that failure at every future round.

The announce does not reserve seats. It is a starting pistol, not a
guarantee, and the reply on arrival is the same claim path as anyone else's.

## Broadcast pacing

`/advert` sends sequentially with the comment that "a household is a handful
of chats, and one at a time keeps well clear of Telegram's rate limits
without any pacing logic". A hundred members is the moment that stops being
true — Telegram's bulk limit is around 30 messages per second, and a
sequential loop on a fast connection can pass it.

The send loop moves into one `broadcast` helper used by both `/advert` and
`/invite announce`: same sequential sends and same per-recipient failure
handling as today, plus a small delay between messages to stay under 30 per
second, and honouring `RetryAfter` when Telegram asks for it rather than
counting the message as failed.

This is not new scope so much as the existing feature meeting the number of
users this design is built to produce.

## Admin surface

Gated on `admin_user_ids`, the same check `/advert` uses.

- `/invite new <name> [capacity]` — capacity defaults to 100. Validates the
  name against `A-Za-z0-9_-` and 64 characters, refuses a name already
  used, and replies with `https://t.me/<username>?start=<name>`. The bot's
  username comes from `get_me`.
- `/invite status` — every round with seats used, capacity, and open or
  closed; plus how many people are waiting.
- `/invite close <name>` / `/invite open <name>` — stop or resume admitting
  without waiting for the round to fill.
- `/invite announce <name>` — as above.
- `/kick <user_id>` / `/unkick <user_id>` — set or clear `revoked_at`, and
  drop from or restore to the membership set.

`open` and `unkick` exist because their counterparts are one-way doors.
Closing a round early or kicking the wrong id are both plausible mistakes,
and each inverse is a single `UPDATE` plus a set operation. `unkick` does
not consume a seat: the row is already counted against its round, which is
the same reason revoking did not free one.

The admin names rounds rather than having one generated. It costs no
dependency, `t.me/scout_bot?start=autumn-drop` reads better in a post than a
random blob, and an admin who wants an unguessable round can type one. The
code is an identifier, not a secret — the link is meant to be shared — but
naming a round before announcing it should not let anyone drain it.

`/invite` and `/kick` are appended to `ADMIN_HELP`, so they are discoverable
by the person who can use them without advertising to everyone else.

## Daily cap

`INVITE_DAILY_REQUESTS`, default 20. Checked in `handle_text` and
`handle_photo`: `COUNT(*) FROM request_log WHERE user_id = ? AND kind IN
('text','photo') AND created_at >= current_date`. `request_log` already
carries the rows and the timestamps, so this adds a query and no writes.

The check runs *before* `log_request`, so a refused message is not itself
logged. Otherwise a person over their cap would keep pushing their own count
up by being told they are over it, and the daily total in `/stat` would
count refusals as work.

The day boundary is DuckDB's `current_date`, i.e. the container clock. The
container runs UTC, which is what the reply says.

Ids in `ALLOWED_TELEGRAM_USER_IDS` skip the check. Founders are the people
paying for the bot.

Reactions and `flight_search` rows are not counted: a reaction is not a
request, and a flight search is a sub-event of a request already counted,
so counting it would charge a message twice.

The cap bounds volume, not the cost of one message. A single request can
still fan out to fifteen Kagi queries and a flexible flight window; those
have their own budgets in `SearchBudget` and `FlightBudget`.

## What each person sees

| Who | What they send | What happens |
|---|---|---|
| Stranger | `/start <valid, open round>` | Welcomed, admitted, shown `HELP` |
| Stranger | `/start <full or closed round>` | "All invites are gone — wait for the next round. I'll message you here when one opens." Added to the waitlist. |
| Stranger | `/start <unknown code>` | Same reply as full |
| Stranger | bare `/start` | "Scout is invite-only right now." |
| Stranger | anything else | Silence, as today |
| Revoked | any `/start` | "Your access was removed." No waitlist row. |
| Member | `/start <any code>` | "You're already in", plus `HELP` |
| Member | over the daily cap | "You've used today's N requests. It resets at midnight UTC." |

Silence for a stranger's ordinary messages is deliberate and unchanged: a
bot that answers everyone who finds it is a bot that can be made to send
mail on a stranger's behalf. A bare `/start` is the one exception, because
it is the single most likely thing a person does with a bot link, and
silence there reads as broken rather than as closed.

## Error handling

- **DuckDB unavailable during a claim.** Reply that something went wrong and
  do not admit. Failing closed is right: a failure that admits people is a
  failure that overfills the round.
- **Membership set and table disagree.** The table wins. It is the only
  thing that survives a restart, and the set is rebuilt from it at startup.
- **`get_me` fails when minting a round.** The round is still created; the
  reply carries the code and says the link could not be built. Losing the
  round because the username lookup blipped would be worse.
- **Announce partially delivered.** Same reporting as `/advert` today: count
  sent, list who could not be reached. `invited_at` is stamped per success,
  so a re-run reaches only the people the first run missed.

## Testing

Store level, against a temp database:

- A round of N admits exactly N; the N+1th claim is refused
- Concurrent claims never oversell
- A second claim by the same user is a no-op and does not spend a seat
- A revoked user cannot rejoin, through their old code or a new one
- Revoking does not return the seat to the pool, and neither does unkicking
  consume one
- An unkicked user can talk to the bot again
- An unknown code and a closed round are both refused
- A reopened round admits again
- A turned-away user lands on the waitlist; a successful claim clears it
- `invited_at` is stamped only on the rows an announce actually reached

Pure functions:

- `is_start` and `join_code` over `/start x`, `/start@bot x`, bare `/start`,
  `/start@bot`, surrounding whitespace, and a message that merely begins
  with the word "start"
- Round name validation: the allowed alphabet, the 64-character limit,
  empty, and a duplicate name
- Capacity parsing, including a missing argument defaulting to 100

Config:

- `INVITE_DAILY_REQUESTS` parses, defaults to 20, and rejects nonsense

The gate itself is covered by `is_member` unit tests over a founder id, a
member id, a revoked id and an unknown id.

## Consequences worth stating

**The allowlist stops being the whole picture.** `ALLOWED_TELEGRAM_USER_IDS`
becomes the founder list; the members table is where growth lives. Anyone
debugging access has two places to look, and `/invite status` is the answer
to "why can this person talk to the bot".

**Rounds are how you meter cost, not the daily cap alone.** The cap bounds
each person; the round bounds how many people there are. Opening a round is
the moment to think about the bill.

**A dead link stays dead.** Codes do not carry between rounds, so a link in
a post that has scrolled away will keep saying the round is full. The
waitlist exists precisely because that link cannot become live again.
