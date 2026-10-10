# Shared Trips — Design

## Purpose

A trip belongs to one account. Couples, families and friends plan one trip
together, and today only one of them can see it. This lets several people
share a trip:

1. **People on a trip.** An owner and editors, listed on the trip.
2. **Sharing by link**, to someone already on Scout or not.
3. **Who added what.** Each card says who put it there and how.
4. **An address per trip.** Mail sent to it lands on that trip, for
   everyone on it.

Built as two slices, each with its own plan and deploy: people (1–3), then
the trip's address (4).

## Decisions taken

Settled in conversation; the rest follows from them.

- **An owner and editors.** Editors add, move, note and remove items, ask
  Scout about the trip, copy the share link and leave. Only the owner
  deletes the trip, removes people, turns the link off or replaces it, and
  replaces the trip's address. View-only guests were rejected for now.
- **The link is an invitation.** Someone without a seat who joins through
  it gets a full seat, outside the invite rounds, under the same daily cap.
  A link admits at most 10 new accounts, expires 14 days after it is made,
  and the owner can turn it off or replace it.
- **Both addresses.** A person's own address (`sasha@goodscout.fyi`) works
  as now. Each kept trip also has its own.
- **A trip's address is made, not picked:** the trip's name as a slug plus
  a short random tail, `hong-kong-sep-4k7q@goodscout.fyi`. The owner can
  replace it.
- **Everyone on the trip is told about its mail.** Forwarded to each
  participant with a verified email, a Telegram message with Add and
  Ignore to each participant on Telegram. The first press settles it.
- **A shared trip, private chats.** Each person talks to Scout in their own
  threads. No group thread.
- **A participants table beside the owner.** `trips.account_id` keeps
  meaning the owner; editors are rows in a new table. Making the owner a
  row too was rejected: DuckDB cannot drop the `UNIQUE (account_id,
  name_key)` constraint, so it would mean rebuilding `trips`.
- **No trip is renamed without someone asking.** A name clash on joining
  is the joiner's to resolve by renaming their own trip.

## Slice 1 — People

### Data

Schema step 21. New tables, and nullable columns added last, in this order.
Nothing is made `NOT NULL` by the step: DuckDB refuses that on a table
with rows, which took production down at step 19. The migration test runs
against a database stripped of all of it, with trips and items in it.

```sql
CREATE TABLE IF NOT EXISTS trip_people (
    trip_id         BIGINT NOT NULL,
    account_id      BIGINT NOT NULL,
    name            TEXT NOT NULL,
    joined_at       TIMESTAMP NOT NULL DEFAULT current_timestamp,
    conversation_id BIGINT,
    PRIMARY KEY (trip_id, account_id)
);
CREATE TABLE IF NOT EXISTS trip_links (
    token      TEXT PRIMARY KEY,
    trip_id    BIGINT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    expires_at TIMESTAMP NOT NULL,
    admits     BIGINT NOT NULL DEFAULT 0,
    revoked_at TIMESTAMP
);
ALTER TABLE trips      ADD COLUMN IF NOT EXISTS owner_name TEXT;
ALTER TABLE trip_items ADD COLUMN IF NOT EXISTS added_by   BIGINT;
ALTER TABLE trip_items ADD COLUMN IF NOT EXISTS added_via  TEXT;
```

- `trip_people` holds editors only. A row going away is a person leaving
  or being removed; their items keep `added_by`.
- `trip_people.name` is what labels show. Stored with the person rather
  than looked up, so a person who left is still named on what they added.
- `trip_people.conversation_id` is the editor's own thread for this trip,
  set the first time they ask about it. NULL until then, and set back to
  NULL if that thread is deleted, as `trips.conversation_id` is.
- `trips.owner_name` is the owner's name in labels. NULL until the trip is
  first shared; then filled the same way a joiner's is (below).
- `trip_links.token` is 16 random bytes, base64url, 22 characters. One live
  link per trip: making a new one revokes the old.
- `trip_items.added_by` is an account id; `added_via` is `page`, `scout`
  or `mail`. Both NULL on items made before this step, which show no label.

### Reaching a trip

Two store questions replace today's owner check.

- `can_reach(account_id, trip_id)` — the owner, or a row in `trip_people`.
- `is_owner(account_id, trip_id)` — `trips.account_id` only.

`find_trip`, `list_trips`, `list_kept_trips` and `trip_by_id` find trips
the account can reach. Every trip route and every desk tool goes through
them, so an editor can do on a shared trip what the owner can, except the
owner-only actions, which check `is_owner` and answer 403.

A trip is still found by name. The names an account reaches must stay
distinct, which the `UNIQUE` constraint no longer guarantees on its own.
Nothing renames a trip today, and `upsert_trip` given a name the account
already reaches finds that trip rather than making another, so the only
way to a clash is joining. Joining checks it in Rust under the store lock:
when the joiner already reaches a trip of that name, the join is refused
with the clash named, and the join page offers to rename the joiner's own
trip, which they then confirm. That rename is a new store operation,
`rename_trip`, on the joiner's own trip only; the shared one is never
renamed.

### The link

`POST /chat/trips/link` — `{trip}`, any participant. Returns the live
link, making one if there is none or the last has expired, been revoked
or filled. `POST /chat/trips/link-new` — owner only: revokes the live link
and makes another. `POST /chat/trips/link-off` — owner only: revokes it.

What is shared is `https://goodscout.fyi/join/<token>`. In the Mini App the
Share action also offers `https://t.me/<bot>?startapp=j_<token>`, which
opens the Mini App for someone who has already chatted with the bot; a
`?start=` link would not, since Telegram only delivers it in an empty
chat. This needs the bot's Main Mini App set in BotFather, a one-time step
for the admin. Until it is set, the web link is the only one offered.

### Joining

`GET /join/<token>` is a public page, no admission required. It reads the
link and shows the trip's name, its first and last dates, and the names of
the people on it — nothing else from the trip.

- **Link not usable.** Expired, revoked or full: a page that says which,
  and names the owner to ask for a new link. No sign-in is offered.
- **Not signed in.** "Continue with Telegram" and "Use email", the existing
  sign-in flows, each returning to `/join/<token>` afterwards. The return
  path is checked to be exactly `/join/<22 base64url characters>`; nothing
  else is accepted as a return.
- **Signed in, already on the trip** (owner or editor): redirect to
  `/chat?trip=<name>`.
- **Signed in, revoked member:** "You no longer have access to Scout." No
  join.
- **Signed in, otherwise:** a name field filled from the Telegram first
  name, or the part of the email before the `@`, cut to 40 characters, and
  **Join trip**. With a name clash, the clash and a rename field for the
  joiner's own trip instead.

`POST /join/<token>` — `{name, rename?}`, with the form token. One store
transaction, in this order:

1. The link is live: not revoked, not expired.
2. The account is not revoked.
3. If `rename` is given, rename the joiner's own clashing trip; refuse if
   the new name clashes too.
4. Refuse if a name clash remains.
5. If the account has no seat: refuse if `admits` is 10; otherwise insert a
   `members` row with code `trip-<trip id>` and add 1 to `admits`.
6. Insert into `trip_people`; fill `trips.owner_name` if it is NULL.

Then 303 to `/chat?trip=<name>`. Two people racing for the last place: the
store lock admits one; the other's transaction reaches step 5 with
`admits` at 10 and gets the "full" page.

**In the Mini App,** `/tg` already verifies Telegram's launch data. A
`start_param` beginning `j_` makes the launch page open
`/join/<token>?in=telegram` with the session it has just made, so the page
lands on the signed-in states above, inside the Mini App. A Telegram
account with no Scout account gets one made by the sign-in, as the website
already does.

### Leaving and removing

`POST /chat/trips/leave` — `{trip}`, editors only. `POST
/chat/trips/remove-person` — `{trip, account_id}`, owner only. Both delete
the `trip_people` row. The person's threads stay theirs; the trip
disappears from their list on the next fetch; any request about it from
them is 404. A seat given by a link stays.

### Who added it

Every write that makes an item records who and how:

| Path | `added_by` | `added_via` |
|---|---|---|
| Add from the page (an arrival's Add) | the person pressing | `mail` |
| A desk tool in a Scout run | the run's account | `scout` |
| A Telegram arrival's Add button | the person pressing | `mail` |
| Anything else made on the page | the person | `page` |

A move or a note does not change `added_by`. On the wire a `TripItem`
gains `added_by: Option<String>` — the name, resolved from the owner's or
the person's stored name, `"you"` for the reader — and `added_via`.

### On the page

- **The People line** in the trip's header, under the dates: "You, Masha,
  Dan · Share", or "Only you · Share" on a trip of one. It opens a panel:
  - each person's name, the owner marked "owner";
  - Copy link;
  - for an editor, Leave trip, which asks once;
  - for the owner, Remove beside each editor, which asks once, Turn link
    off and New link, and the link's state: "Link works until 25 Oct · 2 of
    10 new people used".
- **The label** in the card's footer, muted: "Added by Masha", "Added by
  Masha, via Scout", "Added by Masha, from mail", "Added by you". Shown only
  while the trip has more than one person. None on an item with no
  `added_by`.
- **The printed plan** prints the label in small type on a shared trip.
- **Refetch on focus.** The page refetches the trips when the tab or Mini
  App becomes visible again, so another person's changes show without a
  reload. Edits still carry the stale-tab rule: a changed item is 409 and a
  reload.
- **The Mini App** gets the same header line, panel and labels; Share is
  also in its ⋯ menu.

### Scout

- The desk's trip tools find trips by name among those the account
  reaches, and record the run's account as `added_by` with `scout`.
- `delete_trip` from an editor's run is refused: "Only Sasha can delete
  this trip. You can leave it from the trip page."
- The desk's description of a shared trip gains one line: "Shared: Sasha
  (owner), Masha, Dan. You are an editor." On a trip of one, nothing.
- An editor's "Ask about this trip" goes to their own thread for it,
  `trip_people.conversation_id`, made on first use. `composerTarget` reads
  which thread from the trip as served, as it does for the owner now.
- A move's check runs in the mover's thread and nudges the mover only. A
  warning is on the item, so everyone sees it.

## Slice 2 — The trip's address

### Data

Schema step 22, on the same terms as step 21.

```sql
ALTER TABLE trips        ADD COLUMN IF NOT EXISTS address    TEXT;
ALTER TABLE inbound_mail ADD COLUMN IF NOT EXISTS trip_id    BIGINT;
ALTER TABLE arrivals     ADD COLUMN IF NOT EXISTS trip_id    BIGINT;
ALTER TABLE arrivals     ADD COLUMN IF NOT EXISTS settled_by BIGINT;
CREATE TABLE IF NOT EXISTS mail_forwards (
    mail_id      BIGINT NOT NULL,
    account_id   BIGINT NOT NULL,
    forwarded_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (mail_id, account_id)
);
```

- `trips.address` is the local part, lowercase. Unique across trip
  addresses and personal handles together, checked in Rust under the store
  lock as `set_handle` already is. Setting a personal handle checks trip
  addresses too.
- `inbound_mail.account_id` for a trip's mail is the owner, so every
  existing per-account query keeps working; `trip_id` says it came to the
  trip.
- `mail_forwards` records each person a mail has been forwarded to, so a
  retry after a partial failure does not send anyone a second copy.
  `inbound_mail.forwarded_at` keeps its meaning for personal mail.

### The address

- Made when a trip is kept: the name lowercased, anything outside `a-z0-9`
  turned into `-`, runs of `-` collapsed, trimmed, cut to 20 characters,
  then `-` and 4 random characters from `a-z0-9`. Drawn again on a clash.
  A name with nothing left after slugging gets `trip`.
- Trips kept before step 22 get one when first served.
- Drafts have none.
- `POST /chat/trips/address-new` — owner only: draws a new one. The old one
  becomes unknown, as any unheld handle is.
- Shown in the People panel with Copy. On a trip page, the to-book banner
  names the trip's address instead of the person's own.

### Mail in

`our_handle` is unchanged. The lookup after it tries personal handles,
then trip addresses. Mail for a trip address is recorded with the owner as
`account_id` and the trip as `trip_id`.

The worker, for a trip's mail:

- **Reading.** The extractor runs as now. Matching an arrival to a trip is
  skipped: every arrival from it gets that `trip_id` and sits on that trip,
  pending, for everyone on it. Non-bookings appear under Other mail for
  everyone on the trip.
- **Forwarding.** One forward per participant with an email identity, each
  to that person alone, so nobody sees another's address. Skipped for the
  participant who sent the mail, as a person's own mail is skipped now.
  Each success writes a `mail_forwards` row; a retry skips those rows.
- **Telegram.** Each participant with a Telegram delivery gets the
  arrival's message with Add and Ignore, keyed in the outbox on the
  arrival and the person so a retry sends no second copy.
- **Not counted** toward anyone's daily cap, as personal mail is not.

### Settling

The first Add or Ignore, from the page or Telegram, from anyone on the
trip, writes `settled_by` and settles the arrival as now. A later press:

- on Telegram, answers "Already added by Masha" (or "Already ignored by
  Masha") and edits that one message to say so;
- on the page, is 409 and a reload, as a stale tab is.

Messages already sent to others are not edited: that would mean storing
every message id for little gain.

The `arr:add` and `arr:ign` callbacks check `can_reach` on the arrival's
trip rather than the arrival's account.

## Errors

- Join link expired, revoked or full, and a revoked member: each its own
  page, naming the owner to ask.
- An owner-only action from an editor: 403.
- A removed editor asking about the trip: 404; the page drops it.
- A name clash on joining: refused, naming the clash, with the rename
  offered.
- A trip address that cannot be drawn after 10 tries: logged, the trip
  served without one, tried again on the next serve.

## Tests

- **Store:** steps 21 and 22 on a database with trips, items, arrivals and
  mail in it; `can_reach` against `is_owner`; joining, including the seat
  and `admits`; a link expired, revoked, full; the last place raced; a name
  clash on joining, resolved by a rename, and a rename that clashes
  again; `upsert_trip` finding a shared trip by name; leaving and
  removing; an address unique against handles, and a handle against
  addresses; replacing an address.
- **Routes:** `/join` signed out, signed in, already on the trip, revoked;
  the return path refused unless it is a join path; each owner-only route
  refusing an editor; the labels and `added_via` on the wire.
- **Inbound and worker:** mail to a trip address becomes arrivals on that
  trip; one forward per participant and none to the sender; a retry sends
  no second forward; the first Telegram press settles and a second is told
  who did.
- **Desk:** an editor finds a shared trip, adds an item and is credited
  with `scout`; an editor's delete is refused.
- **Node:** the People line's text; the label text and when it is hidden;
  refetch on becoming visible.
- **Browser,** phone and desktop width, two accounts on the stand-in
  server: share, join from signed out, the label on an item each added,
  leave, remove.

## Out of scope

- View-only guests.
- Handing a trip to another owner; the owner leaving.
- A shared thread with Scout.
- Telegram messages about other people's edits, or telling editors the
  owner deleted the trip.
- Editing the Telegram messages others already received when an arrival
  is settled.
- Choosing a trip's address by hand.
