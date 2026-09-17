use anyhow::Result;
use duckdb::{params, Connection, OptionalExt, Row};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// How many times a mirrored message is retried before it is left alone.
///
/// The reminder path retries indefinitely and that is safe, because a date
/// bounds it. An outbox row has no such bound: a reader who blocks the bot
/// would otherwise be retried against forever.
pub const MIRROR_ATTEMPTS: i64 = 5;

/// How many times the extractor is pointed at one mail before it is left
/// as failed. Same reasoning as `MIRROR_ATTEMPTS`: nothing bounds a mail
/// the model keeps choking on except this.
pub const MAIL_ATTEMPTS: i64 = 3;

/// Bytes of a tool result kept on a trace row. A flight search is a few
/// thousand characters; a fetched page can be far more, and the page is
/// not what anyone reads a trace for.
pub const TRACE_RESULT_CAP: usize = 64 * 1024;

const MIGRATIONS: &str = r#"
CREATE SEQUENCE IF NOT EXISTS purchases_id_seq;
CREATE TABLE IF NOT EXISTS purchases (
    id BIGINT PRIMARY KEY DEFAULT nextval('purchases_id_seq'),
    account_id BIGINT NOT NULL,
    item TEXT NOT NULL,
    store TEXT NOT NULL,
    url TEXT,
    price DOUBLE,
    currency TEXT,
    notes TEXT,
    purchased_at TEXT,
    recorded_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE SEQUENCE IF NOT EXISTS reminders_id_seq;
CREATE TABLE IF NOT EXISTS reminders (
    id BIGINT PRIMARY KEY DEFAULT nextval('reminders_id_seq'),
    account_id BIGINT NOT NULL,
    channel TEXT NOT NULL DEFAULT 'telegram',
    address TEXT NOT NULL,
    item TEXT NOT NULL,
    interval_days BIGINT NOT NULL,
    next_due TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE IF NOT EXISTS user_facts (
    account_id BIGINT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (account_id, key)
);
CREATE TABLE IF NOT EXISTS request_log (
    account_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- Telegram display names, refreshed on every request so /stat can label a
-- user id with something readable. Names change; the id is the identity.
CREATE TABLE IF NOT EXISTS users (
    account_id BIGINT PRIMARY KEY,
    display_name TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- Admission. `ALLOWED_TELEGRAM_USER_IDS` stays the founder list; these three
-- tables are where growth lives — see the invite-links design doc.
--
-- A round is a named code with a capacity, shared as a t.me deep link.
CREATE TABLE IF NOT EXISTS invite_rounds (
    code       TEXT PRIMARY KEY,
    capacity   BIGINT NOT NULL,
    open       BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- One row per person admitted, ever. `account_id` is the key, so a person
-- belongs to one round; `revoked_at` set means removed, and the row stays
-- put so the seat is not handed back.
CREATE TABLE IF NOT EXISTS members (
    account_id    BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    joined_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    revoked_at TIMESTAMP
);
-- People a full or unknown round turned away. Where to reach them lives in
-- `deliveries`, because the START they pressed is the permission to do it.
CREATE TABLE IF NOT EXISTS waitlist (
    account_id BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    seen_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    invited_at TIMESTAMP
);
-- A named plan. The itinerary is durable; prices are not, so nothing here
-- holds an offer id — see the trips design doc.
CREATE SEQUENCE IF NOT EXISTS trips_id_seq;
CREATE TABLE IF NOT EXISTS trips (
    id          BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    account_id     BIGINT NOT NULL,
    name        TEXT NOT NULL,
    -- lowercased `name`: the trip is addressed by what the traveller calls
    -- it, and "September" and "september" are the same trip.
    name_key    TEXT NOT NULL,
    adults      BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT,
    status      TEXT NOT NULL DEFAULT 'planning',
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    -- Last, so a fresh database and a migrated one — where this arrives by
    -- ALTER TABLE in step 9 — have the same column order. NULL means the
    -- chat that made this trip is gone; the next chat to touch it adopts it.
    conversation_id BIGINT,
    -- Last, so a fresh database and a migrated one — where this arrives by
    -- ALTER TABLE in step 10 — have the same column order. False means a
    -- draft: built automatically while searching, invisible to the
    -- traveller until they ask to keep it.
    kept        BOOLEAN NOT NULL DEFAULT false,
    UNIQUE (account_id, name_key)
);
-- Where and when. This is all that gets re-searched.
CREATE TABLE IF NOT EXISTS trip_segments (
    trip_id        BIGINT NOT NULL,
    position       BIGINT NOT NULL,
    origin         TEXT NOT NULL,
    destination    TEXT NOT NULL,
    departure_date TEXT NOT NULL,
    -- Hands out candidate numbers and never takes one back. Deriving the next
    -- number from max(candidate) over live rows recycles it as soon as the
    -- highest is dropped, and a traveller who was shown "option 2" would
    -- then be given a different flight under the same name.
    next_candidate BIGINT NOT NULL DEFAULT 1,
    PRIMARY KEY (trip_id, position)
);
-- The options on a segment. Several may sit here undecided; at most one
-- carries `chosen`, which is enforced in Rust because `false` repeats.
CREATE TABLE IF NOT EXISTS segment_candidates (
    trip_id            BIGINT NOT NULL,
    position           BIGINT NOT NULL,
    candidate          BIGINT NOT NULL,
    chosen             BOOLEAN NOT NULL DEFAULT false,
    airline            TEXT NOT NULL,
    flight_numbers     TEXT NOT NULL,
    itinerary          TEXT NOT NULL,
    departing_at_local TEXT,
    arriving_at_local  TEXT,
    duration_minutes   BIGINT,
    quoted_price       DOUBLE,
    quoted_currency    TEXT,
    quoted_at          TIMESTAMP,
    source             TEXT,
    PRIMARY KEY (trip_id, position, candidate)
);
-- The trip's timeline: flights, stays, activities and transport in one
-- date-ordered list. Replaces trip_segments, which stays above until a
-- later release drops it. `position` is recomputed by date on every write.
CREATE SEQUENCE IF NOT EXISTS trip_items_id_seq;
CREATE TABLE IF NOT EXISTS trip_items (
    id                BIGINT PRIMARY KEY DEFAULT nextval('trip_items_id_seq'),
    trip_id           BIGINT NOT NULL,
    position          BIGINT NOT NULL,
    kind              TEXT NOT NULL,
    title             TEXT NOT NULL,
    place             TEXT,
    origin            TEXT,
    destination       TEXT,
    starts_at         TEXT,
    ends_at           TEXT,
    date              TEXT NOT NULL,
    booked            BOOLEAN NOT NULL DEFAULT false,
    confirmation_code TEXT,
    price             DOUBLE,
    currency          TEXT,
    notes             TEXT,
    arrival_id        BIGINT,
    next_candidate    BIGINT NOT NULL DEFAULT 1,
    created_at        TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at        TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- The options on a flight item; segment_candidates keyed by item id.
CREATE TABLE IF NOT EXISTS item_candidates (
    item_id            BIGINT NOT NULL,
    candidate          BIGINT NOT NULL,
    chosen             BOOLEAN NOT NULL DEFAULT false,
    airline            TEXT NOT NULL,
    flight_numbers     TEXT NOT NULL,
    itinerary          TEXT NOT NULL,
    departing_at_local TEXT,
    arriving_at_local  TEXT,
    duration_minutes   BIGINT,
    quoted_price       DOUBLE,
    quoted_currency    TEXT,
    quoted_at          TIMESTAMP,
    source             TEXT,
    PRIMARY KEY (item_id, candidate)
);
CREATE SEQUENCE IF NOT EXISTS accounts_id_seq;
-- A person, independent of how they reach Scout. Deliberately almost empty:
-- everything knowable about someone belongs to one of their identities or to
-- their data, not here.
CREATE TABLE IF NOT EXISTS accounts (
    id         BIGINT PRIMARY KEY DEFAULT nextval('accounts_id_seq'),
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    -- Whether this person sees a Trace under each answer in the browser.
    -- Only an admin can turn it on; the flag lives here so it survives a
    -- restart and a new tab alike.
    debug      BOOLEAN NOT NULL DEFAULT false,
    -- The local part of this person's booking address, lowercase; NULL
    -- until they pick one. Unique by the check in `set_handle`, not by an
    -- index: DuckDB indexes and row updates do not mix, and unlike the
    -- UNIQUE columns on outbox and inbound_mail, which are written once,
    -- this is the indexed value itself being rewritten when someone
    -- changes their handle. Last, so a migrated database has the same
    -- column order.
    handle     TEXT
);
-- One row per way of proving you are that account. `kind` is 'telegram'
-- today; a web login is a second kind. The primary key is what stops one
-- Telegram id from being claimed by two accounts.
CREATE TABLE IF NOT EXISTS identities (
    account_id  BIGINT NOT NULL,
    kind        TEXT NOT NULL,
    external_id TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (kind, external_id)
);
-- Where to reach an account on a channel, when nothing more specific says
-- otherwise. Replaces `user_chats`.
CREATE TABLE IF NOT EXISTS deliveries (
    account_id BIGINT NOT NULL,
    channel    TEXT NOT NULL,
    address    TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (account_id, channel)
);
CREATE SEQUENCE IF NOT EXISTS outbox_id_seq;
-- Messages waiting to be mirrored to a channel the reader also uses.
--
-- A table rather than an in-memory queue because the web crate cannot reach
-- the Telegram bot — the dependency runs scout-telegram -> scout-web ->
-- scout-core — and because an in-memory queue loses a half-sent backfill on
-- every deploy.
--
-- `turn_key` is what makes enqueueing idempotent, and it exists because a
-- position is not something the mirror can hold on to: messages are swept
-- when their thread expires, a channel mirrors turns rather than rows, and
-- what a run appends is model traffic that `turns_of` mostly discards. So
-- "mirrored up to here" cannot be a pointer into `messages`. "Have I
-- already sent this turn" can be answered; "how far did I get" cannot.
--
-- A row with `sent_at` already set was never going to be sent: that is how
-- the Telegram channel records its own messages so a backfill does not echo
-- them back at it.
CREATE TABLE IF NOT EXISTS outbox (
    id         BIGINT PRIMARY KEY DEFAULT nextval('outbox_id_seq'),
    account_id BIGINT NOT NULL,
    channel    TEXT NOT NULL,
    address    TEXT NOT NULL,
    body       TEXT NOT NULL,
    turn_key   TEXT NOT NULL,
    attempts   BIGINT NOT NULL DEFAULT 0,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    sent_at    TIMESTAMP,
    UNIQUE (account_id, channel, turn_key)
);
-- Who wants their browser thread mirrored. Presence is the setting: a row
-- means on, no row means off, and there is no boolean that can fall out of
-- step with itself.
CREATE TABLE IF NOT EXISTS mirrored_accounts (
    account_id BIGINT PRIMARY KEY,
    enabled_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- Magic-link tokens. A row rather than a signed value because a link must
-- be single-use: a replayable one is a standing account key sitting in an
-- inbox. Sign-in is rare, so the store mutex is cheap here in a way it
-- would not be on a per-request session check.
CREATE TABLE IF NOT EXISTS login_tokens (
    token_hash  TEXT PRIMARY KEY,
    email       TEXT NOT NULL,
    -- Set when linking an address to an account that is already signed in;
    -- NULL when the link is a sign-in and the account is not known yet.
    account_id  BIGINT,
    expires_at  TIMESTAMP NOT NULL,
    -- Kept rather than deleted, so "already used" and "expired" stay
    -- distinguishable — they call for different advice.
    consumed_at TIMESTAMP
);
CREATE SEQUENCE IF NOT EXISTS conversations_id_seq;
-- A rolling thread. `scope` keeps a group chat's history out of the
-- account's private thread: 'direct' is the 1:1 chat and the web app, which
-- share; a group is 'telegram:<chat_id>'.
CREATE TABLE IF NOT EXISTS conversations (
    id            BIGINT PRIMARY KEY DEFAULT nextval('conversations_id_seq'),
    account_id    BIGINT NOT NULL,
    scope         TEXT NOT NULL,
    pending_draft TEXT,
    started_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    -- Last two, so a fresh database and a migrated one — where these
    -- arrive by ALTER TABLE in step 7 — have the same column order.
    -- What the sidebar calls it. Null until the first answer lands; then
    -- the first message trimmed, unless someone renamed it. See the
    -- threads design doc.
    title         TEXT,
    -- "Permanent": exempt from the 48-hour expiry. Nothing else.
    pinned        BOOLEAN NOT NULL DEFAULT false
);
CREATE SEQUENCE IF NOT EXISTS messages_id_seq;
-- `body` is a serde_json `rig::completion::Message`. Storing the whole
-- message rather than plain text keeps tool calls and their results paired,
-- which `trim_history` depends on.
CREATE TABLE IF NOT EXISTS messages (
    id              BIGINT PRIMARY KEY DEFAULT nextval('messages_id_seq'),
    conversation_id BIGINT NOT NULL,
    position        BIGINT NOT NULL,
    body            TEXT NOT NULL,
    created_at      TIMESTAMP NOT NULL DEFAULT current_timestamp,
    -- The run that wrote this message, so an answer can find its trace.
    -- Null on rows written before runs were recorded.
    run_id          BIGINT
);
CREATE SEQUENCE IF NOT EXISTS runs_id_seq;
-- One row per agent run. The trace under a browser answer hangs off it,
-- and messages point at it so an answer can find its trace.
CREATE TABLE IF NOT EXISTS runs (
    id              BIGINT PRIMARY KEY DEFAULT nextval('runs_id_seq'),
    account_id      BIGINT NOT NULL,
    conversation_id BIGINT NOT NULL,
    started_at      TIMESTAMP NOT NULL DEFAULT current_timestamp,
    ended_at        TIMESTAMP,
    outcome         TEXT,
    detail          TEXT
);
CREATE SEQUENCE IF NOT EXISTS run_traces_id_seq;
CREATE TABLE IF NOT EXISTS run_traces (
    id          BIGINT PRIMARY KEY DEFAULT nextval('run_traces_id_seq'),
    run_id      BIGINT NOT NULL,
    seq         BIGINT NOT NULL,
    kind        TEXT NOT NULL,
    tool        TEXT,
    args        TEXT,
    nested      BOOLEAN NOT NULL DEFAULT false,
    started_at  TIMESTAMP NOT NULL,
    duration_ms BIGINT,
    status      TEXT,
    detail      TEXT,
    result      TEXT,
    truncated   BOOLEAN NOT NULL DEFAULT false
);
CREATE SEQUENCE IF NOT EXISTS inbound_mail_id_seq;
-- One row per email received at a booking address, as delivered. The body
-- is what the extractor reads; `status` walks new -> extracting -> done or
-- failed, and `attempts` bounds the retries the way `outbox` does.
CREATE TABLE IF NOT EXISTS inbound_mail (
    id           BIGINT PRIMARY KEY DEFAULT nextval('inbound_mail_id_seq'),
    account_id   BIGINT NOT NULL,
    -- The provider's message id, so a redelivery is a no-op.
    provider_id  TEXT NOT NULL UNIQUE,
    from_address TEXT NOT NULL,
    subject      TEXT,
    text         TEXT,
    html         TEXT,
    truncated    BOOLEAN NOT NULL DEFAULT false,
    received_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    status       TEXT NOT NULL DEFAULT 'new',
    attempts     BIGINT NOT NULL DEFAULT 0,
    -- When the last attempt began, so a retry waits a while: the model
    -- that refused a minute ago is the model that refuses now.
    attempted_at TIMESTAMP,
    forwarded_at TIMESTAMP,
    error        TEXT
);
CREATE SEQUENCE IF NOT EXISTS attachments_id_seq;
-- A file that came with a mail. `item_id` is set once the booking is added
-- to a trip, and is what keeps the file when the mail is swept.
CREATE TABLE IF NOT EXISTS attachments (
    id       BIGINT PRIMARY KEY DEFAULT nextval('attachments_id_seq'),
    mail_id  BIGINT NOT NULL,
    item_id  BIGINT,
    filename TEXT NOT NULL,
    mime     TEXT NOT NULL,
    bytes    BLOB,
    text     TEXT
);
CREATE SEQUENCE IF NOT EXISTS mail_parts_id_seq;
-- What the inbound webhook said each of a mail's MIME parts is: the two
-- header fields that tell a ticket from the sender's furniture, and
-- nothing else. The attachment listing is the authority on which parts
-- exist and what they are called, so neither name nor type is copied
-- here; a second copy could only disagree with it. Unlike an attachment,
-- a part is never anything but its mail's, so it is deleted with the mail
-- and has no `item_id` to spare it.
CREATE TABLE IF NOT EXISTS mail_parts (
    id                  BIGINT PRIMARY KEY DEFAULT nextval('mail_parts_id_seq'),
    mail_id             BIGINT NOT NULL,
    -- The provider's id for the part, which is what ties it to a row of
    -- the attachment listing.
    provider_id         TEXT NOT NULL,
    content_disposition TEXT,
    content_id          TEXT
);
CREATE SEQUENCE IF NOT EXISTS arrivals_id_seq;
-- What the extractor read out of a mail. A booking waits here as `pending`
-- until its owner adds it to a trip or ignores it; a non-booking is only
-- ever listed under Other mail.
CREATE TABLE IF NOT EXISTS arrivals (
    id                BIGINT PRIMARY KEY DEFAULT nextval('arrivals_id_seq'),
    account_id        BIGINT NOT NULL,
    mail_id           BIGINT NOT NULL,
    booking           BOOLEAN NOT NULL,
    kind              TEXT,
    title             TEXT,
    place             TEXT,
    origin            TEXT,
    destination       TEXT,
    date              TEXT,
    starts_at         TEXT,
    ends_at           TEXT,
    timezone          TEXT,
    confirmation_code TEXT,
    price             DOUBLE,
    currency          TEXT,
    travellers        TEXT,
    confidence        DOUBLE,
    summary           TEXT NOT NULL,
    trip_id           BIGINT,
    status            TEXT NOT NULL DEFAULT 'pending',
    item_id           BIGINT,
    decided_at        TIMESTAMP,
    -- Which flight the confirmation named, and where it changes planes.
    -- Only a flight has these, and only these say on the leg's card what
    -- was actually bought. Last, and in this order, so a fresh database and
    -- a migrated one — where they arrive by ALTER TABLE in step 17 — have
    -- the same column order.
    airline           TEXT,
    flight_number     TEXT,
    -- The airports changed at, comma-separated, the way `travellers` keeps
    -- its list: a connection is one booking, and the leg's itinerary strip
    -- has to show the stop or the card calls a one-stop ticket direct.
    stops             TEXT
);
"#;

/// A purchase as the agent sees it. `purchased_at` is an ISO `YYYY-MM-DD`
/// string; TEXT keeps date handling trivial and sorts chronologically.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Purchase {
    pub id: i64,
    pub item: String,
    pub store: String,
    pub url: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub notes: Option<String>,
    pub purchased_at: Option<String>,
}

/// Also serves as the `record_purchase` tool's Args.
#[derive(Debug, Clone, Deserialize)]
pub struct NewPurchase {
    pub item: String,
    pub store: String,
    pub url: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub notes: Option<String>,
    pub purchased_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reminder {
    pub id: i64,
    #[serde(skip)]
    pub account_id: i64,
    #[serde(skip)]
    pub channel: String,
    #[serde(skip)]
    pub address: String,
    pub item: String,
    pub interval_days: i64,
    pub next_due: String, // YYYY-MM-DD
}

/// A named plan. `id` is not serialised: it is noise to the model, and
/// exposing it invites addressing a trip by something the traveller never
/// said.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trip {
    #[serde(skip)]
    pub id: i64,
    pub name: String,
    pub adults: i64,
    pub cabin_class: Option<String>,
    /// `planning` or `finalised`. Any edit that changes what would be
    /// priced — a segment, its options, the passenger count or the cabin —
    /// returns it to `planning`: the prices it was finalised at stopped
    /// describing the trip when the trip stopped being that trip.
    pub status: String,
    pub items: Vec<TripItem>,
    /// False while this is a draft the specialist built as it searched.
    /// The model sees this — it needs to know whether to offer to keep it.
    pub kept: bool,
}

impl Trip {
    /// The flight legs, in timeline order: what pricing and the
    /// connection notes look at, since a stay has no candidates.
    pub fn flights(&self) -> impl Iterator<Item = &TripItem> {
        self.items.iter().filter(|i| i.is_flight())
    }
}

/// One thing on a trip: a flight leg, a stay, an activity or a transport
/// booking. Flights carry a route and candidates; the rest carry a title
/// and a place. `position` is 1-based and recomputed by date on every
/// write, so it is a name for talking about the item, not an identity.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TripItem {
    #[serde(skip)]
    pub id: i64,
    pub position: i64,
    /// `flight` | `stay` | `activity` | `transport`
    pub kind: String,
    /// "AMS → LIS" for a flight, the booking's name for the rest.
    pub title: String,
    pub place: Option<String>,
    pub origin: Option<String>,
    pub destination: Option<String>,
    /// The day it starts, `YYYY-MM-DD`; the sort key.
    pub date: String,
    /// Local ISO datetime. For a flight, the chosen option's departure,
    /// filled at read time; `None` while undecided.
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub booked: bool,
    pub confirmation_code: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub notes: Option<String>,
    pub arrival_id: Option<i64>,
    pub candidates: Vec<TripCandidate>,
    /// The ticket the booking arrived with: the files forwarded on the
    /// confirmation mail, moved here by `attach_to_item` when the owner
    /// pressed Add. Empty for an item nobody attached one to — a leg the
    /// specialist searched, or a booking whose mail carried no file.
    pub attachments: Vec<scout_api::AttachmentRef>,
}

impl TripItem {
    pub fn is_flight(&self) -> bool {
        self.kind == "flight"
    }
    /// "AMS→LIS" for a flight, the title otherwise: what a sentence about
    /// this item calls it.
    pub fn route(&self) -> String {
        match (&self.origin, &self.destination) {
            (Some(o), Some(d)) => format!("{o}→{d}"),
            _ => self.title.clone(),
        }
    }
}

/// An item on its way into the database. Flights do not come through this;
/// `add_flight` builds theirs.
#[derive(Debug, Clone, PartialEq)]
pub struct NewItem {
    pub kind: String,
    pub title: String,
    pub place: Option<String>,
    pub date: String,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub notes: Option<String>,
    pub booked: bool,
    pub confirmation_code: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub arrival_id: Option<i64>,
}

/// The fields of an item an edit may touch, each one optional. See
/// `Store::update_item` for what a blank means and why a title cannot be
/// one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ItemEdit<'a> {
    pub title: Option<&'a str>,
    pub place: Option<&'a str>,
    pub date: Option<&'a str>,
    /// `HH:MM`, local to wherever the item is.
    pub time: Option<&'a str>,
    pub end_date: Option<&'a str>,
    /// Whether the traveller holds it. Theirs to say: a lunch arranged
    /// over WhatsApp is as held as a hotel that sent a confirmation, and
    /// leaving this out of an edit meant the only way to mark one was to
    /// forward an email about it.
    pub booked: Option<bool>,
    /// Empty clears it. A booking with no code is ordinary — most
    /// restaurants give none — so this never gates `booked`.
    pub confirmation_code: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TripCandidate {
    pub candidate: i64,
    pub chosen: bool,
    pub airline: String,
    /// Comma-separated and in order (`KL1007,KL0805`), because finalisation
    /// matches this against fresh search results.
    pub flight_numbers: String,
    /// The rendered line `Leg::itinerary` produces, for showing.
    pub itinerary: String,
    pub departing_at_local: Option<String>,
    pub arriving_at_local: Option<String>,
    pub duration_minutes: Option<i64>,
    /// What it cost when parked. Never refreshed — see the design doc.
    pub quoted_price: Option<f64>,
    pub quoted_currency: Option<String>,
    pub source: Option<String>,
}

/// The conversation a trip belongs to, as much of it as a client needs to
/// name the place a message will land.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TripChat {
    pub id: i64,
    pub title: Option<String>,
    /// `direct` is the thread web and 1:1 Telegram share. Anything else is
    /// a room the web client must not post into.
    pub scope: String,
}

/// A candidate on its way into the database.
#[derive(Debug, Clone, PartialEq)]
pub struct NewCandidate {
    pub airline: String,
    pub flight_numbers: String,
    pub itinerary: String,
    pub departing_at_local: Option<String>,
    pub arriving_at_local: Option<String>,
    pub duration_minutes: Option<i64>,
    pub quoted_price: Option<f64>,
    pub quoted_currency: Option<String>,
    pub source: Option<String>,
}

/// The outcome of choosing a parked flight through an account-facing
/// interface. Keeping the ownership check and the write under one store lock
/// prevents a web route from first resolving a trip it owns and then acting
/// on a row that changed before the selection landed.
#[derive(Debug, Clone, PartialEq)]
pub enum CandidateChoice {
    Chosen(Trip),
    TripNotFound,
    CandidateNotFound,
}

/// What the caller saw when it decided to act on an item — for
/// `add_candidate` and `remove_item_checked` to verify again inside the
/// same lock as the write it guards; see `add_candidate`'s own comment for
/// why the check cannot live only in the caller. Every `Some` must match;
/// `None` is "nothing to verify", not "verified".
pub struct ExpectedItem<'a> {
    pub origin: Option<&'a str>,
    pub destination: Option<&'a str>,
    pub title: Option<&'a str>,
    pub date: Option<&'a str>,
}

/// What happened when somebody pressed START on an invite link.
///
/// The three refusals that share one reply — unknown code, closed round,
/// full round — are one variant on purpose. Collapsing them here rather than
/// at the call site means no caller can accidentally tell a stranger which
/// of the three it was, and so whether a code they guessed exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Claim {
    Admitted,
    AlreadyIn,
    Revoked,
    NoRoom,
}

/// What linking a second way of signing in did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    Linked,
    AlreadyYours,
    /// Somebody else proved this identity first, and both accounts have
    /// been used. Never resolved by moving it: a wrong merge cannot be
    /// undone, so two accounts that both hold something stay two until a
    /// human decides otherwise.
    TakenByAnother,
    /// The identity belonged to another account, but one of the two was
    /// empty — no purchases, reminders, facts, requests, trips or
    /// conversations — so the empty one was absorbed and deleted.
    ///
    /// Carries the account that survived, which is *not* always the one
    /// that asked: signing in by email mints a fresh account, so the
    /// person who then adds Telegram is usually the empty side, and the
    /// account they end up in is the one their history is already under.
    /// A caller holding a session must re-issue it for this id.
    Merged { account_id: i64 },
}

/// What asking for one mail to go now came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailGone {
    /// Nobody's, or not this account's — one answer for both, so an id
    /// tried against a stranger's mail learns nothing from the reply.
    NotFound,
    /// A booking off this mail is still waiting on its owner. Deleting the
    /// mail would take that row off the trip's timeline with nothing said,
    /// so the decision comes first.
    Waiting,
    /// The worker has not finished with it. A mail deleted mid-read has a
    /// reading written against it a moment later: a row pointing at a mail
    /// that is gone, which no page lists and no sweep can reach, because
    /// both find arrivals through their mail.
    Unsettled,
    Gone,
}

/// What a magic link turned out to be worth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenOutcome {
    Valid { email: String, account_id: Option<i64> },
    Expired,
    AlreadyUsed,
    Unknown,
}

/// One round as `/invite status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundStatus {
    pub code: String,
    pub capacity: i64,
    /// Seats taken, counting revoked members: a round of 100 admits 100
    /// people once.
    pub used: i64,
    pub open: bool,
}

/// A numbered change to an existing database. `MIGRATIONS` above creates
/// tables and is safe to re-run; these are not, so each one runs at most
/// once and the number it reached is recorded.
///
/// Never renumber or edit a step that has shipped, and never insert one
/// below a number that has already run — the runner skips anything at or
/// below the recorded version, so it would be silently ignored. Append.
enum Step {
    Sql(&'static str),
    /// For work that needs a loop or a returned id — plain SQL cannot ask
    /// DuckDB for `nextval` per row and keep the mapping.
    Code(fn(&Connection) -> Result<()>),
}

/// The tables phase one introduced. Included in `MIGRATIONS` as well, so a
/// brand-new database is created in the finished shape; kept as a step so an
/// existing one still gets them.
const STEP_1_NEW_TABLES: &str = r#"
CREATE SEQUENCE IF NOT EXISTS accounts_id_seq;
-- A person, independent of how they reach Scout. Deliberately almost empty:
-- everything knowable about someone belongs to one of their identities or to
-- their data, not here.
CREATE TABLE IF NOT EXISTS accounts (
    id         BIGINT PRIMARY KEY DEFAULT nextval('accounts_id_seq'),
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
-- One row per way of proving you are that account. `kind` is 'telegram'
-- today; a web login is a second kind. The primary key is what stops one
-- Telegram id from being claimed by two accounts.
CREATE TABLE IF NOT EXISTS identities (
    account_id  BIGINT NOT NULL,
    kind        TEXT NOT NULL,
    external_id TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (kind, external_id)
);
-- Where to reach an account on a channel, when nothing more specific says
-- otherwise. Replaces `user_chats`.
CREATE TABLE IF NOT EXISTS deliveries (
    account_id BIGINT NOT NULL,
    channel    TEXT NOT NULL,
    address    TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (account_id, channel)
);
CREATE SEQUENCE IF NOT EXISTS conversations_id_seq;
-- A rolling thread. `scope` keeps a group chat's history out of the
-- account's private thread: 'direct' is the 1:1 chat and the web app, which
-- share; a group is 'telegram:<chat_id>'.
CREATE TABLE IF NOT EXISTS conversations (
    id            BIGINT PRIMARY KEY DEFAULT nextval('conversations_id_seq'),
    account_id    BIGINT NOT NULL,
    scope         TEXT NOT NULL,
    pending_draft TEXT,
    started_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at    TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE SEQUENCE IF NOT EXISTS messages_id_seq;
-- `body` is a serde_json `rig::completion::Message`. Storing the whole
-- message rather than plain text keeps tool calls and their results paired,
-- which `trim_history` depends on.
CREATE TABLE IF NOT EXISTS messages (
    id              BIGINT PRIMARY KEY DEFAULT nextval('messages_id_seq'),
    conversation_id BIGINT NOT NULL,
    position        BIGINT NOT NULL,
    body            TEXT NOT NULL,
    created_at      TIMESTAMP NOT NULL DEFAULT current_timestamp
);
"#;

/// Every Telegram id that appears anywhere in the pre-phase-one schema.
/// `user_chats` is included even though it is about to be replaced: someone
/// might appear there and nowhere else.
const LEGACY_USER_IDS: &str = "
SELECT user_id FROM purchases
UNION SELECT user_id FROM reminders
UNION SELECT user_id FROM user_facts
UNION SELECT user_id FROM request_log
UNION SELECT user_id FROM users
UNION SELECT user_id FROM user_chats
UNION SELECT user_id FROM members
UNION SELECT user_id FROM waitlist
UNION SELECT user_id FROM trips
";

/// One account per pre-existing Telegram id.
///
/// Written in Rust rather than SQL because each row needs `nextval` and the
/// id it produced, and DuckDB has no `setval` to fix a sequence up
/// afterwards.
fn step_2_backfill_accounts(conn: &Connection) -> Result<()> {
    let sql = format!("SELECT DISTINCT user_id FROM ({LEGACY_USER_IDS}) ORDER BY user_id");
    let mut stmt = conn.prepare(&sql)?;
    let ids: Vec<i64> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    for user_id in ids {
        let account_id: i64 = conn.query_row(
            "INSERT INTO accounts (id) VALUES (nextval('accounts_id_seq')) RETURNING id",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO identities (account_id, kind, external_id) VALUES (?, 'telegram', ?)",
            params![account_id, user_id.to_string()],
        )?;
    }
    Ok(())
}

/// Every table is rebuilt rather than altered in place, even where the
/// column is unconstrained and `ALTER` would have worked.
///
/// Measured: DuckDB refuses to `ALTER` and then `UPDATE` the same table
/// inside one transaction — "Attempting to modify table purchases but
/// another transaction has altered this table". Doing it in autocommit
/// works but gives up atomicity, and this migration is one-way over live
/// purchase history. A rebuild touches the new table and the old one
/// separately, so it commits as a unit.
///
/// `reminders` gains its `channel` and `address` here rather than in a later
/// step; the table is being rewritten anyway and two rebuilds would be waste.
const STEP_3_UNCONSTRAINED: &str = r#"
CREATE TABLE purchases_new (
    id BIGINT PRIMARY KEY DEFAULT nextval('purchases_id_seq'),
    account_id BIGINT NOT NULL,
    item TEXT NOT NULL,
    store TEXT NOT NULL,
    url TEXT,
    price DOUBLE,
    currency TEXT,
    notes TEXT,
    purchased_at TEXT,
    recorded_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
INSERT INTO purchases_new
    (id, account_id, item, store, url, price, currency, notes, purchased_at, recorded_at)
SELECT p.id, i.account_id, p.item, p.store, p.url, p.price, p.currency, p.notes,
       p.purchased_at, p.recorded_at
FROM purchases p
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(p.user_id AS TEXT);
DROP TABLE purchases;
ALTER TABLE purchases_new RENAME TO purchases;

CREATE TABLE reminders_new (
    id BIGINT PRIMARY KEY DEFAULT nextval('reminders_id_seq'),
    account_id BIGINT NOT NULL,
    channel TEXT NOT NULL DEFAULT 'telegram',
    address TEXT NOT NULL,
    item TEXT NOT NULL,
    interval_days BIGINT NOT NULL,
    next_due TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
INSERT INTO reminders_new
    (id, account_id, channel, address, item, interval_days, next_due, active, created_at)
SELECT r.id, i.account_id, 'telegram', CAST(r.chat_id AS TEXT), r.item,
       r.interval_days, r.next_due, r.active, r.created_at
FROM reminders r
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(r.user_id AS TEXT);
DROP TABLE reminders;
ALTER TABLE reminders_new RENAME TO reminders;

CREATE TABLE request_log_new (
    account_id BIGINT NOT NULL,
    kind TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
INSERT INTO request_log_new (account_id, kind, created_at)
SELECT i.account_id, l.kind, l.created_at
FROM request_log l
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(l.user_id AS TEXT);
DROP TABLE request_log;
ALTER TABLE request_log_new RENAME TO request_log;
"#;

/// The five tables whose `account_id` sits inside a PK or UNIQUE constraint.
/// DuckDB will not drop such a column at all, so a rebuild is the only
/// route — and it is the same route step 3 takes, for atomicity.
const STEP_4_REBUILDS: &str = r#"
-- Do this FIRST, before waitlist is rebuilt without its chat_id. Someone on
-- the waitlist may never have had a user_chats row, and the START they
-- pressed is the only permission we have to message them. Dropping the
-- column before reading it would lose that silently, and the next announce
-- would simply skip them.
INSERT INTO deliveries (account_id, channel, address)
SELECT i.account_id, 'telegram', CAST(w.chat_id AS TEXT)
FROM waitlist w
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(w.user_id AS TEXT)
ON CONFLICT (account_id, channel) DO NOTHING;

CREATE TABLE user_facts_new (
    account_id BIGINT NOT NULL,
    key        TEXT NOT NULL,
    value      TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (account_id, key)
);
INSERT INTO user_facts_new (account_id, key, value, updated_at)
SELECT i.account_id, f.key, f.value, f.updated_at FROM user_facts f
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(f.user_id AS TEXT);
DROP TABLE user_facts;
ALTER TABLE user_facts_new RENAME TO user_facts;

CREATE TABLE users_new (
    account_id   BIGINT PRIMARY KEY,
    display_name TEXT NOT NULL,
    updated_at   TIMESTAMP NOT NULL DEFAULT current_timestamp
);
INSERT INTO users_new (account_id, display_name, updated_at)
SELECT i.account_id, u.display_name, u.updated_at FROM users u
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(u.user_id AS TEXT);
DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

CREATE TABLE members_new (
    account_id BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    joined_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    revoked_at TIMESTAMP
);
INSERT INTO members_new (account_id, code, joined_at, revoked_at)
SELECT i.account_id, m.code, m.joined_at, m.revoked_at FROM members m
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(m.user_id AS TEXT);
DROP TABLE members;
ALTER TABLE members_new RENAME TO members;

CREATE TABLE waitlist_new (
    account_id BIGINT PRIMARY KEY,
    code       TEXT NOT NULL,
    seen_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    invited_at TIMESTAMP
);
INSERT INTO waitlist_new (account_id, code, seen_at, invited_at)
SELECT i.account_id, w.code, w.seen_at, w.invited_at FROM waitlist w
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(w.user_id AS TEXT);
DROP TABLE waitlist;
ALTER TABLE waitlist_new RENAME TO waitlist;

CREATE TABLE trips_new (
    id          BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    account_id  BIGINT NOT NULL,
    name        TEXT NOT NULL,
    name_key    TEXT NOT NULL,
    adults      BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT,
    status      TEXT NOT NULL DEFAULT 'planning',
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    UNIQUE (account_id, name_key)
);
INSERT INTO trips_new
    (id, account_id, name, name_key, adults, cabin_class, status, created_at, updated_at)
SELECT t.id, i.account_id, t.name, t.name_key, t.adults, t.cabin_class, t.status,
       t.created_at, t.updated_at
FROM trips t
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(t.user_id AS TEXT);
DROP TABLE trips;
ALTER TABLE trips_new RENAME TO trips;
"#;

/// `user_chats` was already "where to reach this person", which is what
/// `deliveries` is. The waitlist rescue happened at the top of step 4,
/// before that column was dropped; this is the richer record and overwrites
/// it where both exist, because user_chats is the more recent sighting.
const STEP_5_DELIVERIES: &str = r#"
-- `updated_at` is carried from the source row rather than set to now:
-- bare `current_timestamp` inside DO UPDATE SET is parsed as a column
-- reference by DuckDB, and the real last-seen time is the truer value.
INSERT INTO deliveries (account_id, channel, address, updated_at)
SELECT i.account_id, 'telegram', CAST(c.chat_id AS TEXT), c.updated_at
FROM user_chats c
JOIN identities i ON i.kind='telegram' AND i.external_id = CAST(c.user_id AS TEXT)
ON CONFLICT (account_id, channel)
DO UPDATE SET address = excluded.address, updated_at = excluded.updated_at;

DROP TABLE user_chats;
"#;

const STEP_6_LOGIN_TOKENS: &str = r#"
CREATE TABLE IF NOT EXISTS login_tokens (
    token_hash  TEXT PRIMARY KEY,
    email       TEXT NOT NULL,
    account_id  BIGINT,
    expires_at  TIMESTAMP NOT NULL,
    consumed_at TIMESTAMP
);
"#;

/// Threads in the browser: a name and a pin.
///
/// The `pinned` column is added bare, backfilled and given its default
/// here, and made NOT NULL in step 8 — a separate step because DuckDB
/// refuses `SET NOT NULL` in a transaction that has already touched the
/// table's rows, and the `ADD COLUMN` in this one counts. `apply_steps`
/// runs each step in its own transaction, so a separate step is what a
/// separate transaction costs. (`ADD COLUMN` with a constraint is refused
/// outright, which is why the constraint is not simply on the add.)
/// `IF NOT EXISTS` so a database created by `MIGRATIONS` after this
/// shipped, but somehow recorded below 7, is not broken by the step. The
/// backfill and the `SET DEFAULT` are separate statements, rather than
/// folding the default into `ADD COLUMN pinned BOOLEAN DEFAULT false`,
/// because `ADD COLUMN IF NOT EXISTS` may no-op on a file that already has
/// the column — from an interrupted prior run of this same step — and the
/// explicit backfill and default still need to run against that file too.
const STEP_7_THREADS: &str = r#"
ALTER TABLE conversations ADD COLUMN IF NOT EXISTS title TEXT;
ALTER TABLE conversations ADD COLUMN IF NOT EXISTS pinned BOOLEAN;
UPDATE conversations SET pinned = false WHERE pinned IS NULL;
ALTER TABLE conversations ALTER COLUMN pinned SET DEFAULT false;
"#;

/// See `STEP_7_THREADS`. Dying between 7 and 8 leaves a nullable column
/// that holds no nulls, and this runs alone on the next boot.
const STEP_8_PINNED_NOT_NULL: &str = r#"
ALTER TABLE conversations ALTER COLUMN pinned SET NOT NULL;
"#;

/// The chat that made a trip. Nullable: expiry (a later change) detaches
/// rather than cascades, so a trip outliving its chat — idle past
/// `Core::THREAD_IDLE_SECS` — becomes orphaned, not deleted.
///
/// `IF NOT EXISTS` for the same reason STEP_7_THREADS uses it: a database
/// created by `MIGRATIONS` after this column shipped, but recorded below 9,
/// already has the column, and a bare `ADD COLUMN` would fail on it with
/// "already exists".
const STEP_9_TRIP_CONVERSATION: &str = r#"
ALTER TABLE trips ADD COLUMN IF NOT EXISTS conversation_id BIGINT;
"#;

/// `kept` marks a trip the traveller asked to keep. It defaults false —
/// new trips are drafts — but every trip already in the database was made
/// when creating one *was* the act of keeping it, so they are all kept.
/// Without that UPDATE this step hides every trip a traveller already has.
///
/// Split from its `NOT NULL` the same way `STEP_7_THREADS` splits from
/// `STEP_8_PINNED_NOT_NULL`, and for the same reason: DuckDB refuses to
/// `ADD COLUMN` with a constraint on it, so `NOT NULL` cannot ride along
/// on the statement that creates the column.
///
/// `IF NOT EXISTS` for the same reason step 7 has it: a fixture can build
/// the finished shape from `MIGRATIONS` and then record an older version,
/// so this step can meet a column that is already there. Note the UPDATE
/// still runs in that case, which is correct — such a database has no
/// drafts in it to protect.
const STEP_10_KEPT_TRIPS: &str = r#"
ALTER TABLE trips ADD COLUMN IF NOT EXISTS kept BOOLEAN;
UPDATE trips SET kept = true WHERE kept IS NULL;
ALTER TABLE trips ALTER COLUMN kept SET DEFAULT false;
"#;

/// See `STEP_8_PINNED_NOT_NULL`. Dying between 10 and 11 leaves a nullable
/// column that holds no nulls, and this runs alone on the next boot.
const STEP_11_KEPT_TRIPS_NOT_NULL: &str = r#"
ALTER TABLE trips ALTER COLUMN kept SET NOT NULL;
"#;

/// Runs and their traces, the run id on messages, and the debug switch on
/// accounts. Same shape as steps 7/8 and 10/11, for the same DuckDB
/// reasons: `ADD COLUMN` cannot carry a constraint, and `SET NOT NULL`
/// refuses to share a transaction with the rows it just touched.
const STEP_12_RUN_TRACES: &str = r#"
CREATE SEQUENCE IF NOT EXISTS runs_id_seq;
CREATE TABLE IF NOT EXISTS runs (
    id BIGINT PRIMARY KEY DEFAULT nextval('runs_id_seq'),
    account_id BIGINT NOT NULL, conversation_id BIGINT NOT NULL,
    started_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    ended_at TIMESTAMP, outcome TEXT, detail TEXT
);
CREATE SEQUENCE IF NOT EXISTS run_traces_id_seq;
CREATE TABLE IF NOT EXISTS run_traces (
    id BIGINT PRIMARY KEY DEFAULT nextval('run_traces_id_seq'),
    run_id BIGINT NOT NULL, seq BIGINT NOT NULL, kind TEXT NOT NULL,
    tool TEXT, args TEXT, nested BOOLEAN NOT NULL DEFAULT false,
    started_at TIMESTAMP NOT NULL, duration_ms BIGINT, status TEXT,
    detail TEXT, result TEXT, truncated BOOLEAN NOT NULL DEFAULT false
);
ALTER TABLE messages ADD COLUMN IF NOT EXISTS run_id BIGINT;
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS debug BOOLEAN;
UPDATE accounts SET debug = false WHERE debug IS NULL;
ALTER TABLE accounts ALTER COLUMN debug SET DEFAULT false;
"#;

/// See `STEP_8_PINNED_NOT_NULL`. Dying between 12 and 13 leaves a nullable
/// column that holds no nulls, and this runs alone on the next boot.
const STEP_13_DEBUG_NOT_NULL: &str = r#"
ALTER TABLE accounts ALTER COLUMN debug SET NOT NULL;
"#;

/// Items replace segments: one table for flights, stays, activities and
/// transport. Flights are copied in with their candidates re-keyed to the
/// new item ids. The old tables stay until a later release drops them, so
/// a rollback of this release still has its data.
///
/// The `WHERE NOT EXISTS` guards are for the fixture pattern step 10
/// documents: a database built by `MIGRATIONS` and recorded below 14
/// already carries the tables, and copying the flights in twice would
/// double every trip.
const STEP_14_TRIP_ITEMS: &str = r#"
CREATE SEQUENCE IF NOT EXISTS trip_items_id_seq;
CREATE TABLE IF NOT EXISTS trip_items (
    id BIGINT PRIMARY KEY DEFAULT nextval('trip_items_id_seq'),
    trip_id BIGINT NOT NULL, position BIGINT NOT NULL, kind TEXT NOT NULL,
    title TEXT NOT NULL, place TEXT, origin TEXT, destination TEXT,
    starts_at TEXT, ends_at TEXT, date TEXT NOT NULL,
    booked BOOLEAN NOT NULL DEFAULT false, confirmation_code TEXT,
    price DOUBLE, currency TEXT, notes TEXT, arrival_id BIGINT,
    next_candidate BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE IF NOT EXISTS item_candidates (
    item_id BIGINT NOT NULL, candidate BIGINT NOT NULL,
    chosen BOOLEAN NOT NULL DEFAULT false, airline TEXT NOT NULL, flight_numbers TEXT NOT NULL,
    itinerary TEXT NOT NULL, departing_at_local TEXT, arriving_at_local TEXT,
    duration_minutes BIGINT, quoted_price DOUBLE, quoted_currency TEXT, quoted_at TIMESTAMP,
    source TEXT, PRIMARY KEY (item_id, candidate)
);
INSERT INTO trip_items (trip_id, position, kind, title, origin, destination, date, next_candidate)
SELECT trip_id, position, 'flight', origin || ' → ' || destination, origin, destination, departure_date, next_candidate
FROM trip_segments
WHERE NOT EXISTS (SELECT 1 FROM trip_items i WHERE i.trip_id = trip_segments.trip_id AND i.kind = 'flight');
INSERT INTO item_candidates
SELECT i.id, c.candidate, c.chosen, c.airline, c.flight_numbers, c.itinerary,
       c.departing_at_local, c.arriving_at_local, c.duration_minutes,
       c.quoted_price, c.quoted_currency, c.quoted_at, c.source
FROM segment_candidates c
JOIN trip_items i ON i.trip_id = c.trip_id AND i.position = c.position AND i.kind = 'flight'
WHERE NOT EXISTS (SELECT 1 FROM item_candidates x WHERE x.item_id = i.id AND x.candidate = c.candidate);
"#;

/// Positions were copied as they stood; the date rule puts them right.
fn step_15_reorder_items(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("SELECT DISTINCT trip_id FROM trip_items")?;
    let ids: Vec<i64> = stmt.query_map([], |r| r.get(0))?.collect::<duckdb::Result<_>>()?;
    drop(stmt);
    for id in ids {
        reorder_items(conn, id)?;
    }
    Ok(())
}

/// The booking address: a handle on the account, and the mail, files and
/// readings it brings in. `IF NOT EXISTS` throughout, for the fixture
/// pattern step 10 documents.
const STEP_16_INBOX: &str = r#"
ALTER TABLE accounts ADD COLUMN IF NOT EXISTS handle TEXT;
CREATE SEQUENCE IF NOT EXISTS inbound_mail_id_seq;
CREATE TABLE IF NOT EXISTS inbound_mail (
    id BIGINT PRIMARY KEY DEFAULT nextval('inbound_mail_id_seq'),
    account_id BIGINT NOT NULL, provider_id TEXT NOT NULL UNIQUE,
    from_address TEXT NOT NULL, subject TEXT, text TEXT, html TEXT,
    truncated BOOLEAN NOT NULL DEFAULT false,
    received_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    status TEXT NOT NULL DEFAULT 'new', attempts BIGINT NOT NULL DEFAULT 0,
    attempted_at TIMESTAMP, forwarded_at TIMESTAMP, error TEXT
);
CREATE SEQUENCE IF NOT EXISTS attachments_id_seq;
CREATE TABLE IF NOT EXISTS attachments (
    id BIGINT PRIMARY KEY DEFAULT nextval('attachments_id_seq'),
    mail_id BIGINT NOT NULL, item_id BIGINT, filename TEXT NOT NULL, mime TEXT NOT NULL,
    bytes BLOB, text TEXT
);
CREATE SEQUENCE IF NOT EXISTS arrivals_id_seq;
CREATE TABLE IF NOT EXISTS arrivals (
    id BIGINT PRIMARY KEY DEFAULT nextval('arrivals_id_seq'),
    account_id BIGINT NOT NULL, mail_id BIGINT NOT NULL, booking BOOLEAN NOT NULL,
    kind TEXT, title TEXT, place TEXT, origin TEXT, destination TEXT, date TEXT,
    starts_at TEXT, ends_at TEXT, timezone TEXT, confirmation_code TEXT,
    price DOUBLE, currency TEXT, travellers TEXT, confidence DOUBLE,
    summary TEXT NOT NULL, trip_id BIGINT, status TEXT NOT NULL DEFAULT 'pending',
    item_id BIGINT, decided_at TIMESTAMP
);
"#;

/// Which flight a confirmation named, and where it stops. Nullable and
/// added in place: every arrival written before this one is a reading that
/// never had them, and there is nothing to backfill from — the mail it came
/// out of may be gone. The order matches the tail of `arrivals` in
/// `MIGRATIONS`, which is what keeps a migrated table the same shape as a
/// fresh one.
const STEP_17_ARRIVAL_FLIGHT: &str = r#"
ALTER TABLE arrivals ADD COLUMN IF NOT EXISTS airline TEXT;
ALTER TABLE arrivals ADD COLUMN IF NOT EXISTS flight_number TEXT;
ALTER TABLE arrivals ADD COLUMN IF NOT EXISTS stops TEXT;
"#;

/// What the inbound webhook says each part of a mail is. Resend sends
/// `content_disposition` and `content_id` per part on `email.received` and
/// documents neither on the record the worker fetches later, so this is
/// where the decoration rule gets data it has actually seen arrive. Mail
/// already on file gains no rows: nothing kept the fields at the time, and
/// there is nowhere to read them back from.
///
/// This DDL will not create the table on any database that has been opened
/// since, and is not meant to. `Store::open` runs `MIGRATIONS` before it
/// applies a step, and `MIGRATIONS` carries this table as CREATE TABLE IF
/// NOT EXISTS, so it is already there by the time the step runs — which is
/// what makes a new table safe to add at all. The step is here for the
/// version bump, and for what the bump does: a pending step is what makes
/// the migration runner back the database up before touching it, and the
/// recorded version is how a pod says which shape it is running. Steps 6
/// and 16 are inert in exactly this way and are kept for the same reason.
///
/// It still has to be right. The column list and its order match
/// `mail_parts` in `MIGRATIONS`, because a step that ever did run — on a
/// database restored from before this table, say — must build the table
/// the rest of the code expects. Nothing but
/// `the_step_that_adds_mail_parts_builds_the_table_a_fresh_database_has`
/// keeps the two in step, and it has to run the step by itself to do it.
const STEP_18_MAIL_PARTS: &str = r#"
CREATE SEQUENCE IF NOT EXISTS mail_parts_id_seq;
CREATE TABLE IF NOT EXISTS mail_parts (
    id BIGINT PRIMARY KEY DEFAULT nextval('mail_parts_id_seq'),
    mail_id BIGINT NOT NULL, provider_id TEXT NOT NULL,
    content_disposition TEXT, content_id TEXT
);
"#;

fn steps() -> Vec<(i64, Step)> {
    vec![
        (1, Step::Sql(STEP_1_NEW_TABLES)),
        (2, Step::Code(step_2_backfill_accounts)),
        (3, Step::Sql(STEP_3_UNCONSTRAINED)),
        (4, Step::Sql(STEP_4_REBUILDS)),
        (5, Step::Sql(STEP_5_DELIVERIES)),
        (6, Step::Sql(STEP_6_LOGIN_TOKENS)),
        (7, Step::Sql(STEP_7_THREADS)),
        (8, Step::Sql(STEP_8_PINNED_NOT_NULL)),
        (9, Step::Sql(STEP_9_TRIP_CONVERSATION)),
        (10, Step::Sql(STEP_10_KEPT_TRIPS)),
        (11, Step::Sql(STEP_11_KEPT_TRIPS_NOT_NULL)),
        (12, Step::Sql(STEP_12_RUN_TRACES)),
        (13, Step::Sql(STEP_13_DEBUG_NOT_NULL)),
        (14, Step::Sql(STEP_14_TRIP_ITEMS)),
        (15, Step::Code(step_15_reorder_items)),
        (16, Step::Sql(STEP_16_INBOX)),
        (17, Step::Sql(STEP_17_ARRIVAL_FLIGHT)),
        (18, Step::Sql(STEP_18_MAIL_PARTS)),
    ]
}

/// True when this database predates phase one. `purchases.user_id` is the
/// marker: `MIGRATIONS` has created that table with `account_id` since, so
/// the old column name can only survive on a file that already existed.
fn legacy_shape(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM information_schema.columns
         WHERE table_name = 'purchases' AND column_name = 'user_id'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

fn apply_steps(conn: &Connection, db_path: &Path) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL)")?;
    let mut stmt = conn.prepare("SELECT version FROM schema_version")?;
    let current: Option<i64> = stmt.query_map([], |r| r.get(0))?.next().transpose()?;
    drop(stmt);
    let mut current = match current {
        Some(v) => v,
        None => {
            // No recorded version yet, which is true of two very different
            // databases: one created before phase one, and one created just
            // now by `MIGRATIONS` in the finished shape. Telling them apart
            // matters — the upgrade steps read `purchases.user_id`, which
            // only the older one has, and would fail on a fresh file.
            let start = if legacy_shape(conn)? {
                0
            } else {
                steps().last().map(|(n, _)| *n).unwrap_or(0)
            };
            conn.execute("INSERT INTO schema_version (version) VALUES (?)", params![start])?;
            start
        }
    };
    // Before the first step, not after: a migration cannot be undone, so this
    // is the last moment the old shape still exists.
    //
    // A failure here is logged and the migration proceeds anyway. That is
    // deliberate and it is sharp — it means an irreversible change can run
    // unprotected. The alternative, refusing to start, turns a full disk into
    // a bot that will not boot, at the worst possible moment. Reversing this
    // choice is one `?`; see the design doc.
    let target = steps().last().map(|(n, _)| *n).unwrap_or(0);
    if target > current {
        let dir = crate::backup::dir_for(db_path);
        let taken = std::fs::create_dir_all(&dir)
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                let to = dir.join(crate::backup::file_name_now(
                    crate::backup::Reason::Migration { to: target },
                ));
                backup_connection(conn, &to).map(|()| to)
            });
        match taken {
            Ok(to) => tracing::info!(path = %to.display(), from = current, to = target,
                "backed up before migrating"),
            Err(e) => tracing::error!(error = %e, from = current, to = target,
                "COULD NOT BACK UP BEFORE MIGRATING; proceeding anyway"),
        }
    }

    for (n, step) in steps() {
        if n <= current {
            continue;
        }
        // DDL is transactional in DuckDB v1.5.1 (measured), so a step that
        // fails half-way leaves the database exactly as it was.
        conn.execute_batch("BEGIN")?;
        let result = match step {
            Step::Sql(sql) => conn.execute_batch(sql).map_err(anyhow::Error::from),
            Step::Code(f) => f(conn),
        };
        if let Err(e) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e.context(format!("migration step {n} failed")));
        }
        conn.execute("UPDATE schema_version SET version = ?", params![n])?;
        conn.execute_batch("COMMIT")?;
        current = n;
        tracing::info!(step = n, "applied migration step");
    }
    Ok(())
}

/// One thing still waiting to go out.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingMirror {
    pub id: i64,
    pub account_id: i64,
    pub address: String,
    pub body: String,
}

/// One row of the sidebar. `current` is not here: the store does not know
/// which thread a channel would continue, `session::latest_direct` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub id: i64,
    pub title: Option<String>,
    pub pinned: bool,
    /// RFC 3339, UTC.
    pub updated_at: String,
}

/// A mail the extractor should read next, with enough of its state to
/// decide whether this is a retry and whether the owner already has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailToWork {
    pub id: i64,
    pub account_id: i64,
    pub provider_id: String,
    pub from: String,
    pub subject: Option<String>,
    pub text: Option<String>,
    pub html: Option<String>,
    pub attempts: i64,
    pub forwarded: bool,
}

/// What a mail said one of its parts is, as the inbound webhook described
/// it: the provider's id for the part, and the two header fields that
/// separate a ticket from a signature logo. `provider_id` is what ties it
/// to a row of the attachment listing, which stays the authority on what
/// exists and what it is called.
///
/// `content_disposition` is the whole header value, parameters and all
/// (`inline; filename="logo.png"`), because that is what arrived; reading
/// the token out of it is the worker's business.
///
/// `Debug` is here for the tests, which compare these, and it is safe
/// only because of what is missing: no body, no filename. A Content-ID is
/// still the sender's own text, so this goes in an `assert_eq!` and never
/// in a log line — the worker logs counts of these, never one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailPart {
    pub provider_id: String,
    pub content_disposition: Option<String>,
    pub content_id: Option<String>,
}

/// The extractor's reading of one mail: everything `arrivals` takes from
/// the caller. The rest of the row — owner, status, the decision — is the
/// store's.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct NewArrival {
    pub booking: bool,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub place: Option<String>,
    pub origin: Option<String>,
    pub destination: Option<String>,
    /// A flight's own name and number, when the confirmation said them,
    /// and the airports it changes planes at, comma-separated the way
    /// `travellers` is.
    pub airline: Option<String>,
    pub flight_number: Option<String>,
    pub stops: Option<String>,
    pub date: Option<String>,
    pub starts_at: Option<String>,
    pub ends_at: Option<String>,
    pub timezone: Option<String>,
    pub confirmation_code: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub travellers: Option<String>,
    pub confidence: Option<f64>,
    pub summary: String,
    pub trip_id: Option<i64>,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

/// Writes a consistent copy of `conn`'s database to `path`.
///
/// DuckDB is single-writer, so this is the only way to get a copy that is not
/// merely crash-consistent: it runs on the connection that already holds the
/// database open, folding in whatever is still sitting in the write-ahead log.
/// A copy taken from outside — `cp`, a volume snapshot, a provider's block
/// backup — captures whatever was on disk mid-flight and relies on WAL replay,
/// exactly like recovering from a power cut.
///
/// Written to a `.partial` and renamed, so an interrupted backup leaves
/// something obviously unfinished rather than something that looks restorable.
fn backup_connection(conn: &Connection, path: &Path) -> Result<()> {
    // The source's identifier is derived from its filename — `scout` in
    // production, a random temp name under test — so it is asked for rather
    // than assumed. Hardcoding it would pass no test and quietly couple
    // production to a filename.
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

/// Whether an account holds anything its owner would miss.
///
/// Bookkeeping is deliberately not counted. A seat, a waitlist place, a
/// display name, a delivery address and a pending login token are all things
/// Scout wrote *about* someone rather than things they made, and an account
/// can hold every one of them without its owner having asked Scout a single
/// question. Counting them would make the ordinary case permanently
/// unmergeable: signing in by email mints a fresh account, which immediately
/// takes a seat, and that seat would then be the proof it could never be
/// merged away.
fn is_empty_account(conn: &Connection, account_id: i64) -> Result<bool> {
    // Six `?` rather than a repeated `?1`: DuckDB's own placeholder styles
    // are `?` and `$n`, and mixing them is how this reads wrong later.
    let held: i64 = conn.query_row(
        "SELECT (SELECT count(*) FROM purchases     WHERE account_id = ?)
              + (SELECT count(*) FROM reminders     WHERE account_id = ?)
              + (SELECT count(*) FROM user_facts    WHERE account_id = ?)
              + (SELECT count(*) FROM request_log   WHERE account_id = ?)
              + (SELECT count(*) FROM trips         WHERE account_id = ?)
              + (SELECT count(*) FROM conversations WHERE account_id = ?)",
        params![account_id, account_id, account_id, account_id, account_id, account_id],
        |r| r.get(0),
    )?;
    Ok(held == 0)
}

/// Moves everything `absorbed` can prove or be reached by onto `survivor`,
/// then deletes it.
///
/// This is the only operation in the store that deletes an account, so it
/// re-checks the precondition its caller has already checked. What that
/// second check buys is narrow and worth being exact about: it catches a
/// caller that picked the direction backwards — `link_identity` chooses
/// which of the two survives across three branches — and it would not catch
/// `is_empty_account` being wrong, since it asks the same question of the
/// same tables. The predicate is the thing to change carefully.
fn merge_accounts(conn: &Connection, absorbed: i64, survivor: i64) -> Result<()> {
    if absorbed == survivor {
        return Ok(());
    }
    if !is_empty_account(conn, absorbed)? {
        anyhow::bail!("refusing to merge account {absorbed} away: it holds content");
    }

    conn.execute_batch("BEGIN")?;
    match move_rows(conn, absorbed, survivor) {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// The body of a merge, split out so the caller can roll back one call.
fn move_rows(conn: &Connection, absorbed: i64, survivor: i64) -> Result<()> {
    // `deliveries` is keyed per channel, so only channels the survivor has
    // no address for can move. Where both have one the survivor's is kept:
    // it belongs to the account with the history, so it is the address its
    // owner is actually reachable at.
    conn.execute(
        "DELETE FROM deliveries WHERE account_id = ?
         AND channel IN (SELECT channel FROM deliveries WHERE account_id = ?)",
        params![absorbed, survivor],
    )?;

    // One row per account each, so a row can only move where the survivor
    // has none of its own. Dropping the absorbed `members` row when the
    // survivor is already inside hands its seat back to the round, because
    // `rounds` counts member rows — and unlike a revoke, which keeps the row
    // so moderation cannot quietly reopen a round, that seat was never a
    // person.
    for table in ["users", "members", "waitlist"] {
        conn.execute(
            &format!(
                "DELETE FROM {table} WHERE account_id = ?
                 AND EXISTS (SELECT 1 FROM {table} kept WHERE kept.account_id = ?)"
            ),
            params![absorbed, survivor],
        )?;
    }

    for table in ["deliveries", "users", "members", "waitlist", "login_tokens", "identities"] {
        conn.execute(
            &format!("UPDATE {table} SET account_id = ? WHERE account_id = ?"),
            params![survivor, absorbed],
        )?;
    }

    // Somebody inside must not also be queued, or the next announce chases a
    // member. `claim_seat` keeps the same invariant when it admits.
    conn.execute(
        "DELETE FROM waitlist WHERE account_id = ?
         AND EXISTS (SELECT 1 FROM members m WHERE m.account_id = ? AND m.revoked_at IS NULL)",
        params![survivor, survivor],
    )?;

    conn.execute("DELETE FROM accounts WHERE id = ?", params![absorbed])?;
    Ok(())
}

impl Store {
    /// The connection, whatever happened to the last thread that held it.
    ///
    /// Every store call comes through here rather than unwrapping the lock
    /// itself. A poisoned mutex means some earlier holder panicked; the
    /// value it guards is a DuckDB connection that is whole whenever the
    /// guard is not held, so there is nothing to protect anyone from — and
    /// an unwrap would turn one panic into every later call panicking for
    /// the life of the process, while `/healthz` kept saying ok.
    ///
    /// The one thing a panic can leave behind is an open transaction, which
    /// DuckDB will not start another inside. It is rolled back on recovery;
    /// when there is none the `ROLLBACK` errors, and that error is the
    /// uninteresting one.
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let guard = poisoned.into_inner();
                self.conn.clear_poison();
                tracing::error!("the store's lock was poisoned by a panic; recovering");
                let _ = guard.execute_batch("ROLLBACK");
                guard
            }
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())?;
        conn.execute_batch(MIGRATIONS)?;
        apply_steps(&conn, path.as_ref())?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// A consistent copy of this database, taken without stopping anything.
    ///
    /// Holds the store's mutex for the duration, which blocks the agent. At
    /// this database's size that is imperceptible; it is worth knowing because
    /// it scales with the file.
    pub(crate) fn backup_to(&self, path: &Path) -> Result<()> {
        let conn = self.conn();
        backup_connection(&conn, path)
    }

    /// Highest migration step applied to this database.
    pub fn schema_version(&self) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row("SELECT version FROM schema_version", [], |r| r.get(0))?)
    }

    pub fn record_purchase(&self, account_id: i64, p: NewPurchase) -> Result<Purchase> {
        let conn = self.conn();
        let id: i64 = conn.query_row(
            "INSERT INTO purchases (account_id, item, store, url, price, currency, notes, purchased_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
            params![account_id, p.item, p.store, p.url, p.price, p.currency, p.notes, p.purchased_at],
            |row| row.get(0),
        )?;
        Ok(Purchase {
            id,
            item: p.item,
            store: p.store,
            url: p.url,
            price: p.price,
            currency: p.currency,
            notes: p.notes,
            purchased_at: p.purchased_at,
        })
    }

    /// Case-insensitive substring match on item/store/notes, newest first.
    pub fn query_purchases(
        &self,
        account_id: i64,
        term: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Purchase>> {
        const SELECT: &str =
            "SELECT id, item, store, url, price, currency, notes, purchased_at FROM purchases";
        const ORDER: &str = "ORDER BY coalesce(purchased_at, '') DESC, id DESC LIMIT ?";
        let conn = self.conn();
        let mut out = Vec::new();
        match term {
            Some(t) => {
                let like = format!("%{}%", t.to_lowercase());
                let sql = format!(
                    "{SELECT} WHERE account_id = ? AND (lower(item) LIKE ? \
                     OR lower(store) LIKE ? OR lower(coalesce(notes, '')) LIKE ?) {ORDER}"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows =
                    stmt.query_map(params![account_id, like, like, like, limit as i64], row_to_purchase)?;
                for row in rows {
                    out.push(row?);
                }
            }
            None => {
                let sql = format!("{SELECT} WHERE account_id = ? {ORDER}");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(params![account_id, limit as i64], row_to_purchase)?;
                for row in rows {
                    out.push(row?);
                }
            }
        }
        Ok(out)
    }

    /// `address` is where this reminder should be delivered — the chat it
    /// was created in, not wherever the account was last seen. A reminder
    /// set in a group belongs to that group.
    pub fn create_reminder(
        &self,
        account_id: i64,
        channel: &str,
        address: &str,
        item: &str,
        interval_days: i64,
        next_due: &str,
    ) -> Result<Reminder> {
        let conn = self.conn();
        let id: i64 = conn.query_row(
            "INSERT INTO reminders (account_id, channel, address, item, interval_days, next_due)
             VALUES (?, ?, ?, ?, ?, ?) RETURNING id",
            params![account_id, channel, address, item, interval_days, next_due],
            |row| row.get(0),
        )?;
        Ok(Reminder {
            id,
            account_id,
            channel: channel.to_string(),
            address: address.to_string(),
            item: item.to_string(),
            interval_days,
            next_due: next_due.to_string(),
        })
    }

    /// Active reminders for one user, soonest first.
    pub fn list_reminders(&self, account_id: i64) -> Result<Vec<Reminder>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, channel, address, item, interval_days, next_due FROM reminders
             WHERE account_id = ? AND active ORDER BY next_due ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![account_id], row_to_reminder)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Returns true if an active reminder belonging to this user was cancelled.
    pub fn cancel_reminder(&self, account_id: i64, id: i64) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE reminders SET active = false WHERE id = ? AND account_id = ? AND active",
            params![id, account_id],
        )?;
        Ok(n > 0)
    }

    /// All users' active reminders with next_due <= today (ISO date string).
    pub fn due_reminders(&self, today: &str) -> Result<Vec<Reminder>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, channel, address, item, interval_days, next_due FROM reminders
             WHERE active AND next_due <= ? ORDER BY next_due ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![today], row_to_reminder)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Internal: id must come from a trusted source (the scheduler) — no owner check.
    pub fn set_next_due(&self, id: i64, next_due: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE reminders SET next_due = ? WHERE id = ?",
            params![next_due, id],
        )?;
        Ok(())
    }

    /// Insert or overwrite one user-profile fact.
    pub fn upsert_fact(&self, account_id: i64, key: &str, value: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO user_facts (account_id, key, value) VALUES (?, ?, ?)
             ON CONFLICT (account_id, key)
             DO UPDATE SET value = excluded.value, updated_at = now()",
            params![account_id, key, value],
        )?;
        Ok(())
    }

    /// One user's profile facts as (key, value), sorted by key.
    pub fn list_facts(&self, account_id: i64) -> Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT key, value FROM user_facts WHERE account_id = ? ORDER BY key ASC")?;
        let rows = stmt.query_map(params![account_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Returns true if the fact existed and was removed.
    pub fn forget_fact(&self, account_id: i64, key: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "DELETE FROM user_facts WHERE account_id = ? AND key = ?",
            params![account_id, key],
        )?;
        Ok(n > 0)
    }

    /// Record one handled request for usage statistics.
    /// `request_log.kind` for one billable Duffel search. Kept here beside
    /// the table it is written into, because `/stat` reads it back by name
    /// and a typo on either side would silently report zero.
    pub const FLIGHT_SEARCH: &'static str = "flight_search";

    pub fn log_request(&self, account_id: i64, kind: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO request_log (account_id, kind) VALUES (?, ?)",
            params![account_id, kind],
        )?;
        Ok(())
    }

    /// Remember what to call a user id in `/stat`. Deliberately separate
    /// from `log_request`: commands should teach the bot your name without
    /// also counting as requests. A blank name is not a name.
    pub fn remember_user(&self, account_id: i64, display_name: &str) -> Result<()> {
        let name = display_name.trim();
        if name.is_empty() {
            return Ok(());
        }
        let conn = self.conn();
        conn.execute(
            "INSERT INTO users (account_id, display_name) VALUES (?, ?)
             ON CONFLICT (account_id)
             DO UPDATE SET display_name = excluded.display_name, updated_at = now()",
            params![account_id, name],
        )?;
        Ok(())
    }

    #[cfg(test)]
    fn log_request_at(&self, account_id: i64, kind: &str, at: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO request_log (account_id, kind, created_at) VALUES (?, ?, CAST(? AS TIMESTAMP))",
            params![account_id, kind, at],
        )?;
        Ok(())
    }

    /// Per-day request counts scoped to a single user, as
    /// (account_id, day "YYYY-MM-DD", count) at or after `cutoff`
    /// ("YYYY-MM-DD 00:00:00"). This is what non-admin `/stat` callers get,
    /// so they only ever see their own volume however many users share the
    /// bot.
    pub fn usage_stats_for(&self, cutoff: &str, account_id: i64) -> Result<Vec<(i64, String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT account_id, strftime(created_at, '%Y-%m-%d') AS day, count(*)
             FROM request_log WHERE account_id = ? AND created_at >= CAST(? AS TIMESTAMP)
             GROUP BY account_id, day ORDER BY day ASC, account_id ASC",
        )?;
        let rows = stmt.query_map(params![account_id, cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// The same shape across every user. Only reachable from `/stat` when
    /// the caller is in `Config::admin_user_ids` — the callers of this
    /// method are the whole access-control surface for cross-user data.
    pub fn usage_stats_all(&self, cutoff: &str) -> Result<Vec<(i64, String, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT account_id, strftime(created_at, '%Y-%m-%d') AS day, count(*)
             FROM request_log WHERE created_at >= CAST(? AS TIMESTAMP)
             GROUP BY account_id, day ORDER BY day ASC, account_id ASC",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// The most recent conversation for this scope, and whether it has gone
    /// quiet for longer than `ttl_secs`. Creates nothing — whether an
    /// aged-out thread is resumed or replaced is a judgement the store has
    /// no business making.
    pub fn latest_conversation(
        &self,
        account_id: i64,
        scope: &str,
        ttl_secs: i64,
    ) -> Result<Option<(i64, bool)>> {
        let conn = self.conn();
        // The cast is load-bearing: `current_timestamp` is TIMESTAMPTZ and
        // DuckDB has no `TIMESTAMPTZ - INTERVAL` overload.
        // `id DESC` breaks a tie the same way `threads_of` orders its list,
        // so the two reads cannot disagree about which thread is current.
        let mut stmt = conn.prepare(
            "SELECT id,
                    updated_at <= CAST(current_timestamp AS TIMESTAMP) - to_seconds(?)
             FROM conversations WHERE account_id = ? AND scope = ?
             ORDER BY updated_at DESC, id DESC LIMIT 1",
        )?;
        let row: Option<(i64, bool)> = stmt
            .query_map(params![ttl_secs, account_id, scope], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .next()
            .transpose()?;
        Ok(row)
    }

    pub fn start_conversation(&self, account_id: i64, scope: &str) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row(
            "INSERT INTO conversations (id, account_id, scope)
             VALUES (nextval('conversations_id_seq'), ?, ?) RETURNING id",
            params![account_id, scope],
            |r| r.get(0),
        )?)
    }

    /// Marks a conversation as spoken in, so its TTL runs from now. `false`
    /// when there was no such row to bump — the thread expired and was
    /// deleted between the continuation check and this call.
    pub fn touch_conversation(&self, conversation_id: i64) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE conversations SET updated_at = now() WHERE id = ?",
            params![conversation_id],
        )?;
        Ok(n > 0)
    }

    /// The account's `direct` threads, pinned first, then newest use first.
    pub fn threads_of(&self, account_id: i64) -> Result<Vec<ThreadRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, title, pinned, strftime(updated_at, '%Y-%m-%dT%H:%M:%SZ')
             FROM conversations WHERE account_id = ? AND scope = 'direct'
             ORDER BY pinned DESC, updated_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![account_id], |r| {
            Ok(ThreadRow { id: r.get(0)?, title: r.get(1)?, pinned: r.get(2)?, updated_at: r.get(3)? })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Bumps `updated_at`, which is what makes a thread current. `false`
    /// when the account does not own a `direct` thread by that id — which is
    /// also what a thread that no longer exists looks like, on purpose.
    pub fn open_conversation(&self, account_id: i64, conversation_id: i64) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE conversations SET updated_at = now()
             WHERE id = ? AND account_id = ? AND scope = 'direct'",
            params![conversation_id, account_id],
        )?;
        Ok(n > 0)
    }

    /// The ownership check on its own, for a caller that must not bump the
    /// thread the way `open_conversation` does.
    pub fn owns_thread(&self, account_id: i64, conversation_id: i64) -> Result<bool> {
        let conn = self.conn();
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM conversations WHERE id = ? AND account_id = ? AND scope = 'direct'",
            params![conversation_id, account_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// `None` means "no title yet" as much as "not yours or gone" — this is
    /// a read accessor, not something to base a not-found decision on.
    pub fn thread_title(&self, account_id: i64, conversation_id: i64) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT title FROM conversations WHERE id = ? AND account_id = ? AND scope = 'direct'",
        )?;
        let row: Option<Option<String>> = stmt
            .query_map(params![conversation_id, account_id], |r| r.get(0))?
            .next()
            .transpose()?;
        Ok(row.flatten())
    }

    /// A rename. Owner-checked.
    pub fn set_thread_title(&self, account_id: i64, conversation_id: i64, title: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE conversations SET title = ? WHERE id = ? AND account_id = ? AND scope = 'direct'",
            params![title, conversation_id, account_id],
        )?;
        Ok(n > 0)
    }

    /// The automatic title after a first answer. Not owner-checked: the
    /// caller is `run_agent`, which already holds the conversation. Writes
    /// only over null, so a rename is never undone.
    pub fn set_thread_title_if_missing(&self, conversation_id: i64, title: &str) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE conversations SET title = ? WHERE id = ? AND title IS NULL",
            params![title, conversation_id],
        )?;
        Ok(n > 0)
    }

    /// `direct` only: a group thread can never be pinned, from anywhere — a
    /// pinned thread is exempt from expiry forever, and a group thread has no
    /// sidebar row to unpin it from.
    pub fn set_thread_pinned(&self, account_id: i64, conversation_id: i64, pinned: bool) -> Result<bool> {
        let conn = self.conn();
        let n = conn.execute(
            "UPDATE conversations SET pinned = ? WHERE id = ? AND account_id = ? AND scope = 'direct'",
            params![pinned, conversation_id, account_id],
        )?;
        Ok(n > 0)
    }

    /// The thread and its messages, in one transaction. Owner-checked. Rows
    /// already queued in `outbox` for this thread are deliberately not swept
    /// — the outbox has no conversation id, only turn keys the transcript
    /// can mint — so a mirror message enqueued before the delete may still
    /// arrive.
    pub fn delete_conversation(&self, account_id: i64, conversation_id: i64) -> Result<bool> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<bool> {
            let owned = conn.execute(
                "DELETE FROM conversations WHERE id = ? AND account_id = ? AND scope = 'direct'",
                params![conversation_id, account_id],
            )?;
            if owned == 0 {
                return Ok(false);
            }
            conn.execute("DELETE FROM messages WHERE conversation_id = ?", params![conversation_id])?;
            // The trips this thread owns go with it. Inside this same
            // transaction: a cascade that can half-happen would leave a trip
            // pointing at a conversation that no longer exists.
            //
            // Only here. `expire_conversations` detaches instead — see
            // `detach_trips_within`. That difference is the feature.
            conn.execute(
                "DELETE FROM item_candidates WHERE item_id IN
                     (SELECT id FROM trip_items WHERE trip_id IN
                         (SELECT id FROM trips WHERE conversation_id = ?))",
                params![conversation_id],
            )?;
            // A booking Added from the inbox can land on a thread's own
            // trip, tickets and all, so this cascade has to decide about
            // those files exactly as removing the item by hand would —
            // see `release_attachments_within`.
            let mut stmt = conn.prepare(
                "SELECT i.id FROM trip_items i JOIN trips t ON t.id = i.trip_id
                 WHERE t.conversation_id = ?",
            )?;
            let items: Vec<i64> =
                stmt.query_map(params![conversation_id], |r| r.get(0))?.collect::<duckdb::Result<_>>()?;
            drop(stmt);
            release_attachments_within(&conn, &items)?;
            conn.execute(
                "DELETE FROM trip_items WHERE trip_id IN
                     (SELECT id FROM trips WHERE conversation_id = ?)",
                params![conversation_id],
            )?;
            conn.execute(
                "DELETE FROM trips WHERE conversation_id = ?",
                params![conversation_id],
            )?;
            Ok(true)
        })();
        match result {
            Ok(deleted) => {
                conn.execute_batch("COMMIT")?;
                Ok(deleted)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Deletes every unpinned conversation, any scope, idle for longer than
    /// `older_than_secs`, with its messages. Returns how many conversations
    /// went.
    ///
    /// `except` names conversations that must survive whatever their age —
    /// the ids with a run in flight. `updated_at` moves when a run *writes*
    /// its answer, so a thread already at 47h idle when a question is asked
    /// crosses the threshold while the model is still thinking; without this
    /// the hourly sweep deletes the conversation out from under a run that
    /// then writes its answer into nothing, and the reader watches the
    /// conversation they are waiting on disappear.
    ///
    /// Also sweeps messages whose conversation is already gone. A run that
    /// ends after its thread was deleted still writes its messages —
    /// `append_messages` inserts regardless — and nothing else would ever
    /// collect them. As with `delete_conversation`, rows already queued in
    /// `outbox` for an expired thread are not swept — the outbox has no
    /// conversation id — so a mirror message enqueued before expiry may
    /// still arrive.
    ///
    /// A pinned thread is skipped here by design and so is bounded by
    /// nothing this does. `trim_message_logs` is what keeps its log from
    /// growing forever: age is the wrong measure for a thread whose whole
    /// point is that it does not age out, so it is capped by row count
    /// instead.
    pub fn expire_conversations(&self, older_than_secs: i64, except: &[i64]) -> Result<usize> {
        // DuckDB's bindings have no array parameter, so `IN` gets one `?`
        // per id and the ids ride in the parameter list next to
        // `older_than_secs` — the SQL is formatted, the values never are.
        // An empty `except` omits the clause outright: `id NOT IN ()` is a
        // syntax error, not an empty set.
        let not_running = if except.is_empty() {
            String::new()
        } else {
            format!(" AND id NOT IN ({})", ["?"].repeat(except.len()).join(", "))
        };
        let mut args = Vec::with_capacity(except.len() + 1);
        args.push(older_than_secs);
        args.extend_from_slice(except);
        // Written once and pasted into all three statements, because they
        // have to agree exactly. The SELECT below must name the rows the
        // DELETE removes — no more, no less — or trips get released whose
        // thread survives, or a thread dies leaving a trip pointed at an id
        // that is gone. Two copies of a predicate drift; one cannot.
        let doomed = format!(
            "WHERE NOT pinned
               AND updated_at < CAST(current_timestamp AS TIMESTAMP) - to_seconds(?){not_running}"
        );

        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<usize> {
            // This DELETE targets the same rows the orphan sweep below would
            // eventually catch, on purpose: it keeps expiry correct without
            // depending on the sweep's predicate staying "any conversation
            // gone". If that predicate is ever narrowed — say to orphans
            // older than an hour, so a live run's save can't race it — this
            // explicit delete is what stops expired threads' messages from
            // leaking through instead.
            conn.execute(
                &format!(
                    "DELETE FROM messages WHERE conversation_id IN (
                         SELECT id FROM conversations {doomed})"
                ),
                duckdb::params_from_iter(args.iter()),
            )?;
            // Which threads are about to go, read before the DELETE because
            // afterwards there is nothing left to join against.
            let mut stmt = conn.prepare(&format!("SELECT id FROM conversations {doomed}"))?;
            let doomed_ids: Vec<i64> = stmt
                .query_map(duckdb::params_from_iter(args.iter()), |row| row.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            // Released, not deleted. `delete_conversation` cascades because
            // somebody pressed Delete; this is a timer, and a timer must not
            // destroy a plan the traveller is still building.
            //
            // `doomed_ids` is empty on every sweep that expires nothing,
            // which is the normal hourly case — `detach_trips_within` guards
            // that, since `IN ()` is a parser error rather than an empty set.
            //
            // A draft is not a plan: nobody kept it, and the thread it was
            // built in is gone. This is the one place a timer may remove a
            // trip, and it is why the detach below still exists — what the
            // traveller kept is released, not destroyed.
            delete_drafts_within(&conn, &doomed_ids)?;
            detach_trips_within(&conn, &doomed_ids)?;
            let gone = conn.execute(
                &format!("DELETE FROM conversations {doomed}"),
                duckdb::params_from_iter(args.iter()),
            )?;
            conn.execute(
                "DELETE FROM messages
                 WHERE conversation_id NOT IN (SELECT id FROM conversations)",
                [],
            )?;
            Ok(gone)
        })();
        match result {
            Ok(gone) => {
                conn.execute_batch("COMMIT")?;
                Ok(gone)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Drops every conversation's rows beyond its newest `keep`. Returns
    /// how many rows went.
    ///
    /// The `messages` table is a full log now — a run appends what it added
    /// and nothing trims it back — and `expire_conversations` spares pinned
    /// threads, which leaves one thread a person keeps forever as a table
    /// that only ever grows. This is the ceiling on it: a cap by row count
    /// rather than by age, because a pinned thread's whole point is that it
    /// does not age out.
    ///
    /// Per conversation, not overall: one busy thread must not evict a
    /// quiet one's history. The cut is by `position`, the same order a read
    /// comes back in, so what survives is the newest end of each log.
    pub fn trim_message_logs(&self, keep: usize) -> Result<usize> {
        let conn = self.conn();
        Ok(conn.execute(
            "DELETE FROM messages WHERE id IN (
                 SELECT id FROM (
                     SELECT id, row_number() OVER (
                         PARTITION BY conversation_id ORDER BY position DESC
                     ) AS rn
                     FROM messages
                 ) WHERE rn > ?
             )",
            params![keep as i64],
        )?)
    }

    /// Opens a run: one row that the run's trace and its messages hang
    /// off. Returns the new id.
    pub fn open_run(&self, account_id: i64, conversation_id: i64) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row(
            "INSERT INTO runs (account_id, conversation_id) VALUES (?, ?) RETURNING id",
            params![account_id, conversation_id],
            |r| r.get(0),
        )?)
    }

    pub fn close_run(&self, run_id: i64, outcome: &str, detail: Option<&str>) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE runs SET ended_at = now(), outcome = ?, detail = ? WHERE id = ?",
            params![outcome, detail, run_id],
        )?;
        Ok(())
    }

    /// Writes a run's rows in one transaction, results cut at the cap.
    pub fn append_traces(&self, run_id: i64, rows: &[scout_api::TraceRow]) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            for row in rows {
                let (result, truncated) = match &row.result {
                    Some(text) if text.len() > TRACE_RESULT_CAP => {
                        let mut cut = TRACE_RESULT_CAP;
                        while !text.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        (Some(&text[..cut]), true)
                    }
                    Some(text) => (Some(text.as_str()), row.truncated),
                    None => (None, false),
                };
                conn.execute(
                    "INSERT INTO run_traces (run_id, seq, kind, tool, args, nested, started_at, duration_ms, status, detail, result, truncated)
                     VALUES (?, ?, ?, ?, ?, ?, ?::TIMESTAMP, ?, ?, ?, ?, ?)",
                    params![
                        run_id, row.seq, row.kind, row.tool,
                        row.args.as_ref().map(|a| a.to_string()),
                        row.nested, row.started_at, row.duration_ms, row.status, row.detail,
                        result, truncated,
                    ],
                )?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// The run and its rows, or `None` when it is not this account's or
    /// no longer kept. Ownership is in the query so there is no way to read
    /// a trace without asking whose it is.
    pub fn trace_of(&self, run_id: i64, account_id: i64) -> Result<Option<(scout_api::RunRow, Vec<scout_api::TraceRow>)>> {
        let conn = self.conn();
        let run = conn
            .query_row(
                "SELECT id, started_at::TEXT, ended_at::TEXT, outcome, detail FROM runs WHERE id = ? AND account_id = ?",
                params![run_id, account_id],
                |r| Ok(scout_api::RunRow {
                    id: r.get(0)?, started_at: r.get(1)?, ended_at: r.get(2)?, outcome: r.get(3)?, detail: r.get(4)?,
                }),
            )
            .optional()?;
        let Some(run) = run else { return Ok(None) };
        let mut stmt = conn.prepare(
            "SELECT seq, kind, tool, args, nested, started_at::TEXT, duration_ms, status, detail, result, truncated
             FROM run_traces WHERE run_id = ? ORDER BY seq ASC",
        )?;
        let rows = stmt
            .query_map(params![run_id], |r| {
                let args: Option<String> = r.get(3)?;
                Ok(scout_api::TraceRow {
                    seq: r.get(0)?, kind: r.get(1)?, tool: r.get(2)?,
                    args: args.and_then(|a| serde_json::from_str(&a).ok()),
                    nested: r.get(4)?, started_at: r.get(5)?, duration_ms: r.get(6)?,
                    status: r.get(7)?, detail: r.get(8)?, result: r.get(9)?, truncated: r.get(10)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some((run, rows)))
    }

    /// The newest run opened for a conversation. Test-only: production
    /// reaches a run from the message that names it, never by searching.
    #[cfg(test)]
    pub(crate) fn latest_run_id(&self, conversation_id: i64) -> Result<Option<i64>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT id FROM runs WHERE conversation_id = ? ORDER BY id DESC LIMIT 1",
                params![conversation_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Deletes everything but the newest `keep_runs` runs, rows included.
    /// Returns how many runs went.
    pub fn trim_traces(&self, keep_runs: usize) -> Result<usize> {
        let conn = self.conn();
        // Rows first, on purpose: a crash between the two leaves only
        // trace-less runs behind, which the next hourly trim removes.
        conn.execute(
            "DELETE FROM run_traces WHERE run_id NOT IN (SELECT id FROM runs ORDER BY id DESC LIMIT ?)",
            params![keep_runs as i64],
        )?;
        Ok(conn.execute(
            "DELETE FROM runs WHERE id NOT IN (SELECT id FROM runs ORDER BY id DESC LIMIT ?)",
            params![keep_runs as i64],
        )?)
    }

    pub fn set_debug(&self, account_id: i64, on: bool) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE accounts SET debug = ? WHERE id = ?", params![on, account_id])?;
        Ok(())
    }

    pub fn debug_of(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn();
        Ok(conn
            .query_row("SELECT debug FROM accounts WHERE id = ?", params![account_id], |r| r.get(0))
            .optional()?
            .unwrap_or(false))
    }

    /// The last `limit` messages, oldest first — the order a provider
    /// wants — each with the run that wrote it, so a Scout turn can be
    /// paired with its trace.
    pub fn conversation_messages(&self, conversation_id: i64, limit: usize) -> Result<Vec<(Option<i64>, String)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT run_id, body FROM (
                 SELECT run_id, body, position FROM messages WHERE conversation_id = ?
                 ORDER BY position DESC LIMIT ?
             ) ORDER BY position ASC",
        )?;
        let rows = stmt.query_map(params![conversation_id, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Adds messages after whatever the conversation already holds, in one
    /// lock so a reader never sees the thread half-written.
    ///
    /// This is how a run writes: the table is the conversation's whole log,
    /// and a run only ever knows about its own turns. `replace_messages`
    /// used to be the writer, on the argument that the store should hold
    /// exactly what the agent would be sent next — which made the model's
    /// twenty-message window the whole of the thread anybody could read, so
    /// a long conversation opened in the browser showing only its last
    /// exchange. The window is applied on the way *out* now; see
    /// `session::load_history`.
    ///
    /// `updated_at` moves even for an empty append, because a run that
    /// produced nothing still happened: the sidebar orders by that column
    /// and the 48-hour sweep deletes by it.
    pub fn append_messages(&self, conversation_id: i64, run_id: Option<i64>, bodies: &[String]) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            // Read inside the transaction: the next position has to be
            // decided against the rows this insert will sit beside.
            let next: i64 = conn.query_row(
                "SELECT COALESCE(MAX(position) + 1, 0) FROM messages WHERE conversation_id = ?",
                params![conversation_id],
                |r| r.get(0),
            )?;
            for (i, body) in bodies.iter().enumerate() {
                conn.execute(
                    "INSERT INTO messages (id, conversation_id, position, body, run_id)
                     VALUES (nextval('messages_id_seq'), ?, ?, ?, ?)",
                    params![conversation_id, next + i as i64, body, run_id],
                )?;
            }
            conn.execute(
                "UPDATE conversations SET updated_at = now() WHERE id = ?",
                params![conversation_id],
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Replaces a conversation's messages wholesale, in one lock so a reader
    /// never sees the thread half-written.
    ///
    /// Not what a run uses — that appends. This is for a caller that means
    /// "this conversation is exactly these messages": seeding an exchange in
    /// a test, and anything else that writes a thread rather than continuing
    /// one. Kept because "set the log" and "add to the log" are different
    /// claims, and only one of them can lose a conversation.
    pub fn replace_messages(&self, conversation_id: i64, bodies: &[String]) -> Result<()> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            conn.execute(
                "DELETE FROM messages WHERE conversation_id = ?",
                params![conversation_id],
            )?;
            for (i, body) in bodies.iter().enumerate() {
                conn.execute(
                    "INSERT INTO messages (id, conversation_id, position, body)
                     VALUES (nextval('messages_id_seq'), ?, ?, ?)",
                    params![conversation_id, i as i64, body],
                )?;
            }
            conn.execute(
                "UPDATE conversations SET updated_at = now() WHERE id = ?",
                params![conversation_id],
            )?;
            Ok(())
        })();
        if let Err(e) = result {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(e);
        }
        conn.execute_batch("COMMIT")?;
        Ok(())
    }

    /// Queues one turn for a channel, or does nothing if it is already
    /// known. Returns whether a row was written.
    ///
    /// `delivered` is how a channel records a turn it has already shown the
    /// reader: the row is inserted with `sent_at` set, so it occupies the
    /// key and is never dispatched.
    ///
    /// Keyed by channel as well as account and turn. Without it, the first
    /// day a second channel exists, queueing a turn for it returns `false`
    /// because the first channel already holds that key — and that channel
    /// never delivers, with nothing to show for it.
    ///
    /// Check-then-insert rather than `ON CONFLICT`, because the store holds
    /// one mutex and is the only writer, so the pair is atomic here in a way
    /// it would not be over a network. The `UNIQUE` constraint stays as a
    /// backstop against a second writer nobody has added yet.
    pub fn enqueue_mirror(
        &self,
        account_id: i64,
        channel: &str,
        address: &str,
        body: &str,
        turn_key: &str,
        delivered: bool,
    ) -> Result<bool> {
        let conn = self.conn();
        let known: i64 = conn.query_row(
            "SELECT count(*) FROM outbox WHERE account_id = ? AND channel = ? AND turn_key = ?",
            params![account_id, channel, turn_key],
            |r| r.get(0),
        )?;
        if known > 0 {
            return Ok(false);
        }
        // `now()` in the statement rather than a bound timestamp: `duckdb`
        // is built here without its `chrono` feature, so a `NaiveDateTime`
        // has no `ToSql`, and every other write in this file dates itself
        // the same way.
        let sql = if delivered {
            "INSERT INTO outbox (id, account_id, channel, address, body, turn_key, sent_at)
             VALUES (nextval('outbox_id_seq'), ?, ?, ?, ?, ?, now())"
        } else {
            "INSERT INTO outbox (id, account_id, channel, address, body, turn_key)
             VALUES (nextval('outbox_id_seq'), ?, ?, ?, ?, ?)"
        };
        conn.execute(sql, params![account_id, channel, address, body, turn_key])?;
        Ok(true)
    }

    /// Turns still waiting to go out on a channel, oldest first.
    ///
    /// Oldest first because a thread delivered out of order is worse than
    /// one delivered late. Rows past [`MIRROR_ATTEMPTS`] are left behind
    /// rather than returned.
    pub fn pending_mirror(&self, channel: &str, limit: usize) -> Result<Vec<PendingMirror>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, address, body FROM outbox
             WHERE channel = ? AND sent_at IS NULL AND attempts < ?
             ORDER BY id LIMIT ?",
        )?;
        let rows = stmt.query_map(params![channel, MIRROR_ATTEMPTS, limit as i64], |r| {
            Ok(PendingMirror {
                id: r.get(0)?,
                account_id: r.get(1)?,
                address: r.get(2)?,
                body: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// It arrived. The row stays as the ledger entry that stops it being
    /// sent again by a later backfill.
    pub fn mark_mirror_sent(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE outbox SET sent_at = now() WHERE id = ?", params![id])?;
        Ok(())
    }

    /// It did not arrive. Returns how many attempts the row has now spent,
    /// so the caller can say out loud when one is given up on.
    ///
    /// At [`MIRROR_ATTEMPTS`] the row stops being returned by
    /// `pending_mirror` and there is no way back: its `turn_key` is still
    /// occupied, so re-enqueueing it is a no-op and toggling the mirror off
    /// and on will not recover it. That is a thing a human should be able
    /// to find in a log afterwards.
    pub fn mark_mirror_failed(&self, id: i64) -> Result<i64> {
        let conn = self.conn();
        conn.execute("UPDATE outbox SET attempts = attempts + 1 WHERE id = ?", params![id])?;
        let attempts: i64 =
            conn.query_row("SELECT attempts FROM outbox WHERE id = ?", params![id], |r| r.get(0))?;
        Ok(attempts)
    }

    pub fn mirror_enabled(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn();
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM mirrored_accounts WHERE account_id = ?",
            params![account_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Presence is the setting, so turning it on twice is not an error and
    /// turning it off is a delete.
    pub fn set_mirror(&self, account_id: i64, on: bool) -> Result<()> {
        let conn = self.conn();
        if on {
            conn.execute(
                "INSERT INTO mirrored_accounts (account_id) SELECT ?
                 WHERE NOT EXISTS (SELECT 1 FROM mirrored_accounts WHERE account_id = ?)",
                params![account_id, account_id],
            )?;
        } else {
            conn.execute("DELETE FROM mirrored_accounts WHERE account_id = ?", params![account_id])?;
        }
        Ok(())
    }

    /// The account proving control of this identity, creating one if the
    /// identity is new.
    ///
    /// `kind` is `&'static str` rather than `&str` on purpose. It is half of
    /// a primary key, and a kind read off the wire — a typo, or a value an
    /// attacker chose — would silently open a parallel identity space that
    /// nothing else can see.
    ///
    /// Both branches run under the same lock, so two updates arriving from
    /// the same person cannot mint two accounts for them.
    pub fn account_for_identity(&self, kind: &'static str, external_id: &str) -> Result<i64> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT account_id FROM identities WHERE kind = ? AND external_id = ?")?;
        let found: Option<i64> =
            stmt.query_map(params![kind, external_id], |r| r.get(0))?.next().transpose()?;
        drop(stmt);
        if let Some(id) = found {
            return Ok(id);
        }
        let account_id: i64 = conn.query_row(
            "INSERT INTO accounts (id) VALUES (nextval('accounts_id_seq')) RETURNING id",
            [],
            |r| r.get(0),
        )?;
        conn.execute(
            "INSERT INTO identities (account_id, kind, external_id) VALUES (?, ?, ?)",
            params![account_id, kind, external_id],
        )?;
        Ok(account_id)
    }

    /// Attaches a second identity to an account that already exists.
    ///
    /// The `PRIMARY KEY (kind, external_id)` is what actually prevents two
    /// owners under a race; this read exists to produce a sentence a person
    /// can act on instead of a constraint violation.
    pub fn link_identity(
        &self,
        account_id: i64,
        kind: &'static str,
        external_id: &str,
    ) -> Result<LinkOutcome> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT account_id FROM identities WHERE kind = ? AND external_id = ?")?;
        let owner: Option<i64> =
            stmt.query_map(params![kind, external_id], |r| r.get(0))?.next().transpose()?;
        drop(stmt);
        let other = match owner {
            Some(id) if id == account_id => return Ok(LinkOutcome::AlreadyYours),
            Some(id) => id,
            None => {
                conn.execute(
                    "INSERT INTO identities (account_id, kind, external_id) VALUES (?, ?, ?)",
                    params![account_id, kind, external_id],
                )?;
                return Ok(LinkOutcome::Linked);
            }
        };

        // Two accounts, one identity. The side holding nothing is absorbed
        // into the side holding something, and if both hold something they
        // stay apart — a merge cannot be undone, so it is only ever done
        // where there is provably nothing to lose.
        let (survivor, absorbed) =
            match (is_empty_account(&conn, account_id)?, is_empty_account(&conn, other)?) {
                (false, false) => return Ok(LinkOutcome::TakenByAnother),
                (false, true) => (account_id, other),
                (true, false) => (other, account_id),
                // Both empty, so neither has a claim on the other. Keeping
                // the older id makes the result the same whichever of the
                // two asked, rather than a coin toss decided by who clicked.
                (true, true) => (account_id.min(other), account_id.max(other)),
            };

        merge_accounts(&conn, absorbed, survivor)?;
        Ok(LinkOutcome::Merged { account_id: survivor })
    }

    /// Which ways of proving this account exist — `'email'`, `'telegram'`.
    ///
    /// The kinds and never the external ids. What asks is a page offering
    /// to attach whichever method is missing, and it needs to know that a
    /// method exists, not what it is. Handing back the address as well
    /// would put a value somebody chose into a page, and then escaping it
    /// correctly would be everyone's problem forever.
    ///
    /// `DISTINCT` because the row is `(kind, external_id)` and one account
    /// may hold two of a kind — two Telegram accounts linked to one Scout
    /// account is a row per identity and a legitimate shape. The caller
    /// asks which *methods* exist, so it wants one answer per method: the
    /// page that renders this list writes it out in order, and without
    /// this it reads "Signed in with Telegram, Telegram."
    pub fn identity_kinds(&self, account_id: i64) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT DISTINCT kind FROM identities WHERE account_id = ? ORDER BY kind ASC",
        )?;
        let rows = stmt.query_map(params![account_id], |row| row.get(0))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// The Telegram ids this account can prove, as numbers.
    ///
    /// `external_id` is TEXT because an identity's id is opaque to the
    /// store; Telegram's happen to be integers and the founder list is
    /// integers, so this is where the two meet. A row that will not parse
    /// is skipped rather than failing the call — one unreadable identity
    /// must not make an account impossible to ask about.
    pub fn telegram_ids(&self, account_id: i64) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT external_id FROM identities WHERE account_id = ? AND kind = 'telegram'",
        )?;
        let rows = stmt.query_map(params![account_id], |r| r.get::<_, String>(0))?;
        let mut ids = Vec::new();
        for row in rows {
            if let Ok(id) = row?.parse::<i64>() {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// Whether this account holds a seat that has not been revoked.
    ///
    /// The same question `claim_seat` asks first, as a read. A page that
    /// reported standing by calling `claim_seat` would seat a queued
    /// visitor the moment they looked at it, and spend a seat on a `GET`.
    pub fn is_member(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT revoked_at IS NULL FROM members WHERE account_id = ?")?;
        let standing: Option<bool> =
            stmt.query_map(params![account_id], |row| row.get(0))?.next().transpose()?;
        Ok(standing.unwrap_or(false))
    }

    /// Records a token that has been mailed out.
    ///
    /// `token_hash` is a hash of the value in the link, never the value:
    /// a database that leaks must not hand over working sign-in links.
    /// `ttl_secs` is signed so a test can issue one already expired.
    ///
    /// `expires_at` is computed in Rust from `chrono::Utc::now()` — the same
    /// UTC-naive clock `consume_login_token` compares against via
    /// `current_timestamp AT TIME ZONE 'UTC'` — rather than derived with
    /// bare `current_timestamp`, which is local. See `requests_today` for
    /// the bug that pattern caused.
    ///
    /// The addition happens in Rust rather than as `TIMESTAMP + INTERVAL`
    /// in SQL because of a DuckDB behaviour that is reproducible and
    /// unexplained. Measured on this build, five runs, identical every
    /// time: `(current_timestamp AT TIME ZONE 'UTC') +
    /// to_seconds(CAST(? AS INTEGER))`, executed as the *first* statement
    /// on a freshly opened connection, fails to bind with "No function
    /// matches ... (TIMESTAMP, INTERVAL)". The same statement, executed
    /// after any four other queries on the same connection, succeeds. It is
    /// not prepare-versus-execute — both were executed — and it is not
    /// flaky. Literal intervals, `to_seconds(900)` and `CAST(? AS INTERVAL)`
    /// all bind from cold.
    ///
    /// Why the first statement differs is not known. An earlier version of
    /// this comment called it a "startup race", which is wrong twice over:
    /// nothing here is racing, and no mechanism was ever established. The
    /// honest statement is that the shape is avoided, not understood.
    ///
    /// Computing the value here sidesteps it entirely, and the `<`
    /// comparison in `consume_login_token` is between two TIMESTAMPs with
    /// no arithmetic, so it never meets the same wall.
    pub fn issue_login_token(
        &self,
        token_hash: &str,
        email: &str,
        account_id: Option<i64>,
        ttl_secs: i64,
    ) -> Result<()> {
        let expires_at = chrono::Utc::now().naive_utc() + chrono::Duration::seconds(ttl_secs);
        let conn = self.conn();
        conn.execute(
            "INSERT INTO login_tokens (token_hash, email, account_id, expires_at)
             VALUES (?, ?, ?, ?)",
            params![token_hash, email, account_id, expires_at.to_string()],
        )?;
        Ok(())
    }

    /// Spends a token, if it has anything left to spend.
    ///
    /// Marking consumed and reading the row happen under one mutex, so two
    /// simultaneous clicks cannot both come back `Valid`.
    ///
    /// `consumed_at` is computed here from `Utc::now()`, matching
    /// `issue_login_token` and `prune_login_tokens`. It used to be written
    /// by bare `current_timestamp`, which DuckDB resolves in the session's
    /// local zone, while every other timestamp this table holds — and the
    /// expiry comparison right above — is UTC-naive. Nothing reads the
    /// value yet, only whether it is null, so the two clocks cost nothing
    /// today; that is the whole reason to fix it now rather than after
    /// something reads it. See `requests_today` for what this mismatch
    /// costs once something does.
    ///
    /// Rust rather than `current_timestamp AT TIME ZONE 'UTC'`, which would
    /// also be correct, because then all three writers of a timestamp in
    /// this table are the same line and there is nothing to compare.
    pub fn consume_login_token(&self, token_hash: &str) -> Result<TokenOutcome> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT email, account_id, consumed_at IS NOT NULL,
                    expires_at < (current_timestamp AT TIME ZONE 'UTC')::TIMESTAMP
             FROM login_tokens WHERE token_hash = ?",
        )?;
        let row: Option<(String, Option<i64>, bool, bool)> = stmt
            .query_map(params![token_hash], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .next()
            .transpose()?;
        drop(stmt);
        let Some((email, account_id, consumed, expired)) = row else {
            return Ok(TokenOutcome::Unknown);
        };
        if consumed {
            return Ok(TokenOutcome::AlreadyUsed);
        }
        if expired {
            return Ok(TokenOutcome::Expired);
        }
        let consumed_at = chrono::Utc::now().naive_utc();
        conn.execute(
            "UPDATE login_tokens SET consumed_at = ? WHERE token_hash = ?",
            params![consumed_at.to_string(), token_hash],
        )?;
        Ok(TokenOutcome::Valid { email, account_id })
    }

    /// Drops login tokens that stopped being worth keeping, returning how
    /// many went. `keep_secs` is measured from a row's own expiry.
    ///
    /// The table is written once per sign-in *attempt*, by anyone who can
    /// reach the form, and nothing ever deleted from it — so it grew with
    /// every request forever, which is a table a stranger controls the size
    /// of. Consumed rows are kept past their expiry on purpose, so that
    /// "already used" and "expired" stay distinguishable, but that only has
    /// to hold for as long as the advice is worth giving: someone clicking
    /// a link from a week-old email is told it is dead either way.
    ///
    /// The cutoff is computed here from `Utc::now()` rather than in SQL,
    /// matching `issue_login_token`: `expires_at` is UTC-naive and bare
    /// `current_timestamp` is local, and mixing the two is the mismatch
    /// documented on `requests_today` as having caused a real bug.
    pub fn prune_login_tokens(&self, keep_secs: i64) -> Result<usize> {
        let cutoff = chrono::Utc::now().naive_utc() - chrono::Duration::seconds(keep_secs);
        let conn = self.conn();
        Ok(conn.execute(
            "DELETE FROM login_tokens WHERE expires_at < ?",
            params![cutoff.to_string()],
        )?)
    }

    /// The account behind a Telegram user, creating one the first time.
    ///
    /// Delegates to `account_for_identity` so there is one lookup-or-create
    /// implementation rather than two that can drift.
    pub fn account_for_telegram(&self, telegram_id: i64) -> Result<i64> {
        self.account_for_identity("telegram", &telegram_id.to_string())
    }

    /// Where this account can be reached on a channel, if anywhere.
    ///
    /// The counterpart of `note_delivery`. Nothing could read a single
    /// account's address before: the only reader was the announce query,
    /// which wants the whole waitlist at once. A run that has to decide
    /// whether it can honour a reminder needs to ask about one person.
    pub fn delivery_address(&self, account_id: i64, channel: &str) -> Result<Option<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT address FROM deliveries WHERE account_id = ? AND channel = ?",
        )?;
        let found: Option<String> =
            stmt.query_map(params![account_id, channel], |r| r.get(0))?.next().transpose()?;
        Ok(found)
    }

    /// Records where this person last spoke, for announcements.
    ///
    /// A user id and a chat id are the same number in a private chat and
    /// different in a group, so the chat is stored rather than derived.
    pub fn note_delivery(&self, account_id: i64, channel: &str, address: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO deliveries (account_id, channel, address) VALUES (?, ?, ?)
             ON CONFLICT (account_id, channel)
             DO UPDATE SET address = excluded.address, updated_at = now()",
            params![account_id, channel, address],
        )?;
        Ok(())
    }

    /// Everyone the bot could announce something to on a channel, as
    /// (account, address).
    pub fn broadcast_targets(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT account_id, CAST(address AS BIGINT) FROM deliveries
             WHERE channel = 'telegram' ORDER BY account_id ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Everyone admitted and not since revoked, as **Telegram ids**. Read
    /// once at startup to build the membership set the gate consults.
    ///
    /// Telegram ids rather than account ids because the gate runs on every
    /// update and all it has is `sender_id`. Resolving an account there
    /// would put a database read in front of every message from every
    /// stranger, which is the cost this set exists to avoid.
    pub fn active_members(&self) -> Result<Vec<i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT CAST(i.external_id AS BIGINT)
             FROM members m
             JOIN identities i ON i.account_id = m.account_id AND i.kind = 'telegram'
             WHERE m.revoked_at IS NULL
             ORDER BY 1 ASC",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Try to take a seat in `code` for `account_id`.
    ///
    /// One method, one lock acquisition, so check-and-insert is atomic by
    /// construction: a round of 100 admits exactly 100 however many people
    /// press START at the same moment. There is no counter to drift from
    /// the rows — seats used *is* `count(*)` over them.
    pub fn claim_seat(&self, account_id: i64, code: &str) -> Result<Claim> {
        let conn = self.conn();

        // Membership decides before the round does. Checking the round
        // first would let a revoked person be told "that round is full",
        // and a member re-clicking a link consume a second seat.
        let mut stmt = conn.prepare("SELECT revoked_at IS NULL FROM members WHERE account_id = ?")?;
        let standing: Option<bool> = stmt
            .query_map(params![account_id], |row| row.get(0))?
            .next()
            .transpose()?;
        match standing {
            Some(true) => return Ok(Claim::AlreadyIn),
            // Without this, revoking is theatre: the next link would let
            // them straight back in.
            Some(false) => return Ok(Claim::Revoked),
            None => {}
        }

        let mut stmt = conn.prepare(
            "SELECT r.open, r.capacity, (SELECT count(*) FROM members m WHERE m.code = r.code)
             FROM invite_rounds r WHERE r.code = ?",
        )?;
        let round: Option<(bool, i64, i64)> = stmt
            .query_map(params![code], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .next()
            .transpose()?;

        // Unknown code, closed round and full round are one outcome on
        // purpose: telling a stranger which it was says whether a code they
        // guessed exists, and that is information with no use.
        if !matches!(round, Some((true, capacity, used)) if used < capacity) {
            conn.execute(
                "INSERT INTO waitlist (account_id, code) VALUES (?, ?)
                 ON CONFLICT (account_id) DO UPDATE SET
                     code = excluded.code,
                     -- They tried again and were turned away again, so they
                     -- are waiting again — but `seen_at` is untouched, so a
                     -- second attempt does not cost them their place.
                     invited_at = NULL",
                params![account_id, code],
            )?;
            return Ok(Claim::NoRoom);
        }

        conn.execute(
            "INSERT INTO members (account_id, code) VALUES (?, ?)",
            params![account_id, code],
        )?;
        // Nobody who is inside should be chased by a later announce.
        conn.execute("DELETE FROM waitlist WHERE account_id = ?", params![account_id])?;
        Ok(Claim::Admitted)
    }

    /// Opens a round. False when the name is already taken — reusing one
    /// would silently pool two rounds' seats under a single capacity.
    pub fn create_round(&self, code: &str, capacity: i64) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            "INSERT INTO invite_rounds (code, capacity) VALUES (?, ?)
             ON CONFLICT (code) DO NOTHING",
            params![code, capacity],
        )?;
        Ok(changed == 1)
    }

    /// Stops or resumes admitting. False when there is no such round.
    pub fn set_round_open(&self, code: &str, open: bool) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE invite_rounds SET open = ? WHERE code = ?",
            params![open, code],
        )?;
        Ok(changed > 0)
    }

    /// Every round, oldest first, with seats counted from the member rows.
    pub fn rounds(&self) -> Result<Vec<RoundStatus>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT r.code, r.capacity, r.open,
                    (SELECT count(*) FROM members m WHERE m.code = r.code)
             FROM invite_rounds r ORDER BY r.created_at ASC, r.code ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RoundStatus {
                code: row.get(0)?,
                capacity: row.get(1)?,
                open: row.get(2)?,
                used: row.get(3)?,
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// How many people are queued and have not been told about a new round.
    pub fn waiting_count(&self) -> Result<i64> {
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT count(*) FROM waitlist WHERE invited_at IS NULL")?;
        let n: i64 = stmt.query_map([], |row| row.get(0))?.next().transpose()?.unwrap_or(0);
        Ok(n)
    }

    /// Removes a member. False when they were not one (or already were
    /// removed). The row stays: the seat is spent, and moderation must not
    /// quietly reopen a round.
    pub fn revoke(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE members SET revoked_at = current_timestamp
             WHERE account_id = ? AND revoked_at IS NULL",
            params![account_id],
        )?;
        Ok(changed > 0)
    }

    /// Hands a seat back to its round by removing the row, rather than
    /// marking it.
    ///
    /// The opposite choice from `revoke`, deliberately. Revoking keeps the
    /// row so that moderation cannot quietly reopen a round. This deletes
    /// it, because it is only ever called for a seat nobody is sitting in:
    /// a founder is admitted by `ALLOWED_TELEGRAM_USER_IDS` and never
    /// consults `members` at all, so a seat that lands on one is a seat the
    /// round has lost for no reason. False when there was none to hand back.
    pub fn release_seat(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn();
        let changed =
            conn.execute("DELETE FROM members WHERE account_id = ?", params![account_id])?;
        Ok(changed > 0)
    }

    /// Undoes a revoke. False when they were not revoked. Consumes no seat,
    /// for the same reason revoking returned none.
    pub fn restore(&self, account_id: i64) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE members SET revoked_at = NULL WHERE account_id = ? AND revoked_at IS NOT NULL",
            params![account_id],
        )?;
        Ok(changed > 0)
    }

    /// Who an announce should reach, as (user, chat), oldest first — so if
    /// the new round is smaller than the queue, the people who have waited
    /// longest hear first.
    pub fn waitlist_to_invite(&self) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn();
        // An inner join, deliberately: someone with no recorded address
        // cannot be reached, and silently announcing to nobody would look
        // like a delivered invitation.
        let mut stmt = conn.prepare(
            "SELECT w.account_id, CAST(d.address AS BIGINT)
             FROM waitlist w
             JOIN deliveries d ON d.account_id = w.account_id AND d.channel = 'telegram'
             WHERE w.invited_at IS NULL
             ORDER BY w.seen_at ASC, w.account_id ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Stamped per successful send, so re-running an announce reaches only
    /// the people the first run missed.
    pub fn mark_invited(&self, account_id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE waitlist SET invited_at = current_timestamp WHERE account_id = ?",
            params![account_id],
        )?;
        Ok(())
    }

    /// Drops somebody from the queue. Used when a send proves they cannot
    /// be reached at all — they blocked the bot or deleted the chat, which
    /// is an opt-out, and carrying them forward would mean retrying that
    /// same failure at every future round.
    pub fn forget_waitlist(&self, account_id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("DELETE FROM waitlist WHERE account_id = ?", params![account_id])?;
        Ok(())
    }

    /// Requests this user has made since midnight, for the daily cap.
    ///
    /// Reactions and flight searches are excluded: a reaction is not a
    /// request, and a flight search is a sub-event of a message already
    /// counted, so counting it would charge one message twice.
    ///
    /// Midnight is UTC because that is how the rows are stored, not because
    /// UTC is nicer. `created_at` is a naive TIMESTAMP defaulting to
    /// `current_timestamp`, and DuckDB writes that as the UTC instant with
    /// the zone stripped, while `current_date` stays local. Measured on a
    /// CEST machine: stored `2026-08-29 22:35`, `current_date`
    /// `2026-08-30`. This used to compare against `current_date`, which
    /// therefore excluded every row written since local midnight and
    /// counted zero — the cap switching itself off for as many hours as the
    /// offset. The container runs UTC, so the two agreed and production
    /// never saw it. The test suite did, nightly.
    pub fn requests_today(&self, account_id: i64) -> Result<i64> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT count(*) FROM request_log
             WHERE account_id = ? AND kind IN ('text', 'photo')
               AND created_at >= CAST(current_timestamp AT TIME ZONE 'UTC' AS DATE)",
        )?;
        let n: i64 = stmt
            .query_map(params![account_id], |row| row.get(0))?
            .next()
            .transpose()?
            .unwrap_or(0);
        Ok(n)
    }

    /// Flight searches per user since `cutoff`, scoped to one user. See
    /// [`FLIGHT_SEARCH`].
    pub fn flight_searches_for(&self, cutoff: &str, account_id: i64) -> Result<BTreeMap<i64, i64>> {
        self.kind_counts(cutoff, Self::FLIGHT_SEARCH, Some(account_id))
    }

    /// The same across every user. Reachable from `/stat` only when the
    /// caller is an admin — like `usage_stats_all`, this method and its
    /// callers are the access-control surface for cross-user data.
    pub fn flight_searches_all(&self, cutoff: &str) -> Result<BTreeMap<i64, i64>> {
        self.kind_counts(cutoff, Self::FLIGHT_SEARCH, None)
    }

    /// Requests of one `kind` per user, optionally narrowed to a single
    /// user. `None` means every user, so callers pass `Some` unless they
    /// have already checked the caller is allowed to see everyone.
    fn kind_counts(
        &self,
        cutoff: &str,
        kind: &str,
        account_id: Option<i64>,
    ) -> Result<BTreeMap<i64, i64>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT account_id, count(*) FROM request_log
             WHERE kind = ? AND created_at >= CAST(? AS TIMESTAMP)
               AND (? IS NULL OR account_id = ?)
             GROUP BY account_id ORDER BY account_id ASC",
        )?;
        let rows = stmt.query_map(params![kind, cutoff, account_id, account_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Last-seen display name per user id. Small enough to read whole —
    /// one row per person who has ever messaged the bot.
    /// Accounts with a Telegram identity but no recorded display name, as
    /// (account, telegram id).
    ///
    /// Both ids, because the caller needs the Telegram one to ask Telegram
    /// and the account one to file the answer. Handing back only the account
    /// id is what let a display-name backfill call `get_chat` on an account
    /// id and mint a bogus identity from the result.
    pub fn accounts_missing_display_names(&self, limit: usize) -> Result<Vec<(i64, i64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT i.account_id, CAST(i.external_id AS BIGINT)
             FROM identities i
             WHERE i.kind = 'telegram'
               AND NOT EXISTS (SELECT 1 FROM users u WHERE u.account_id = i.account_id)
             ORDER BY i.account_id ASC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    pub fn display_names(&self) -> Result<BTreeMap<i64, String>> {
        let conn = self.conn();
        // The Telegram name when there is one; otherwise the email the
        // account signed in with, so a web-only account is not a dash in
        // `/stat`. An account with neither is left out, as before.
        let mut stmt = conn.prepare(
            "SELECT account_id, display_name FROM users
             UNION ALL
             SELECT account_id, min(external_id) FROM identities
             WHERE kind = 'email'
               AND account_id NOT IN (SELECT account_id FROM users)
             GROUP BY account_id",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Creates the trip if the name is new, otherwise updates only what was
    /// supplied. Separate statements rather than one `ON CONFLICT DO UPDATE`:
    /// with upsert, an unsupplied `adults` would arrive as the insert's
    /// default and overwrite a value already set. `conversation_id` fills the
    /// owner only if there isn't one: the creating chat keeps a trip, an
    /// orphan is adopted.
    pub fn upsert_trip(
        &self,
        account_id: i64,
        name: &str,
        adults: Option<i64>,
        cabin_class: Option<&str>,
        conversation_id: Option<i64>,
    ) -> Result<Trip> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("a trip needs a name — ask the traveller what to call it");
        }
        let key = name.to_lowercase();
        let conn = self.conn();
        let inserted = conn.execute(
            "INSERT INTO trips (account_id, name, name_key) VALUES (?, ?, ?)
             ON CONFLICT (account_id, name_key) DO NOTHING",
            params![account_id, name, key],
        )?;
        // Only ever fills a hole. `IS NULL` is what makes the creator keep
        // the trip while an orphan gets adopted, in one statement and with
        // no second code path. No `updated_at` bump here, unlike the
        // updates below: adoption is bookkeeping, not an edit, and bumping
        // it would reorder the traveller's trip list just because a
        // different chat mentioned the trip.
        if let Some(conversation_id) = conversation_id {
            conn.execute(
                "UPDATE trips SET conversation_id = ?
                 WHERE account_id = ? AND name_key = ? AND conversation_id IS NULL",
                params![conversation_id, account_id, key],
            )?;
        }
        // A freshly created trip already starts in `planning`, and a call
        // that supplies neither field is how find-or-create works — it must
        // stay inert. Only an edit to an *existing* trip's price-relevant
        // fields can invalidate prices it was finalised at.
        let invalidates_prices = inserted == 0 && (adults.is_some() || cabin_class.is_some());
        if let Some(adults) = adults {
            conn.execute(
                "UPDATE trips SET adults = ?, updated_at = current_timestamp
                 WHERE account_id = ? AND name_key = ?",
                params![adults, account_id, key],
            )?;
        }
        if let Some(cabin) = cabin_class {
            conn.execute(
                "UPDATE trips SET cabin_class = ?, updated_at = current_timestamp
                 WHERE account_id = ? AND name_key = ?",
                params![cabin, account_id, key],
            )?;
        }
        if invalidates_prices {
            conn.execute(
                "UPDATE trips SET status = 'planning', updated_at = current_timestamp
                 WHERE account_id = ? AND name_key = ?",
                params![account_id, key],
            )?;
        }
        let id: i64 = conn.query_row(
            "SELECT id FROM trips WHERE account_id = ? AND name_key = ?",
            params![account_id, key],
            |row| row.get(0),
        )?;
        load_trip(&conn, id)
    }

    /// Which conversation owns this trip, if any.
    ///
    /// Test-only, and gated so that stays true: production reads ownership
    /// through `Plan`, which a later task has carry it. Ungated it is dead
    /// code in a release build, and a build that always warns is a build
    /// whose warnings nobody reads.
    #[cfg(test)]
    pub fn trip_owner(&self, trip_id: i64) -> Result<Option<i64>> {
        let conn = self.conn();
        Ok(conn.query_row(
            "SELECT conversation_id FROM trips WHERE id = ?",
            params![trip_id],
            |row| row.get(0),
        )?)
    }

    /// The conversation that owns this trip, if it still exists. A `JOIN`,
    /// not two reads: a trip whose `conversation_id` points at a row that is
    /// gone must read as orphaned rather than as a chat with missing fields.
    pub fn trip_chat(&self, trip_id: i64) -> Result<Option<TripChat>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.title, c.scope FROM trips t
             JOIN conversations c ON c.id = t.conversation_id
             WHERE t.id = ?",
        )?;
        let chat = stmt
            .query_map(params![trip_id], |r| {
                Ok(TripChat { id: r.get(0)?, title: r.get(1)?, scope: r.get(2)? })
            })?
            .next()
            .transpose()?;
        Ok(chat)
    }

    /// Used by finalisation to record that a trip has been priced.
    pub fn set_trip_status(&self, trip_id: i64, status: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trips SET status = ?, updated_at = current_timestamp WHERE id = ?",
            params![status, trip_id],
        )?;
        Ok(())
    }

    /// One trip by id, if it is this account's. The owner is part of the
    /// key, as in `arrival_of`: an id from the page proves nothing.
    pub fn trip_by_id(&self, account_id: i64, trip_id: i64) -> Result<Option<Trip>> {
        let conn = self.conn();
        let found: Option<i64> = conn
            .query_row(
                "SELECT id FROM trips WHERE id = ? AND account_id = ?",
                params![trip_id, account_id],
                |row| row.get(0),
            )
            .optional()?;
        found.map(|id| load_trip(&conn, id)).transpose()
    }

    pub fn find_trip(&self, account_id: i64, name: &str) -> Result<Option<Trip>> {
        let key = name.trim().to_lowercase();
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id FROM trips WHERE account_id = ? AND name_key = ?")?;
        let id: Option<i64> = stmt
            .query_map(params![account_id, key], |row| row.get(0))?
            .next()
            .transpose()?;
        match id {
            Some(id) => Ok(Some(load_trip(&conn, id)?)),
            None => Ok(None),
        }
    }

    /// Every trip this user has, newest activity first.
    pub fn list_trips(&self, account_id: i64) -> Result<Vec<Trip>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT id FROM trips WHERE account_id = ? ORDER BY updated_at DESC, id DESC")?;
        let ids: Vec<i64> = stmt
            .query_map(params![account_id], |row| row.get(0))?
            .collect::<duckdb::Result<_>>()?;
        ids.into_iter().map(|id| load_trip(&conn, id)).collect()
    }

    /// The trips the traveller asked to keep, newest activity first.
    ///
    /// Nothing outside this file's tests calls this today. It was the
    /// channel-facing read for the one day the Trips tab hid drafts; that
    /// was reversed, and `trips::list` records why. Left here rather than
    /// deleted because `kept` is still a real distinction — expiry takes an
    /// unkept draft and spares a kept trip — so this is the query any
    /// "kept only" view would ask for.
    ///
    /// `#[cfg(test)]` and not `#[allow(dead_code)]`: with `mod store`
    /// private, an unused method here is dead in earnest, and this says so
    /// in the type system instead of silencing the compiler. Its tests
    /// still run, so it cannot rot before something wants it back. Drop the
    /// attribute the moment a caller appears — or drop the method, if none
    /// ever does.
    #[cfg(test)]
    pub fn list_kept_trips(&self, account_id: i64) -> Result<Vec<Trip>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM trips WHERE account_id = ? AND kept
             ORDER BY updated_at DESC, id DESC",
        )?;
        let ids: Vec<i64> = stmt
            .query_map(params![account_id], |row| row.get(0))?
            .collect::<duckdb::Result<_>>()?;
        ids.into_iter().map(|id| load_trip(&conn, id)).collect()
    }

    /// Marks a trip the traveller asked to keep, so it appears in their
    /// list. Idempotent: keeping a kept trip is a no-op that still reports
    /// the trip was found, because "keep this" said twice is a traveller
    /// repeating themselves, not an error.
    ///
    /// This does bump `updated_at`, unlike adoption: keeping is something
    /// the traveller did, and the list is ordered by it, so a trip they
    /// just kept belongs at the top.
    pub fn keep_trip(&self, account_id: i64, name: &str) -> Result<bool> {
        let key = name.trim().to_lowercase();
        let conn = self.conn();
        let found: i64 = conn.query_row(
            "SELECT count(*) FROM trips WHERE account_id = ? AND name_key = ?",
            params![account_id, key],
            |row| row.get(0),
        )?;
        if found == 0 {
            return Ok(false);
        }
        conn.execute(
            "UPDATE trips SET kept = true, updated_at = current_timestamp
             WHERE account_id = ? AND name_key = ?",
            params![account_id, key],
        )?;
        Ok(true)
    }

    /// Adds a flight leg. It lands where its date puts it — every write
    /// ends in `reorder_items`, so there is no position to pass — and a leg
    /// sharing a day with another sorts by kind and then behind the legs
    /// already on it, until a chosen option gives it a time of day.
    pub fn add_flight(
        &self,
        trip_id: i64,
        origin: &str,
        destination: &str,
        date: &str,
    ) -> Result<Trip> {
        let conn = self.conn();
        add_flight_within(&conn, trip_id, origin, destination, date)
    }

    /// `add_flight`, but a trip that is gone reads as `None` rather than an
    /// error.
    ///
    /// For a caller drawing from a copy of the trip that may be older than
    /// the trip — a browser tab — a trip deleted underneath it is the same
    /// failure as a stale remove: its picture is out of date, and the
    /// answer is to re-read, not to apologise. It needs to tell that apart
    /// from a real fault, and matching on the text of an error message
    /// would break silently the first time somebody reworded it.
    pub fn add_flight_checked(
        &self,
        trip_id: i64,
        origin: &str,
        destination: &str,
        date: &str,
    ) -> Result<Option<Trip>> {
        let conn = self.conn();
        // Checked here as well as inside `add_flight_within`: this one turns
        // "no such trip" into `None` before the write path is entered, so
        // nothing has to read the text of an error to tell it apart.
        if !trip_exists(&conn, trip_id)? {
            return Ok(None);
        }
        add_flight_within(&conn, trip_id, origin, destination, date).map(Some)
    }

    /// Adds a stay, an activity or a transport booking. Flights carry a
    /// route and candidates and come through `add_flight`, so a flight here
    /// is a caller's mistake rather than a second way in.
    pub fn add_item(&self, trip_id: i64, item: NewItem) -> Result<Trip> {
        if item.kind == "flight" {
            anyhow::bail!("flights go through add_flight");
        }
        // The page has a card for each of these and nothing else; an item
        // of some other kind would be stored and never drawn.
        if !matches!(item.kind.as_str(), "stay" | "activity" | "transport") {
            anyhow::bail!("kind must be stay, activity or transport, not {:?}", item.kind);
        }
        let conn = self.conn();
        if !trip_exists(&conn, trip_id)? {
            anyhow::bail!("no such trip");
        }
        conn.execute(
            "INSERT INTO trip_items (
                 trip_id, position, kind, title, place, date, starts_at, ends_at,
                 notes, booked, confirmation_code, price, currency, arrival_id)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                trip_id,
                next_position(&conn, trip_id)?,
                item.kind,
                item.title,
                item.place,
                item.date,
                item.starts_at,
                item.ends_at,
                item.notes,
                item.booked,
                item.confirmation_code,
                item.price,
                item.currency,
                item.arrival_id
            ],
        )?;
        reorder_items(&conn, trip_id)?;
        touch(&conn, trip_id)?;
        load_trip(&conn, trip_id)
    }

    /// Marks an item booked with what the confirmation said. Exists for
    /// flights: `add_flight` builds a leg with no booking fields, because a
    /// leg is normally planned before it is bought, and a forwarded ticket
    /// is the one case where the buying came first. The caller has proven
    /// the item is theirs, as `attach_to_item` demands.
    pub fn book_item(
        &self,
        item_id: i64,
        confirmation_code: Option<&str>,
        price: Option<f64>,
        currency: Option<&str>,
        arrival_id: Option<i64>,
    ) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trip_items SET booked = true, confirmation_code = ?, price = ?, currency = ?, arrival_id = ?
             WHERE id = ?",
            params![confirmation_code, price, currency, arrival_id, item_id],
        )?;
        Ok(())
    }

    /// Changes where or when one flight goes, leaving the rest alone.
    ///
    /// Returns the trip and how many parked options were dropped by the
    /// change. They have to go: an option is a flight on a particular route
    /// and day, and `add_candidate` would refuse to attach it to these
    /// values now for exactly that reason. Reporting the count is what stops
    /// them vanishing silently.
    ///
    /// A change that changes nothing keeps them — restating a date must not
    /// cost the traveller their shortlist.
    /// What an edit asks to change on an item. `None` leaves the field as
    /// it is; `Some("")` clears it, which is how a place typed by mistake
    /// or a time that turned out not to be fixed comes off again.
    ///
    /// `title` is the exception: an item is drawn by its title, so there
    /// is nothing for a blank one to mean. The tool refuses it before it
    /// reaches here.
    pub fn update_item(&self, trip_id: i64, position: i64, edit: ItemEdit<'_>) -> Result<(Trip, bool)> {
        let conn = self.conn();
        let Some((item_id, kind)) = item_at(&conn, trip_id, position)? else {
            anyhow::bail!("this trip has no item {position}");
        };
        // A leg's date and route belong to `update_flight`, which also
        // drops the options quoted for the old ones. Changing them here
        // would leave a flight described as one journey and priced as
        // another.
        if kind == "flight" {
            anyhow::bail!("item {position} is a flight; its date and route go through update_flight");
        }
        let (title, place, date, starts_at, ends_at, booked, code) = conn.query_row(
            "SELECT title, place, date, starts_at, ends_at, booked, confirmation_code
             FROM trip_items WHERE id = ?",
            params![item_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, bool>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )?;
        let keep = |asked: Option<&str>, current: Option<String>| -> Option<String> {
            match asked {
                Some("") => None,
                Some(value) => Some(value.to_string()),
                None => current,
            }
        };
        let wanted_title = edit.title.unwrap_or(&title).to_string();
        let wanted_date = edit.date.unwrap_or(&date).to_string();
        let wanted_place = keep(edit.place, place.clone());
        let wanted_ends = keep(edit.end_date, ends_at.clone());
        // The clock hangs off the item's own day, so moving the day moves
        // the time with it: a lunch at 14:30 on the 24th that becomes the
        // 20th is still at 14:30, and a `starts_at` left on the old date
        // would sort the item by a day it is no longer on.
        let wanted_time = keep(edit.time, starts_at.as_deref().map(|at| at[11..16].to_string()));
        let wanted_starts = wanted_time.map(|clock| format!("{wanted_date}T{clock}:00"));
        let wanted_booked = edit.booked.unwrap_or(booked);
        let wanted_code = keep(edit.confirmation_code, code.clone());
        if (&wanted_title, &wanted_place, &wanted_date, &wanted_starts, &wanted_ends, wanted_booked, &wanted_code)
            == (&title, &place, &date, &starts_at, &ends_at, booked, &code)
        {
            // Asking for what is already true is not a failure, and the
            // caller has to be able to tell the two apart — as on a leg.
            return Ok((load_trip(&conn, trip_id)?, false));
        }
        conn.execute(
            "UPDATE trip_items SET title = ?, place = ?, date = ?, starts_at = ?, ends_at = ?,
                 booked = ?, confirmation_code = ?, updated_at = current_timestamp
             WHERE id = ?",
            params![
                wanted_title,
                wanted_place,
                wanted_date,
                wanted_starts,
                wanted_ends,
                wanted_booked,
                wanted_code,
                item_id
            ],
        )?;
        // Positions follow dates on every write, and a date is one of the
        // things this changes.
        reorder_items(&conn, trip_id)?;
        touch(&conn, trip_id)?;
        Ok((load_trip(&conn, trip_id)?, true))
    }

    pub fn update_flight(
        &self,
        trip_id: i64,
        position: i64,
        origin: Option<&str>,
        destination: Option<&str>,
        date: Option<&str>,
    ) -> Result<(Trip, usize, bool)> {
        let conn = self.conn();
        let Some((item_id, kind)) = item_at(&conn, trip_id, position)? else {
            anyhow::bail!("this trip has no segment {position}");
        };
        if kind != "flight" {
            anyhow::bail!("segment {position} is a {kind}, not a flight");
        }
        let current = conn.query_row(
            "SELECT origin, destination, date FROM trip_items WHERE id = ?",
            params![item_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, String>(2)?,
                ))
            },
        )?;

        let wanted = (
            origin.unwrap_or(&current.0).to_string(),
            destination.unwrap_or(&current.1).to_string(),
            date.unwrap_or(&current.2).to_string(),
        );
        if wanted == current {
            // Asking for what is already true is not a failure, and the
            // caller has to be able to tell the two apart.
            return Ok((load_trip(&conn, trip_id)?, 0, false));
        }

        conn.execute(
            "UPDATE trip_items SET origin = ?, destination = ?, date = ?, title = ?,
                 updated_at = current_timestamp
             WHERE id = ?",
            params![wanted.0, wanted.1, wanted.2, format!("{} → {}", wanted.0, wanted.1), item_id],
        )?;
        let dropped =
            conn.execute("DELETE FROM item_candidates WHERE item_id = ?", params![item_id])?;
        reorder_items(&conn, trip_id)?;
        touch(&conn, trip_id)?;
        Ok((load_trip(&conn, trip_id)?, dropped, true))
    }

    pub fn drop_item(&self, trip_id: i64, position: i64) -> Result<Trip> {
        let conn = self.conn();
        let Some((item_id, _)) = item_at(&conn, trip_id, position)? else {
            anyhow::bail!("this trip has no segment {position}");
        };
        remove_item_within(&conn, trip_id, item_id)
    }

    /// Removes an item only if it is still the one the caller read.
    ///
    /// Returns whether it removed anything: a position that is gone and a
    /// position now holding a different item are both `false`, because both
    /// mean the caller's picture of the trip is stale, and neither is an
    /// error worth a log line — they are what a second browser tab looks
    /// like from here.
    ///
    /// **The single `self.conn()` is the guard, not the comparison.**
    /// Every write renumbers, so between a check and a write under two
    /// separate acquisitions a concurrent edit can slide a different item
    /// into `position` and this method would delete it. That is why the
    /// check below and `remove_item_within` share this one `conn`, and why
    /// `drop_item` is not called here: calling it would release the lock
    /// and re-take it. No test in this file can catch that regression —
    /// they are single-threaded, and every one of them still passes with
    /// the lock released in between. The structure is the whole protection;
    /// do not "simplify" it back into a call to `drop_item`.
    pub fn remove_item_checked(
        &self,
        trip_id: i64,
        position: i64,
        expected: ExpectedItem<'_>,
    ) -> Result<bool> {
        let conn = self.conn();
        let Some(item_id) = item_still_seen(&conn, trip_id, position, expected)? else {
            return Ok(false);
        };
        remove_item_within(&conn, trip_id, item_id)?;
        Ok(true)
    }

    /// Writes one item's note, or clears it when `note` is `None` — the
    /// traveller's own words about this booking, which is where a map link
    /// or a "ask for the terrace" lives. Nothing here reads it.
    ///
    /// Returns the trip and whether anything changed, the way
    /// `update_flight` does: a note that already says exactly this is not a
    /// failure, and a caller that has to tell the traveller what it did has
    /// to be able to tell the two apart.
    ///
    /// The lookup and the write share the one `self.conn()` for the reason
    /// Held, or not, on the item the caller still says it is looking at.
    /// `note_item_checked`'s guard, for the same reason.
    pub fn hold_item_checked(
        &self,
        trip_id: i64,
        position: i64,
        expected: ExpectedItem<'_>,
        held: bool,
    ) -> Result<bool> {
        let conn = self.conn();
        let Some(item_id) = item_still_seen(&conn, trip_id, position, expected)? else {
            return Ok(false);
        };
        conn.execute(
            "UPDATE trip_items SET booked = ?, updated_at = current_timestamp WHERE id = ?",
            params![held, item_id],
        )?;
        conn.execute("UPDATE trips SET updated_at = current_timestamp WHERE id = ?", params![trip_id])?;
        Ok(true)
    }

    /// The traveller's note, but only on the item the caller still says it
    /// is looking at — `remove_item_checked`'s guard, and worth more here
    /// rather than less. A stale tab that removes the wrong item shows the
    /// traveller something missing; one that writes a map link onto the
    /// wrong item shows them nothing at all, and the mistake keeps.
    ///
    /// `false` is "that is not the item you drew", which the caller answers
    /// by re-reading, exactly as a refused removal does.
    pub fn note_item_checked(
        &self,
        trip_id: i64,
        position: i64,
        expected: ExpectedItem<'_>,
        note: Option<&str>,
    ) -> Result<bool> {
        let conn = self.conn();
        let Some(item_id) = item_still_seen(&conn, trip_id, position, expected)? else {
            return Ok(false);
        };
        note_item_within(&conn, trip_id, item_id, note)?;
        Ok(true)
    }

    /// `remove_item_checked` spells out: positions are recomputed on every
    /// write, so an id read under one acquisition and written under the
    /// next can be a different item by then.
    ///
    /// The note's length is capped at both doors — the model's tool and the
    /// browser's route — through `tools::trips::note_text`, and not here,
    /// the same division dates and airport codes already follow.
    pub fn note_item(
        &self,
        trip_id: i64,
        position: i64,
        note: Option<&str>,
    ) -> Result<(Trip, bool)> {
        let conn = self.conn();
        let Some((item_id, _)) = item_at(&conn, trip_id, position)? else {
            anyhow::bail!("this trip has no segment {position}");
        };
        note_item_within(&conn, trip_id, item_id, note)
    }

    /// Parks a flight against a flight item. `decided` also marks it
    /// chosen, so the common single-option path is one call.
    ///
    /// `expected` is checked against the item's row in this same lock
    /// acquisition, not against a `Trip` the caller read earlier: that read
    /// and this write are two separate lock acquisitions, so a concurrent
    /// write renumbering positions in between could otherwise land a
    /// candidate validated against one route onto an item that is now
    /// something else. The caller passes what it validated; this re-checks
    /// that it is still true.
    ///
    /// Candidate numbers are never reused: they are what the traveller sees and
    /// what `choose_candidate` takes, and recycling one would silently retarget
    /// a decision made against the old numbering.
    pub fn add_candidate(
        &self,
        trip_id: i64,
        position: i64,
        expected: ExpectedItem,
        new: NewCandidate,
        decided: bool,
    ) -> Result<Trip> {
        let conn = self.conn();
        // Distinguished from the item check below: "no such trip" and
        // "this trip has no segment N" point the caller at different fixes.
        if !trip_exists(&conn, trip_id)? {
            anyhow::bail!("no such trip");
        }
        let Some((item_id, kind)) = item_at(&conn, trip_id, position)? else {
            anyhow::bail!("this trip has no segment {position}");
        };
        if kind != "flight" {
            anyhow::bail!("segment {position} is a {kind}; options go on flights");
        }
        let (origin, destination, title, date) = conn.query_row(
            "SELECT origin, destination, title, date FROM trip_items WHERE id = ?",
            params![item_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )?;
        // The route and date guard: nothing else stops a flight validated
        // against one item being written to a different one by the time
        // this lock is taken.
        if expected.origin.is_some_and(|o| o != origin)
            || expected.destination.is_some_and(|d| d != destination)
        {
            anyhow::bail!(
                "segment {position} is {origin}→{destination} but that flight is \
                 {}→{}",
                expected.origin.unwrap_or("?"),
                expected.destination.unwrap_or("?")
            );
        }
        if expected.title.is_some_and(|t| t != title) {
            anyhow::bail!("segment {position} is {title}, not {}", expected.title.unwrap_or("?"));
        }
        if let Some(expected_date) = expected.date {
            if date != expected_date {
                anyhow::bail!(
                    "segment {position} departs {date} but that flight departs {expected_date}"
                );
            }
        }
        // next_candidate is a high-water mark on the item row: read and
        // advance it here, inside the lock this method already holds, so a
        // dropped candidate's number is never handed to the next insert.
        let next: i64 = conn.query_row(
            "SELECT next_candidate FROM trip_items WHERE id = ?",
            params![item_id],
            |row| row.get(0),
        )?;
        conn.execute(
            "UPDATE trip_items SET next_candidate = next_candidate + 1 WHERE id = ?",
            params![item_id],
        )?;
        conn.execute(
            "INSERT INTO item_candidates (
                 item_id, candidate, chosen, airline, flight_numbers, itinerary,
                 departing_at_local, arriving_at_local, duration_minutes,
                 quoted_price, quoted_currency, quoted_at, source)
             VALUES (?, ?, false, ?, ?, ?, ?, ?, ?, ?, ?, current_timestamp, ?)",
            params![
                item_id,
                next,
                new.airline,
                new.flight_numbers,
                new.itinerary,
                new.departing_at_local,
                new.arriving_at_local,
                new.duration_minutes,
                new.quoted_price,
                new.quoted_currency,
                new.source
            ],
        )?;
        if decided {
            choose_within(&conn, item_id, next)?;
        }
        // A chosen departure gives the flight a time of day, which can move
        // it past an item that shares its date.
        reorder_items(&conn, trip_id)?;
        touch(&conn, trip_id)?;
        load_trip(&conn, trip_id)
    }

    pub fn choose_candidate(&self, trip_id: i64, position: i64, candidate: i64) -> Result<Trip> {
        let conn = self.conn();
        // Checked first, separately: a trip gone by the time this runs
        // reads as "no such trip" rather than a bad position or option
        // number — a deleted trip has no items, which from the item lookup
        // alone is indistinguishable from a numbering mistake on a trip
        // that still exists.
        if !trip_exists(&conn, trip_id)? {
            anyhow::bail!("no such trip");
        }
        let Some((item_id, kind)) = item_at(&conn, trip_id, position)? else {
            anyhow::bail!("this trip has no segment {position}");
        };
        if kind != "flight" {
            anyhow::bail!("segment {position} is a {kind}; options go on flights");
        }
        choose_within(&conn, item_id, candidate)?;
        reorder_items(&conn, trip_id)?;
        touch(&conn, trip_id)?;
        load_trip(&conn, trip_id)
    }

    /// Chooses a candidate by the trip name a traveller knows, scoped to the
    /// account proved by the caller. Unlike `choose_candidate`, this never
    /// exposes or accepts the database id that the model-facing trip type
    /// deliberately hides.
    pub fn choose_candidate_for_account(
        &self,
        account_id: i64,
        trip_name: &str,
        position: i64,
        candidate: i64,
    ) -> Result<CandidateChoice> {
        let key = trip_name.trim().to_lowercase();
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id FROM trips WHERE account_id = ? AND name_key = ?")?;
        let id: Option<i64> = stmt
            .query_map(params![account_id, key], |row| row.get(0))?
            .next()
            .transpose()?;
        drop(stmt);
        let Some(id) = id else {
            return Ok(CandidateChoice::TripNotFound);
        };
        let Some((item_id, kind)) = item_at(&conn, id, position)? else {
            return Ok(CandidateChoice::CandidateNotFound);
        };
        // An error, not `CandidateNotFound`: that variant means "re-read and
        // pick again", and no re-read will put an option on a stay.
        if kind != "flight" {
            anyhow::bail!("segment {position} is a {kind}; options go on flights");
        }

        let known: i64 = conn.query_row(
            "SELECT count(*) FROM item_candidates WHERE item_id = ? AND candidate = ?",
            params![item_id, candidate],
            |row| row.get(0),
        )?;
        if known == 0 {
            return Ok(CandidateChoice::CandidateNotFound);
        }

        choose_within(&conn, item_id, candidate)?;
        reorder_items(&conn, id)?;
        touch(&conn, id)?;
        Ok(CandidateChoice::Chosen(load_trip(&conn, id)?))
    }

    pub fn drop_candidate(&self, trip_id: i64, position: i64, candidate: i64) -> Result<Trip> {
        let conn = self.conn();
        let Some((item_id, kind)) = item_at(&conn, trip_id, position)? else {
            anyhow::bail!("this trip has no segment {position}");
        };
        if kind != "flight" {
            anyhow::bail!("segment {position} is a {kind}; options go on flights");
        }
        let removed = conn.execute(
            "DELETE FROM item_candidates WHERE item_id = ? AND candidate = ?",
            params![item_id, candidate],
        )?;
        if removed == 0 {
            anyhow::bail!("segment {position} has no option {candidate}");
        }
        // Dropping the chosen option takes the flight's time of day with it.
        reorder_items(&conn, trip_id)?;
        touch(&conn, trip_id)?;
        load_trip(&conn, trip_id)
    }

    /// False when there was no such trip. Deleting something already gone is
    /// the state the caller wanted, not a failure.
    pub fn delete_trip(&self, account_id: i64, name: &str) -> Result<bool> {
        let key = name.trim().to_lowercase();
        let conn = self.conn();
        let mut stmt =
            conn.prepare("SELECT id FROM trips WHERE account_id = ? AND name_key = ?")?;
        let mut ids = stmt.query_map(params![account_id, key], |row| row.get::<_, i64>(0))?;
        let Some(id) = ids.next().transpose()? else {
            return Ok(false);
        };
        drop(ids);
        drop(stmt);
        // Children before parents, or the subquery finds nothing.
        conn.execute(
            "DELETE FROM item_candidates WHERE item_id IN
                 (SELECT id FROM trip_items WHERE trip_id = ?)",
            params![id],
        )?;
        // Every item at once, which is the same rule `remove_item_within`
        // applies to one: a ticket goes back to its mail if that mail is
        // still there, and goes with the item if it is not.
        release_attachments_within(&conn, &item_ids_of(&conn, id)?)?;
        conn.execute("DELETE FROM trip_items WHERE trip_id = ?", params![id])?;
        conn.execute("DELETE FROM trips WHERE id = ?", params![id])?;
        Ok(true)
    }
}

fn trip_exists(conn: &Connection, trip_id: i64) -> Result<bool> {
    let known: i64 =
        conn.query_row("SELECT count(*) FROM trips WHERE id = ?", params![trip_id], |row| {
            row.get(0)
        })?;
    Ok(known > 0)
}

/// The item a traveller-facing position names right now: its id and kind.
/// Positions are recomputed on every write, so the id is what the write
/// that follows must key on — under the same lock as this lookup.
fn item_at(conn: &Connection, trip_id: i64, position: i64) -> Result<Option<(i64, String)>> {
    let mut stmt =
        conn.prepare("SELECT id, kind FROM trip_items WHERE trip_id = ? AND position = ?")?;
    let found = stmt
        .query_map(params![trip_id, position], |r| Ok((r.get(0)?, r.get(1)?)))?
        .next()
        .transpose()?;
    Ok(found)
}

/// The id of the item at `position`, but only while it is still the item
/// the caller says it saw. `None` means the caller's picture is stale —
/// nothing there any more, or something else there now.
///
/// Takes `&Connection` so the check and the write it guards happen under
/// one acquisition of the store's non-reentrant mutex: every write
/// renumbers, so a check and a write under two acquisitions can be about
/// two different items. Shared by `remove_item_checked` and
/// `note_item_checked` so a stale tab means the same thing to both.
fn item_still_seen(
    conn: &Connection,
    trip_id: i64,
    position: i64,
    expected: ExpectedItem<'_>,
) -> Result<Option<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id, origin, destination, title, date FROM trip_items
         WHERE trip_id = ? AND position = ?",
    )?;
    /// What the row says the item is, for comparing with what the caller
    /// saw.
    struct Seen {
        id: i64,
        origin: Option<String>,
        destination: Option<String>,
        title: String,
        date: String,
    }
    let item: Option<Seen> = stmt
        .query_map(params![trip_id, position], |r| {
            Ok(Seen {
                id: r.get(0)?,
                origin: r.get(1)?,
                destination: r.get(2)?,
                title: r.get(3)?,
                date: r.get(4)?,
            })
        })?
        .next()
        .transpose()?;
    drop(stmt);
    let Some(item) = item else {
        return Ok(None);
    };
    // `None` is "nothing to verify", not "verified" — the same reading
    // `add_candidate` gives these fields, so a caller that has only a
    // route to go on is not quietly granted a free pass on the date.
    let seen =
        |expected: Option<&str>, actual: Option<&str>| expected.is_none_or(|e| actual == Some(e));
    if !seen(expected.origin, item.origin.as_deref())
        || !seen(expected.destination, item.destination.as_deref())
        || !seen(expected.title, Some(&item.title))
        || !seen(expected.date, Some(&item.date))
    {
        return Ok(None);
    }
    Ok(Some(item.id))
}

/// Clears the item's flags and sets one. "At most one chosen" cannot be a
/// `UNIQUE` constraint because `false` repeats, so it is this function's
/// job — and the caller always holds the connection lock, which is what
/// makes the pair of statements indivisible.
fn choose_within(conn: &Connection, item_id: i64, candidate: i64) -> Result<()> {
    let known: i64 = conn.query_row(
        "SELECT count(*) FROM item_candidates WHERE item_id = ? AND candidate = ?",
        params![item_id, candidate],
        |row| row.get(0),
    )?;
    if known == 0 {
        // Worded by position, which is the name the traveller knows the
        // item by; only the error path pays for the lookup.
        let position: i64 = conn.query_row(
            "SELECT position FROM trip_items WHERE id = ?",
            params![item_id],
            |row| row.get(0),
        )?;
        anyhow::bail!("segment {position} has no option {candidate}");
    }
    conn.execute(
        "UPDATE item_candidates SET chosen = false WHERE item_id = ?",
        params![item_id],
    )?;
    conn.execute(
        "UPDATE item_candidates SET chosen = true WHERE item_id = ? AND candidate = ?",
        params![item_id, candidate],
    )?;
    Ok(())
}

/// Marks a trip edited. Status goes back to `planning` because whatever it
/// was priced at no longer describes it.
fn touch(conn: &Connection, trip_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE trips SET status = 'planning', updated_at = current_timestamp WHERE id = ?",
        params![trip_id],
    )?;
    Ok(())
}

/// Releases the trips owned by these conversations without touching the
/// trips themselves.
///
/// The distinction this draws is the whole point of the feature: a thread
/// that a *timer* removed must not take a travel plan with it. A trip is
/// built over weeks and a thread expires after two days of quiet, so a
/// cascade here would delete travel plans on a schedule, with no button
/// pressed, nothing to undo and nothing in any log. Only
/// `delete_conversation` cascades, because there somebody pressed Delete.
/// An orphaned trip is an ordinary state; the next chat to touch it adopts
/// it.
///
/// Takes a `&Connection` rather than `&Store` because its caller,
/// `expire_conversations`, is already inside its own transaction and
/// already holds the lock — re-locking would deadlock, and a release that
/// could commit separately from the delete would leave a trip pointing at
/// a conversation that is gone.
fn detach_trips_within(conn: &Connection, conversation_ids: &[i64]) -> Result<usize> {
    // `IN ()` is a parser error, not an empty set — the same trap
    // `expire_conversations` documents. The caller passes a SELECT result
    // that is empty on every sweep that expires nothing, which is the
    // ordinary hourly case rather than an edge case.
    if conversation_ids.is_empty() {
        return Ok(0);
    }
    let holes = ["?"].repeat(conversation_ids.len()).join(", ");
    Ok(conn.execute(
        &format!("UPDATE trips SET conversation_id = NULL WHERE conversation_id IN ({holes})"),
        duckdb::params_from_iter(conversation_ids.iter()),
    )?)
}

/// Removes the unkept drafts owned by these conversations, with their
/// items and parked options.
///
/// Children before parents, or the subquery finds nothing. Guards the empty
/// slice for the same reason `detach_trips_within` does: `IN ()` is a parser
/// error, and the ordinary hourly sweep expires nothing.
fn delete_drafts_within(conn: &Connection, conversation_ids: &[i64]) -> Result<usize> {
    if conversation_ids.is_empty() {
        return Ok(0);
    }
    let holes = ["?"].repeat(conversation_ids.len()).join(", ");
    let doomed =
        format!("(SELECT id FROM trips WHERE NOT kept AND conversation_id IN ({holes}))");
    conn.execute(
        &format!(
            "DELETE FROM item_candidates WHERE item_id IN
                 (SELECT id FROM trip_items WHERE trip_id IN {doomed})"
        ),
        duckdb::params_from_iter(conversation_ids.iter()),
    )?;
    // Nothing reaches this today — a draft is kept the moment a booking
    // lands on it, so a draft holding a ticket is a state only a future
    // edit could produce. It follows the rule anyway, because this delete
    // runs on a timer and a difference discovered later would be
    // discovered as rows nobody can account for.
    let mut stmt =
        conn.prepare(&format!("SELECT id FROM trip_items WHERE trip_id IN {doomed}"))?;
    let items: Vec<i64> = stmt
        .query_map(duckdb::params_from_iter(conversation_ids.iter()), |r| r.get(0))?
        .collect::<duckdb::Result<_>>()?;
    drop(stmt);
    release_attachments_within(conn, &items)?;
    conn.execute(
        &format!("DELETE FROM trip_items WHERE trip_id IN {doomed}"),
        duckdb::params_from_iter(conversation_ids.iter()),
    )?;
    Ok(conn.execute(
        &format!("DELETE FROM trips WHERE NOT kept AND conversation_id IN ({holes})"),
        duckdb::params_from_iter(conversation_ids.iter()),
    )?)
}

/// Where a new item goes before `reorder_items` has looked at it: behind
/// everything already there. The date puts it right; the position only
/// breaks ties, and a leg sharing its day with legs already on it should
/// land behind them — the traveller typing legs in order expects it, and
/// there is nothing better to break that tie with until a chosen option
/// gives it a time of day.
fn next_position(conn: &Connection, trip_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT coalesce(max(position), 0) + 1 FROM trip_items WHERE trip_id = ?",
        params![trip_id],
        |row| row.get(0),
    )?)
}

/// Adds a flight leg and lets `reorder_items` put it where its date
/// belongs. Shared by `add_flight` and `add_flight_checked`, whose only
/// disagreement is what a missing trip means.
fn add_flight_within(
    conn: &Connection,
    trip_id: i64,
    origin: &str,
    destination: &str,
    date: &str,
) -> Result<Trip> {
    // Checked before anything is written: without this, a bad trip_id
    // would reach the INSERT below and leave an item row for a trip that
    // does not exist — and no read path can ever find it again, because
    // every read goes through a trip.
    if !trip_exists(conn, trip_id)? {
        anyhow::bail!("no such trip");
    }
    conn.execute(
        "INSERT INTO trip_items (trip_id, position, kind, title, origin, destination, date)
         VALUES (?, ?, 'flight', ?, ?, ?, ?)",
        params![
            trip_id,
            next_position(conn, trip_id)?,
            format!("{origin} → {destination}"),
            origin,
            destination,
            date
        ],
    )?;
    reorder_items(conn, trip_id)?;
    touch(conn, trip_id)?;
    load_trip(conn, trip_id)
}

/// Removes an item by id, with its options, and renumbers what is left.
///
/// Takes `&Connection` rather than `&Store` so a caller that has already
/// checked something about the item can do the check and this delete
/// under one acquisition of the store's non-reentrant mutex — see
/// `remove_item_checked`, whose correctness is exactly that. Re-locking
/// would deadlock; releasing and re-locking would silently reintroduce the
/// race the check exists to close.
fn remove_item_within(conn: &Connection, trip_id: i64, item_id: i64) -> Result<Trip> {
    conn.execute("DELETE FROM item_candidates WHERE item_id = ?", params![item_id])?;
    release_attachments_within(conn, &[item_id])?;
    conn.execute("DELETE FROM trip_items WHERE id = ?", params![item_id])?;
    reorder_items(conn, trip_id)?;
    touch(conn, trip_id)?;
    load_trip(conn, trip_id)
}

/// Writes an item's note by id, and says whether that changed anything.
///
/// Takes `&Connection` for the reason `remove_item_within` does: the
/// caller has already turned a position into this id and must not release
/// the lock in between.
///
/// A blank note is no note. Both doors trim before they get here, but the
/// normalisation is repeated rather than assumed, so that clearing a note
/// and never having written one are the same row for every caller —
/// including the tests and anything added later that reaches the store
/// directly.
///
/// No `reorder_items`: nothing a note touches sorts the timeline, so the
/// positions the caller is holding stay the positions it drew. No `touch`
/// either — that would put a finalised trip back to `planning`, and a note
/// changes nothing that was priced. The trip's `updated_at` does move: the
/// traveller edited this trip, and the list they see is ordered by it.
fn note_item_within(
    conn: &Connection,
    trip_id: i64,
    item_id: i64,
    note: Option<&str>,
) -> Result<(Trip, bool)> {
    let note = note.map(str::trim).filter(|note| !note.is_empty());
    let current: Option<String> = conn.query_row(
        "SELECT notes FROM trip_items WHERE id = ?",
        params![item_id],
        |row| row.get(0),
    )?;
    if current.as_deref() == note {
        return Ok((load_trip(conn, trip_id)?, false));
    }
    conn.execute(
        "UPDATE trip_items SET notes = ?, updated_at = current_timestamp WHERE id = ?",
        params![note, item_id],
    )?;
    conn.execute(
        "UPDATE trips SET updated_at = current_timestamp WHERE id = ?",
        params![trip_id],
    )?;
    Ok((load_trip(conn, trip_id)?, true))
}

/// What becomes of the files these items are carrying, called just before
/// the items go. An attachment has no owner of its own — `attachment_owner`
/// finds one through the mail it came with, or through the trip behind its
/// item — so deleting an item without deciding this leaves rows that answer
/// with nobody: undownloadable, and unreachable by both mail sweeps, whose
/// deletes are keyed on mail ids that by then no longer exist.
///
/// The mail decides it. While the mail is still there the file goes back to
/// being loose: a leg coming off a trip must not destroy the traveller's
/// ticket, which is still listed under Other mail and will be swept with
/// that mail at thirty days like any other loose file. Once the mail has
/// gone the item was the last thing that could reach those bytes, and
/// keeping a stranger's PDF that nobody can see, download or delete is a
/// liability rather than a kindness — so it goes with the item.
///
/// `attachments.mail_id` is `NOT NULL` and keeps pointing at a mail that
/// has been deleted, so "the mail is still there" is this join and never a
/// null check. The residual risk is the obvious one: a file whose mail has
/// already been swept and whose item is removed by accident is gone for
/// good, with no undo. That is the same bargain the inbox sweep already
/// made when it let the mail go.
///
/// Takes `&Connection` for the reason `remove_item_within` documents at
/// length: its callers hold the store's non-reentrant mutex already.
fn release_attachments_within(conn: &Connection, item_ids: &[i64]) -> Result<()> {
    // `IN ()` is a parser error rather than an empty set — the trap
    // `detach_trips_within` documents.
    if item_ids.is_empty() {
        return Ok(());
    }
    let holes = ["?"].repeat(item_ids.len()).join(", ");
    // The orphans first, so the hand-back below needs no condition of its
    // own: what is left on these items after this delete is exactly the
    // files whose mail is still there.
    conn.execute(
        &format!(
            "DELETE FROM attachments WHERE item_id IN ({holes})
               AND NOT EXISTS (SELECT 1 FROM inbound_mail m WHERE m.id = attachments.mail_id)"
        ),
        duckdb::params_from_iter(item_ids.iter()),
    )?;
    conn.execute(
        &format!("UPDATE attachments SET item_id = NULL WHERE item_id IN ({holes})"),
        duckdb::params_from_iter(item_ids.iter()),
    )?;
    Ok(())
}

/// The ids of every item on a trip, for the callers that are about to
/// delete all of them at once and have to decide about their files first.
fn item_ids_of(conn: &Connection, trip_id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM trip_items WHERE trip_id = ?")?;
    let ids = stmt.query_map(params![trip_id], |r| r.get(0))?.collect::<duckdb::Result<_>>()?;
    Ok(ids)
}

/// Recomputes positions for one trip: by date, then a start time (an item
/// with one sorts before an item without on the same day; a flight's is its
/// chosen option's departure), then kind — flight, transport, stay,
/// activity — then the previous position, so two items nothing else
/// separates keep their order. Called inside every write, under the lock
/// the caller already holds.
///
/// The chosen option is joined through a GROUP BY, not the raw table: "at
/// most one chosen" is enforced in Rust, and a duplicate slipping past it
/// would otherwise number the item twice and leave a position vacant.
fn reorder_items(conn: &Connection, trip_id: i64) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT i.id FROM trip_items i
         LEFT JOIN (SELECT item_id, min(departing_at_local) AS departing_at_local
                    FROM item_candidates WHERE chosen GROUP BY item_id) c ON c.item_id = i.id
         WHERE i.trip_id = ?
         ORDER BY i.date,
                  COALESCE(i.starts_at, c.departing_at_local) IS NULL,
                  COALESCE(i.starts_at, c.departing_at_local),
                  CASE i.kind WHEN 'flight' THEN 0 WHEN 'transport' THEN 1 WHEN 'stay' THEN 2 ELSE 3 END,
                  i.position, i.id",
    )?;
    let ids: Vec<i64> =
        stmt.query_map(params![trip_id], |r| r.get(0))?.collect::<duckdb::Result<_>>()?;
    drop(stmt);
    for (n, id) in ids.iter().enumerate() {
        conn.execute("UPDATE trip_items SET position = ? WHERE id = ?", params![n as i64 + 1, id])?;
    }
    Ok(())
}

/// Reads one whole trip. Takes `&Connection` rather than `&Store` so it can
/// be called by a method that already holds the lock — every trip-mutating
/// method returns the trip it just changed, and re-locking would deadlock.
fn load_trip(conn: &Connection, id: i64) -> Result<Trip> {
    let (name, adults, cabin_class, status, kept) = conn.query_row(
        "SELECT name, adults, cabin_class, status, kept FROM trips WHERE id = ?",
        params![id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, bool>(4)?,
            ))
        },
    )?;

    let mut stmt = conn.prepare(
        "SELECT id, position, kind, title, place, origin, destination, date, starts_at, ends_at,
                booked, confirmation_code, price, currency, notes, arrival_id
         FROM trip_items WHERE trip_id = ? ORDER BY position",
    )?;
    let rows: Vec<TripItem> = stmt
        .query_map(params![id], |r| {
            Ok(TripItem {
                id: r.get(0)?,
                position: r.get(1)?,
                kind: r.get(2)?,
                title: r.get(3)?,
                place: r.get(4)?,
                origin: r.get(5)?,
                destination: r.get(6)?,
                date: r.get(7)?,
                starts_at: r.get(8)?,
                ends_at: r.get(9)?,
                booked: r.get(10)?,
                confirmation_code: r.get(11)?,
                price: r.get(12)?,
                currency: r.get(13)?,
                notes: r.get(14)?,
                arrival_id: r.get(15)?,
                candidates: Vec::new(),
                attachments: Vec::new(),
            })
        })?
        .collect::<duckdb::Result<_>>()?;

    let mut stmt = conn.prepare(
        "SELECT item_id, candidate, chosen, airline, flight_numbers, itinerary,
                departing_at_local, arriving_at_local, duration_minutes,
                quoted_price, quoted_currency, source
         FROM item_candidates
         WHERE item_id IN (SELECT id FROM trip_items WHERE trip_id = ?)
         ORDER BY item_id, candidate",
    )?;
    let candidates: Vec<(i64, TripCandidate)> = stmt
        .query_map(params![id], |r| {
            Ok((
                r.get(0)?,
                TripCandidate {
                    candidate: r.get(1)?,
                    chosen: r.get(2)?,
                    airline: r.get(3)?,
                    flight_numbers: r.get(4)?,
                    itinerary: r.get(5)?,
                    departing_at_local: r.get(6)?,
                    arriving_at_local: r.get(7)?,
                    duration_minutes: r.get(8)?,
                    quoted_price: r.get(9)?,
                    quoted_currency: r.get(10)?,
                    source: r.get(11)?,
                },
            ))
        })?
        .collect::<duckdb::Result<_>>()?;

    // Every ticket on the trip in one query, joined to its item in memory
    // below — the same shape as the candidates above, and for a harder
    // reason. `list_trips` reads every trip of an account with all of its
    // items, and it sits on the placement path — once per incoming mail,
    // as `inbox::Trips::load` is careful to keep it. A query per item would
    // make that read grow with the size of the account's trips rather than
    // with the number of them, which is the one thing that read must not
    // do. `item_id IN (…)` also drops a file that is still only the mail's:
    // a NULL `item_id` matches nothing.
    //
    // It names four columns and not `bytes`, which matters more than it
    // looks: this once computed each file's size with `octet_length(bytes)`,
    // and DuckDB answers that by projecting the BLOB in the scan and taking
    // the length above the join — 40 tickets of 250 KB cost ~13 ms cold and
    // ~1 ms warm per trip read, to produce a number nothing displays. See
    // `AttachmentRef` for why there is no size to fill.
    let mut stmt = conn.prepare(
        "SELECT item_id, id, filename, mime
         FROM attachments
         WHERE item_id IN (SELECT id FROM trip_items WHERE trip_id = ?)
         ORDER BY item_id, id",
    )?;
    let attachments: Vec<(i64, scout_api::AttachmentRef)> = stmt
        .query_map(params![id], |r| {
            Ok((
                r.get(0)?,
                scout_api::AttachmentRef { id: r.get(1)?, filename: r.get(2)?, mime: r.get(3)? },
            ))
        })?
        .collect::<duckdb::Result<_>>()?;

    let items = rows
        .into_iter()
        .map(|mut item| {
            item.attachments = attachments
                .iter()
                .filter(|(item_id, _)| *item_id == item.id)
                .map(|(_, a)| a.clone())
                .collect();
            item.candidates = candidates
                .iter()
                .filter(|(item_id, _)| *item_id == item.id)
                .map(|(_, c)| c.clone())
                .collect();
            // A flight's start is its chosen option's departure, read here
            // rather than kept in a second column that would have to be
            // updated in step with every choose and drop.
            if item.starts_at.is_none() {
                item.starts_at = item
                    .candidates
                    .iter()
                    .find(|c| c.chosen)
                    .and_then(|c| c.departing_at_local.clone());
            }
            item
        })
        .collect();

    Ok(Trip { id, name, adults, cabin_class, status, items, kept })
}

fn row_to_purchase(row: &Row) -> duckdb::Result<Purchase> {
    Ok(Purchase {
        id: row.get(0)?,
        item: row.get(1)?,
        store: row.get(2)?,
        url: row.get(3)?,
        price: row.get(4)?,
        currency: row.get(5)?,
        notes: row.get(6)?,
        purchased_at: row.get(7)?,
    })
}

fn row_to_reminder(row: &Row) -> duckdb::Result<Reminder> {
    Ok(Reminder {
        id: row.get(0)?,
        account_id: row.get(1)?,
        channel: row.get(2)?,
        address: row.get(3)?,
        item: row.get(4)?,
        interval_days: row.get(5)?,
        next_due: row.get(6)?,
    })
}

/// The select `arrival_row` reads — columns, FROM and joins — so the two
/// readers cannot drift. The trip join is owner-scoped: a `trip_id` that
/// names someone else's trip yields no name rather than theirs.
const ARRIVAL_SELECT: &str =
    "a.id, a.mail_id, a.booking, a.kind, a.title, a.place, a.origin, a.destination,
     a.airline, a.flight_number, a.stops, a.date,
     a.starts_at, a.ends_at, a.confirmation_code, a.price, a.currency, a.confidence, a.summary,
     a.trip_id, t.name, a.status, strftime(m.received_at, '%Y-%m-%dT%H:%M:%SZ')
     FROM arrivals a
     JOIN inbound_mail m ON m.id = a.mail_id
     LEFT JOIN trips t ON t.id = a.trip_id AND t.account_id = a.account_id";

/// Everything but the attachments, which need a second query per row.
fn arrival_row(r: &Row) -> duckdb::Result<scout_api::Arrival> {
    Ok(scout_api::Arrival {
        id: r.get(0)?, mail_id: r.get(1)?, booking: r.get(2)?, kind: r.get(3)?, title: r.get(4)?,
        place: r.get(5)?, origin: r.get(6)?, destination: r.get(7)?, airline: r.get(8)?,
        flight_number: r.get(9)?, stops: listed(r.get(10)?), date: r.get(11)?,
        starts_at: r.get(12)?, ends_at: r.get(13)?, confirmation_code: r.get(14)?, price: r.get(15)?,
        currency: r.get(16)?, confidence: r.get(17)?, summary: r.get(18)?, trip_id: r.get(19)?,
        trip_name: r.get(20)?, status: r.get(21)?, received_at: r.get(22)?, attachments: Vec::new(),
    })
}

/// A comma-separated column back as the list it was written from. Blanks
/// are dropped, so a stored `""` and a NULL are the same empty list.
fn listed(stored: Option<String>) -> Vec<String> {
    stored
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn attachments_of(conn: &Connection, mail_id: i64) -> duckdb::Result<Vec<scout_api::AttachmentRef>> {
    let mut stmt = conn.prepare(
        "SELECT id, filename, mime FROM attachments WHERE mail_id = ? ORDER BY id",
    )?;
    let rows = stmt.query_map(params![mail_id], |r| {
        Ok(scout_api::AttachmentRef { id: r.get(0)?, filename: r.get(1)?, mime: r.get(2)? })
    })?;
    rows.collect()
}

/// A wall-clock cutoff computed here rather than as `TIMESTAMP - INTERVAL`
/// in SQL, for the reason `issue_login_token` documents at length.
fn days_ago(days: i64) -> String {
    (chrono::Utc::now().naive_utc() - chrono::Duration::days(days)).to_string()
}

/// `days_ago`, in minutes, for the retry spacing on inbound mail.
fn minutes_ago(minutes: i64) -> String {
    (chrono::Utc::now().naive_utc() - chrono::Duration::minutes(minutes)).to_string()
}

/// How long a mail waits after a failed attempt before it is served
/// again. A model that is down is down for a while, and three attempts
/// spent in the same second are one attempt.
pub const MAIL_RETRY_MINUTES: i64 = 5;

/// A booking that is still waiting on its owner. Only a booking waits: a
/// non-booking is born decided, so this is the one condition both the
/// inbox and the sweep read as "undecided".
const UNDECIDED: &str = "a.booking AND a.status = 'pending'";

/// Mail the worker could not read, over the alias `m`: refused outright, or
/// out of attempts while still marked `extracting` because the worker died
/// mid-call. The second is as unreadable as the first and must not be
/// mistaken for work still in progress — `inbox_view` lists both as
/// `failed`, and `delete_mail` counts both as finished with.
fn unreadable(m: &str) -> String {
    format!("({m}.status = 'failed' OR ({m}.status = 'extracting' AND {m}.attempts >= {MAIL_ATTEMPTS}))")
}

impl Store {
    /// Claims `handle` for the account; `false` when another account holds
    /// it. The check and the write share the lock, and that is the whole
    /// uniqueness guarantee — see the column's comment for why there is no
    /// index. Case is the caller's job.
    pub fn set_handle(&self, account_id: i64, handle: &str) -> Result<bool> {
        let conn = self.conn();
        let taken: Option<i64> = conn
            .query_row(
                "SELECT id FROM accounts WHERE handle = ? AND id <> ?",
                params![handle, account_id],
                |r| r.get(0),
            )
            .optional()?;
        if taken.is_some() {
            return Ok(false);
        }
        conn.execute("UPDATE accounts SET handle = ? WHERE id = ?", params![handle, account_id])?;
        Ok(true)
    }

    pub fn handle_of(&self, account_id: i64) -> Result<Option<String>> {
        let conn = self.conn();
        Ok(conn
            .query_row("SELECT handle FROM accounts WHERE id = ?", params![account_id], |r| r.get(0))
            .optional()?
            .flatten())
    }

    pub fn account_for_handle(&self, handle: &str) -> Result<Option<i64>> {
        let conn = self.conn();
        Ok(conn
            .query_row("SELECT id FROM accounts WHERE handle = ?", params![handle], |r| r.get(0))
            .optional()?)
    }

    /// Every address this person signed in with, oldest first — the first
    /// of them is where a forwarded mail goes, and the rest matter because
    /// a mail this person sent themselves may come from any of them.
    ///
    /// Ordered by the address as well as the time, so two identities
    /// linked in the same instant still come back in one order and the
    /// destination of a forward does not change from read to read.
    pub fn emails_of(&self, account_id: i64) -> Result<Vec<String>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT external_id FROM identities WHERE account_id = ? AND kind = 'email'
             ORDER BY created_at, external_id",
        )?;
        let rows = stmt.query_map(params![account_id], |r| r.get(0))?.collect::<duckdb::Result<Vec<String>>>()?;
        Ok(rows)
    }

    /// Stores a delivered mail and what the webhook said its parts are;
    /// `None` when this `provider_id` has been seen, which a provider's
    /// retry makes routine. Checked under the lock rather than caught from
    /// the UNIQUE index, because a redelivery is not an error and should
    /// not read like one — and a redelivery that returns `None` here has
    /// written no parts either, so the table grows with the mail and not
    /// with Resend's retries.
    ///
    /// The mail and its parts go in one transaction: a part whose mail is
    /// not there describes nothing, and a mail whose parts were lost would
    /// have the worker keep the decoration it was told about. Only a crash
    /// between the two statements could do either, which is exactly what a
    /// transaction is for.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_mail(
        &self,
        account_id: i64,
        provider_id: &str,
        from: &str,
        subject: Option<&str>,
        text: Option<&str>,
        html: Option<&str>,
        truncated: bool,
        parts: &[MailPart],
    ) -> Result<Option<i64>> {
        let conn = self.conn();
        let seen: Option<i64> = conn
            .query_row("SELECT id FROM inbound_mail WHERE provider_id = ?", params![provider_id], |r| r.get(0))
            .optional()?;
        if seen.is_some() {
            return Ok(None);
        }
        conn.execute_batch("BEGIN")?;
        let written = (|| -> Result<i64> {
            let id: i64 = conn.query_row(
                "INSERT INTO inbound_mail (account_id, provider_id, from_address, subject, text, html, truncated)
                 VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
                params![account_id, provider_id, from, subject, text, html, truncated],
                |r| r.get(0),
            )?;
            for part in parts {
                conn.execute(
                    "INSERT INTO mail_parts (mail_id, provider_id, content_disposition, content_id)
                     VALUES (?, ?, ?, ?)",
                    params![
                        id,
                        part.provider_id,
                        part.content_disposition.as_deref(),
                        part.content_id.as_deref()
                    ],
                )?;
            }
            Ok(id)
        })();
        match written {
            // A `COMMIT` that fails leaves the transaction open, and DuckDB
            // will not start another inside it — the next caller on this
            // connection would fail for a reason that was never theirs.
            // `delete_mail` says the same at more length.
            Ok(id) => match conn.execute_batch("COMMIT") {
                Ok(()) => Ok(Some(id)),
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e.into())
                }
            },
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// What the webhook said this mail's parts are. Empty for a mail
    /// stored before `mail_parts` existed, and for one that came with no
    /// attachments at all — the two are the same answer because there is
    /// nothing to tell them apart with, and the worker treats silence the
    /// same way either way.
    ///
    /// `ORDER BY id` so that two reads of one mail agree; no caller
    /// depends on the order, and the worker keys these by `provider_id`
    /// the moment it has them.
    pub fn mail_parts_of(&self, mail_id: i64) -> Result<Vec<MailPart>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT provider_id, content_disposition, content_id FROM mail_parts WHERE mail_id = ? ORDER BY id",
        )?;
        let rows = stmt.query_map(params![mail_id], |r| {
            Ok(MailPart { provider_id: r.get(0)?, content_disposition: r.get(1)?, content_id: r.get(2)? })
        })?;
        Ok(rows.collect::<duckdb::Result<_>>()?)
    }

    /// Mail the extractor has not finished with, oldest first so a burst
    /// is read in the order it came, and not one attempted in the last
    /// `MAIL_RETRY_MINUTES`.
    pub fn mail_to_work(&self, limit: usize) -> Result<Vec<MailToWork>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, provider_id, from_address, subject, text, html, attempts,
                    forwarded_at IS NOT NULL
             FROM inbound_mail
             WHERE status IN ('new', 'extracting') AND attempts < ?
               AND (attempted_at IS NULL OR attempted_at < ?)
             ORDER BY received_at, id LIMIT ?",
        )?;
        let rows = stmt.query_map(params![MAIL_ATTEMPTS, minutes_ago(MAIL_RETRY_MINUTES), limit as i64], |r| {
            Ok(MailToWork {
                id: r.get(0)?, account_id: r.get(1)?, provider_id: r.get(2)?, from: r.get(3)?,
                subject: r.get(4)?, text: r.get(5)?, html: r.get(6)?, attempts: r.get(7)?,
                forwarded: r.get(8)?,
            })
        })?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Counted before the model is called, not after, so a crash mid-call
    /// still spends an attempt.
    pub fn mail_attempted(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE inbound_mail SET attempts = attempts + 1, status = 'extracting', attempted_at = now() WHERE id = ?",
            params![id],
        )?;
        Ok(())
    }

    /// Hands an attempt back: the provider could not be reached, which
    /// says nothing about the mail, and a ten-minute outage must not
    /// spend a mail's three tries. `attempted_at` stands, so the next try
    /// still waits `MAIL_RETRY_MINUTES` — an outage is not over in a
    /// second, and three tries in one are one try.
    pub fn mail_unattempted(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE inbound_mail SET attempts = greatest(attempts - 1, 0) WHERE id = ?", params![id])?;
        Ok(())
    }

    /// Whether the extractor's reading of this mail is on record: the
    /// worker asks before it pays the model, so a mail that only owes its
    /// forward is not read twice.
    pub fn mail_has_arrival(&self, mail_id: i64) -> Result<bool> {
        let conn = self.conn();
        let n: i64 = conn.query_row("SELECT count(*) FROM arrivals WHERE mail_id = ?", params![mail_id], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Backdates the last attempt by an hour, so a test can walk a mail
    /// through its retries without waiting `MAIL_RETRY_MINUTES` between.
    #[doc(hidden)]
    pub fn age_attempts(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE inbound_mail SET attempted_at = ? WHERE id = ?", params![minutes_ago(60), id])?;
        Ok(())
    }

    /// Backdates a trip's creation past `DRAFT_GRACE_MINUTES`, so a test
    /// can sweep a draft the grace would otherwise spare.
    ///
    /// Written by the same bare `current_timestamp` the column's default
    /// uses, for the reason `collectable_draft` gives at length: a value
    /// from `chrono::Utc` would land hours off the rows it has to compare
    /// with, and the test would pass or fail on the host's zone.
    ///
    /// `#[cfg(test)]` rather than `#[doc(hidden)] pub` like `age_attempts`,
    /// which the web crate's tests need from outside: nothing outside this
    /// crate moves a trip's clock backwards, and a door that exists gets
    /// opened.
    #[cfg(test)]
    pub(crate) fn age_trip(&self, trip_id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE trips SET created_at =
                 CAST(current_timestamp AS TIMESTAMP) - to_minutes(CAST(? AS INTEGER))
             WHERE id = ?",
            params![DRAFT_GRACE_MINUTES + 60, trip_id],
        )?;
        Ok(())
    }

    pub fn mail_done(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE inbound_mail SET status = 'done', error = NULL WHERE id = ?", params![id])?;
        Ok(())
    }

    pub fn mail_failed(&self, id: i64, error: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE inbound_mail SET status = 'failed', error = ? WHERE id = ?", params![error, id])?;
        Ok(())
    }

    pub fn mail_forwarded(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE inbound_mail SET forwarded_at = now() WHERE id = ?", params![id])?;
        Ok(())
    }

    /// The body, fetched after the fact: the provider's webhook carries
    /// the envelope and the content is a second call. Text and html are
    /// each cut at `cap_chars` characters, independently — the model reads
    /// them, and a 4 MB newsletter is not a booking — and `truncated`
    /// records that a cut happened, here or on the way in, so the page can
    /// say the reading is of a part.
    ///
    /// The sender comes with the body, because it comes from the same
    /// call: the webhook's `from` is optional and the record's is the
    /// same header seen in full. Writing it here is what makes every
    /// later pass — and the drawn row — judge the one string the pass
    /// that fetched it judged. A blank is not an answer and never
    /// overwrites the one the row already has.
    pub fn mail_fetched(&self, id: i64, sender: Option<&str>, text: Option<&str>, html: Option<&str>, cap_chars: usize) -> Result<()> {
        let cut = |s: Option<&str>| -> (Option<String>, bool) {
            match s {
                Some(s) if s.chars().count() > cap_chars => (Some(s.chars().take(cap_chars).collect()), true),
                Some(s) => (Some(s.to_string()), false),
                None => (None, false),
            }
        };
        let (text, text_cut) = cut(text);
        let (html, html_cut) = cut(html);
        let sender = sender.map(str::trim).filter(|s| !s.is_empty());
        let conn = self.conn();
        conn.execute(
            "UPDATE inbound_mail SET from_address = COALESCE(?, from_address), text = ?, html = ?,
                    truncated = truncated OR ? WHERE id = ?",
            params![sender, text, html, text_cut || html_cut, id],
        )?;
        Ok(())
    }

    pub fn insert_attachment(
        &self,
        mail_id: i64,
        filename: &str,
        mime: &str,
        bytes: Option<&[u8]>,
        text: Option<&str>,
    ) -> Result<i64> {
        let conn = self.conn();
        Ok(conn.query_row(
            "INSERT INTO attachments (mail_id, filename, mime, bytes, text)
             VALUES (?, ?, ?, ?, ?) RETURNING id",
            params![mail_id, filename, mime, bytes, text],
            |r| r.get(0),
        )?)
    }

    /// The caller has already proven `mail_id` is theirs — like
    /// `flight_searches_all`, this and its callers are the access surface.
    pub fn attachments_of_mail(&self, mail_id: i64) -> Result<Vec<scout_api::AttachmentRef>> {
        let conn = self.conn();
        Ok(attachments_of(&conn, mail_id)?)
    }

    /// `(mail_id, item_id, filename, mime, bytes)`.
    #[allow(clippy::type_complexity)]
    pub fn attachment(&self, id: i64) -> Result<Option<(i64, Option<i64>, String, String, Option<Vec<u8>>)>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT mail_id, item_id, filename, mime, bytes FROM attachments WHERE id = ?",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?)
    }

    /// Whose file this is: an attachment has no owner of its own, so a
    /// download has to ask here. By the mail it came with while that
    /// exists, and by the trip item it joined once the sweep has taken the
    /// mail — a kept ticket must not become nobody's.
    pub fn attachment_owner(&self, id: i64) -> Result<Option<i64>> {
        let conn = self.conn();
        Ok(conn
            .query_row(
                "SELECT coalesce(m.account_id, t.account_id)
                 FROM attachments a
                 LEFT JOIN inbound_mail m ON m.id = a.mail_id
                 LEFT JOIN trip_items i ON i.id = a.item_id
                 LEFT JOIN trips t ON t.id = i.trip_id
                 WHERE a.id = ?",
                params![id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// The caller has already proven both ids are theirs (`attachment_owner`
    /// for the file, the trip for the item); nothing is checked here but
    /// that the file is still loose. A ticket belongs to the booking it
    /// came with, so the first item to claim it keeps it.
    pub fn attach_to_item(&self, attachment_id: i64, item_id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE attachments SET item_id = ? WHERE id = ? AND item_id IS NULL",
            params![item_id, attachment_id],
        )?;
        Ok(())
    }

    /// `(filename, text)` for every attachment of a mail, in the order they
    /// came: what the extractor reads alongside the body. A file with no
    /// text (an image, a PDF nobody could read) is listed with `None` so
    /// the model can still be told it exists.
    pub fn attachment_texts_of(&self, mail_id: i64) -> Result<Vec<(String, Option<String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT filename, text FROM attachments WHERE mail_id = ? ORDER BY id")?;
        let rows = stmt.query_map(params![mail_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// `(filename, bytes)` for the attachments that kept their bytes: what
    /// a forward carries along. A text-only row has nothing to send.
    pub fn attachment_bytes_of(&self, mail_id: i64) -> Result<Vec<(String, Vec<u8>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT filename, bytes FROM attachments WHERE mail_id = ? AND bytes IS NOT NULL ORDER BY id",
        )?;
        let rows = stmt.query_map(params![mail_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Every reading of one mail, written together. A retry replaces the
    /// undecided readings of that mail, so a mail worked twice shows once;
    /// readings the owner has already decided on are history and stay.
    ///
    /// The delete runs once for the batch rather than once per row: one
    /// email can confirm a round trip, and a delete per insert would leave
    /// only the last leg standing.
    ///
    /// All of it in one transaction. Half a batch would be worse than
    /// none: the worker takes any arrival on a mail as proof it has been
    /// read, so the legs that did not land would never be read again.
    pub fn insert_arrivals(&self, account_id: i64, mail_id: i64, rows: &[NewArrival]) -> Result<Vec<i64>> {
        let conn = self.conn();
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<Vec<i64>> {
            conn.execute("DELETE FROM arrivals WHERE mail_id = ? AND status = 'pending'", params![mail_id])?;
            rows.iter()
                .map(|a| {
                    Ok(conn.query_row(
                        "INSERT INTO arrivals (account_id, mail_id, booking, kind, title, place, origin, destination,
                                               airline, flight_number, stops,
                                               date, starts_at, ends_at, timezone, confirmation_code, price, currency,
                                               travellers, confidence, summary, trip_id)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
                        params![
                            account_id, mail_id, a.booking, a.kind, a.title, a.place, a.origin, a.destination,
                            a.airline, a.flight_number, a.stops,
                            a.date, a.starts_at, a.ends_at, a.timezone, a.confirmation_code, a.price, a.currency,
                            a.travellers, a.confidence, a.summary, a.trip_id
                        ],
                        |r| r.get(0),
                    )?)
                })
                .collect()
        })();
        match result {
            Ok(ids) => {
                conn.execute_batch("COMMIT")?;
                Ok(ids)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// The one-reading case, for the tests that want a row and its id
    /// rather than a batch.
    pub fn insert_arrival(&self, account_id: i64, mail_id: i64, a: &NewArrival) -> Result<i64> {
        let mut ids = self.insert_arrivals(account_id, mail_id, std::slice::from_ref(a))?;
        Ok(ids.remove(0))
    }

    /// The arrival, if it is this account's. The owner is part of the key
    /// on purpose: an id from the page is not proof of anything.
    pub fn arrival_of(&self, id: i64, account_id: i64) -> Result<Option<scout_api::Arrival>> {
        let conn = self.conn();
        let found = conn
            .query_row(
                &format!("SELECT {ARRIVAL_SELECT} WHERE a.id = ? AND a.account_id = ?"),
                params![id, account_id],
                arrival_row,
            )
            .optional()?;
        match found {
            Some(mut arrival) => {
                arrival.attachments = attachments_of(&conn, arrival.mail_id)?;
                Ok(Some(arrival))
            }
            None => Ok(None),
        }
    }

    /// Records the owner's decision, and is the claim on it: `false` when
    /// the arrival is not this account's (an id from the page proves
    /// nothing), was decided already, or was never a booking. The check is
    /// in the `WHERE`, under the one lock, so two clicks that both read
    /// "pending" cannot both win — the second finds nothing to update.
    /// `item_id` is the trip item an added booking became, when the caller
    /// already knows it; `note_arrival_item` fills it in later otherwise.
    pub fn decide_arrival(&self, id: i64, account_id: i64, status: &str, item_id: Option<i64>) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE arrivals SET status = ?, item_id = ?, decided_at = now()
             WHERE id = ? AND account_id = ? AND status = 'pending' AND booking",
            params![status, item_id, id, account_id],
        )?;
        Ok(changed > 0)
    }

    /// Undoes a claim whose item could not be built, so the booking waits
    /// on the page again instead of reading as added with nothing to show.
    /// No owner check: the caller reverts only what it just claimed.
    pub fn reopen_arrival(&self, id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "UPDATE arrivals SET status = 'pending', item_id = NULL, decided_at = NULL WHERE id = ?",
            params![id],
        )?;
        Ok(())
    }

    /// The item a claimed arrival became. Written after the claim rather
    /// than with it because the item does not exist until the claim has
    /// won.
    pub fn note_arrival_item(&self, id: i64, item_id: i64) -> Result<()> {
        let conn = self.conn();
        conn.execute("UPDATE arrivals SET item_id = ? WHERE id = ?", params![item_id, id])?;
        Ok(())
    }

    /// What the Trips tab shows of the inbox: bookings still waiting, then
    /// the last month of everything that is not one. `handle`, `domain`
    /// and each row's `sent_by_you` are the caller's — the store knows
    /// neither the address's domain, nor how the handle should read, nor
    /// which senders are the account's own.
    pub fn inbox_view(&self, account_id: i64) -> Result<scout_api::InboxView> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            // Newest mail first, but the readings of one mail in the order
            // they were read: the legs of a ticket belong in the ticket's
            // order, out before back.
            "SELECT {ARRIVAL_SELECT} WHERE a.account_id = ? AND {UNDECIDED}
             ORDER BY m.received_at DESC, a.id ASC"
        ))?;
        let mut pending: Vec<scout_api::Arrival> =
            stmt.query_map(params![account_id], arrival_row)?.collect::<duckdb::Result<_>>()?;
        for arrival in &mut pending {
            arrival.attachments = attachments_of(&conn, arrival.mail_id)?;
        }
        // Other mail lists mail, one row each. One mail can hold several
        // readings — a return ticket is two — and the join would name it
        // once per decided reading, so the `QUALIFY` keeps the first and
        // the row is named by it. A mail that spent its attempts without a
        // verdict — the worker died mid-call — is as failed as one the
        // model refused. The reasons are ranked: unreadable mail comes
        // first (a failed mail with an undecided arrival cannot be produced
        // by the worker, which writes the arrival and only then marks the
        // mail done), and a non-booking is that before it is anything else.
        // The ranking survives the collapse: `failed` is the mail's own
        // status, so it is every reading of that mail's reason at once.
        let failed = unreadable("m");
        let mut stmt = conn.prepare(&format!(
            "SELECT m.id, m.from_address, m.subject, strftime(m.received_at, '%Y-%m-%dT%H:%M:%SZ'), m.forwarded_at IS NOT NULL, a.id,
                    CASE WHEN {failed} THEN 'failed'
                         WHEN NOT a.booking THEN 'not_booking'
                         ELSE 'ignored' END
             FROM inbound_mail m LEFT JOIN arrivals a ON a.mail_id = m.id
             WHERE m.account_id = ? AND m.received_at >= ?
               AND ({failed} OR (a.id IS NOT NULL AND (NOT a.booking OR a.status = 'ignored')))
             -- One row per mail. The partition is ordered by the same rank the
             -- CASE above uses, so the reason the collapse keeps is the reason
             -- the mail would have shown anyway: a not-a-booking reading
             -- outranks an ignored one, and `failed` is the mail's own status
             -- and so is every row's reason at once.
             QUALIFY row_number() OVER (PARTITION BY m.id ORDER BY (NOT a.booking) DESC, a.id) = 1
             ORDER BY m.received_at DESC, m.id DESC"
        ))?;
        let mut other: Vec<scout_api::MailRow> = stmt
            .query_map(params![account_id, days_ago(30)], |r| {
                Ok(scout_api::MailRow {
                    mail_id: r.get(0)?, from: r.get(1)?, subject: r.get(2)?, received_at: r.get(3)?,
                    forwarded: r.get(4)?, arrival_id: r.get(5)?, reason: r.get(6)?, attachments: Vec::new(),
                    // Like `handle` and `domain` below: the caller's to
                    // fill. Whether a sender is one of the account's own
                    // addresses is a rule, not a query, and this module
                    // is the wrong side of the layering to hold it.
                    sent_by_you: false,
                })
            })?
            .collect::<duckdb::Result<_>>()?;
        for row in &mut other {
            row.attachments = attachments_of(&conn, row.mail_id)?;
        }
        Ok(scout_api::InboxView { handle: None, domain: String::new(), pending, other })
    }

    /// Forgets mail older than `days`, with its reading, its parts and its
    /// loose files. Kept regardless of age: a mail whose booking nobody has
    /// decided on yet, and any file that has become part of a trip item.
    /// Returns the mail rows deleted.
    pub fn sweep_inbox(&self, days: i64) -> Result<usize> {
        let cutoff = days_ago(days);
        let conn = self.conn();
        // The one definition of "sweepable" the four deletes share; the
        // mail goes last so a crash between them leaves nothing orphaned
        // that the next sweep will not find again.
        let sweepable = format!(
            "SELECT m.id FROM inbound_mail m WHERE m.received_at < ?
               AND NOT EXISTS (SELECT 1 FROM arrivals a WHERE a.mail_id = m.id AND {UNDECIDED})"
        );
        // Unconditionally, unlike the files below: a part describes one
        // mail and can never come to belong to anything else.
        conn.execute(&format!("DELETE FROM mail_parts WHERE mail_id IN ({sweepable})"), params![cutoff])?;
        conn.execute(
            &format!("DELETE FROM attachments WHERE item_id IS NULL AND mail_id IN ({sweepable})"),
            params![cutoff],
        )?;
        conn.execute(&format!("DELETE FROM arrivals WHERE mail_id IN ({sweepable})"), params![cutoff])?;
        Ok(conn.execute(&format!("DELETE FROM inbound_mail WHERE id IN ({sweepable})"), params![cutoff])?)
    }

    /// Forgets one mail now, on its owner's say-so, instead of in thirty
    /// days.
    ///
    /// The four deletes are `sweep_inbox`'s — the parts, the loose files,
    /// the readings, then the mail — but unlike the sweep they are in one
    /// transaction, which is what keeps them whole: a crash takes all four
    /// back, so nothing is orphaned and the order between them decides
    /// nothing. It is kept anyway, so the two paths that delete a mail read
    /// the same. A file that has joined a trip item is the trip's now and
    /// stays — `attachment_owner` answers for it through the item. A part
    /// has no such escape: it says what one mail's part was and is worth
    /// nothing once that mail is gone.
    ///
    /// Two refusals, both about rows this delete would strand:
    ///
    /// - a booking still waiting on its owner, which is `UNDECIDED`, the
    ///   same predicate that keeps such a mail out of the sweep;
    /// - a mail the worker has not finished with, which would have its
    ///   reading written against it moments later.
    ///
    /// Every check runs under the one `conn()` the deletes hold, so nothing
    /// can decide, undecide or finish reading in between.
    pub fn delete_mail(&self, account_id: i64, mail_id: i64) -> Result<MailGone> {
        let conn = self.conn();
        // The account is half the key, as in `arrival_of`: an id in a URL
        // is not proof of anything. Whether the worker is done with the
        // mail is read in the same row — read, refused, or out of attempts,
        // which is `unreadable` and so the same set `inbox_view` can list.
        // A `new` or `extracting` mail with attempts left is still on its
        // way to a reading.
        let mine: Option<bool> = conn
            .query_row(
                &format!(
                    "SELECT m.status = 'done' OR {} FROM inbound_mail m WHERE m.id = ? AND m.account_id = ?",
                    unreadable("m")
                ),
                params![mail_id, account_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(settled) = mine else {
            return Ok(MailGone::NotFound);
        };
        let waiting: Option<i64> = conn
            .query_row(
                &format!("SELECT a.id FROM arrivals a WHERE a.mail_id = ? AND {UNDECIDED} LIMIT 1"),
                params![mail_id],
                |r| r.get(0),
            )
            .optional()?;
        if waiting.is_some() {
            return Ok(MailGone::Waiting);
        }
        if !settled {
            return Ok(MailGone::Unsettled);
        }
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            conn.execute("DELETE FROM mail_parts WHERE mail_id = ?", params![mail_id])?;
            conn.execute(
                "DELETE FROM attachments WHERE item_id IS NULL AND mail_id = ?",
                params![mail_id],
            )?;
            conn.execute("DELETE FROM arrivals WHERE mail_id = ?", params![mail_id])?;
            conn.execute("DELETE FROM inbound_mail WHERE id = ?", params![mail_id])?;
            Ok(())
        })();
        match result {
            // A `COMMIT` that fails leaves the transaction open, and DuckDB
            // will not start another inside it — the next caller on this
            // connection would fail for a reason that was never theirs.
            Ok(()) => match conn.execute_batch("COMMIT") {
                Ok(()) => Ok(MailGone::Gone),
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e.into())
                }
            },
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// `(trip id, date, place)` per booking still waiting on one of this
    /// account's trips.
    ///
    /// A draft holds no items until its owner presses Add, so for the hours
    /// or days in between these rows are the only thing that says what the
    /// trip is about. Placement reads them beside the items, or a second
    /// email about the same journey can never find the draft the first one
    /// made.
    pub fn pending_arrival_marks(&self, account_id: i64) -> Result<Vec<(i64, String, Option<String>)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT a.trip_id, a.date, a.place FROM arrivals a
             WHERE a.account_id = ? AND {UNDECIDED} AND a.trip_id IS NOT NULL AND a.date IS NOT NULL
             ORDER BY a.id"
        ))?;
        let rows = stmt.query_map(params![account_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Deletes this account's drafts that hold nothing, that nothing is
    /// waiting on, and that are old enough to be stale. Returns how many
    /// went.
    pub fn sweep_empty_drafts(&self, account_id: i64) -> Result<usize> {
        let conn = self.conn();
        sweep_drafts_within(&conn, Some(account_id))
    }

    /// The same collection over every account, for the hourly maintenance:
    /// the drafts already sitting in somebody's list were abandoned before
    /// `add_arrival` swept, and nothing else will ever reach them.
    pub fn sweep_all_empty_drafts(&self) -> Result<usize> {
        let conn = self.conn();
        sweep_drafts_within(&conn, None)
    }

    /// Deletes the attachments nothing owns, and returns how many went.
    ///
    /// A row whose owner resolves to nothing is unreachable by definition:
    /// no reader may download it, no listing draws it, and neither mail
    /// sweep can find it, because both key their deletes on a mail id that
    /// is already gone. Nothing can be lost by deleting it, and nothing
    /// else will ever delete it — which is why this exists even though the
    /// delete paths no longer make such rows. It is what clears the ones
    /// already stranded in a live database.
    ///
    /// The condition is `attachment_owner`'s two routes, negated: neither
    /// the mail nor a trip behind the item is still there. Run from the
    /// hourly maintenance; a pass that finds nothing is two index-less
    /// scans of a small table, which is what the sweeps beside it cost too.
    pub fn sweep_orphaned_attachments(&self) -> Result<usize> {
        let conn = self.conn();
        Ok(conn.execute(
            "DELETE FROM attachments a
             WHERE NOT EXISTS (SELECT 1 FROM inbound_mail m WHERE m.id = a.mail_id)
               AND NOT EXISTS (SELECT 1 FROM trip_items i JOIN trips t ON t.id = i.trip_id
                               WHERE i.id = a.item_id)",
            params![],
        )?)
    }
}

/// How long a draft has to have existed before it can be collected.
///
/// Without a floor a draft is collectable the instant `upsert_trip`
/// returns, and there are two windows in which that is reachable now that
/// every Add and Ignore sweeps: `record_arrivals` between placing a mail
/// and writing its arrivals, and `add_arrival` between claiming the arrival
/// and building the item. Both are milliseconds; five minutes is far past
/// either and far short of any draft a person is looking at.
const DRAFT_GRACE_MINUTES: i64 = 5;

/// A draft nothing landed on: not kept, owned by no conversation, holding
/// no items, with no booking waiting to become one, and older than the
/// grace above.
///
/// Those together are only ever a placement draft that was abandoned —
/// `inbox::record_arrivals` made it to have somewhere to put a booking,
/// and the booking went elsewhere or was ignored. A chat's own draft has a
/// conversation and is the thread sweep's business; a draft whose booking
/// is still pending is the reason that booking has a trip name on the page.
///
/// `UNDECIDED` is interpolated rather than restated: it is the same "still
/// waiting" that makes a trip matchable in `inbox::Trips::matching`, and a
/// collection rule that drifted from the matching rule would delete a draft
/// the next mail could still have joined.
///
/// **Both sides of the age test are on the clock that writes the column.**
/// `trips.created_at` defaults to bare `current_timestamp`, which DuckDB
/// resolves in the session's zone — measured on this build with
/// `SET TimeZone = 'America/New_York'`: a trip written at 10:54 UTC stores
/// `06:54`, while `current_timestamp AT TIME ZONE 'UTC'` and
/// `chrono::Utc::now()` both read `10:54`. A cutoff taken from either of
/// those sits hours ahead of every row west of UTC, which makes the floor a
/// no-op and silently reopens the two windows it exists to close. So the
/// cutoff is derived in SQL from the same bare `current_timestamp`, and
/// `age_trip` writes on it too. A daylight-saving shift moves both sides
/// together; the worst it can do inside the shifted hour is collect an
/// empty draft an hour early.
fn collectable_draft() -> String {
    format!(
        "NOT kept AND conversation_id IS NULL
         AND created_at < CAST(current_timestamp AS TIMESTAMP) - to_minutes(CAST(? AS INTEGER))
         AND NOT EXISTS (SELECT 1 FROM trip_items i WHERE i.trip_id = trips.id)
         AND NOT EXISTS (SELECT 1 FROM arrivals a WHERE a.trip_id = trips.id AND {UNDECIDED})"
    )
}

/// Collects the stale empty drafts of one account, or of everybody.
///
/// Reads the rows before deleting them and logs what went by name: a sweep
/// that deletes a person's trip must leave enough behind to say which trip
/// it was. The arrivals that pointed at a collected draft — decided ones,
/// which is why it was collectable — have their `trip_id` cleared in the
/// same transaction, so nothing is left naming a trip that is gone.
fn sweep_drafts_within(conn: &Connection, account_id: Option<i64>) -> Result<usize> {
    let mine = if account_id.is_some() { "account_id = ? AND " } else { "" };
    // The account first when there is one, then the grace: the order the
    // `?`s appear in the statement. The grace is a number of minutes, not a
    // timestamp — see `collectable_draft` for why the cutoff cannot come
    // from Rust.
    let mut args: Vec<Box<dyn duckdb::ToSql>> = Vec::new();
    if let Some(id) = account_id {
        args.push(Box::new(id));
    }
    args.push(Box::new(DRAFT_GRACE_MINUTES));
    // Bounded: the hourly pass over every account must not build an
    // unbounded id list out of a database nobody has swept in a year. What
    // it leaves behind the next pass takes.
    let mut stmt = conn.prepare(&format!(
        "SELECT id, account_id, name FROM trips WHERE {mine}{} LIMIT 500",
        collectable_draft()
    ))?;
    let doomed: Vec<(i64, i64, String)> = stmt
        .query_map(duckdb::params_from_iter(args.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<duckdb::Result<_>>()?;
    drop(stmt);
    if doomed.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i64> = doomed.iter().map(|(id, _, _)| *id).collect();
    let holes = ["?"].repeat(ids.len()).join(", ");
    conn.execute_batch("BEGIN")?;
    let result = (|| -> Result<usize> {
        conn.execute(
            &format!("UPDATE arrivals SET trip_id = NULL WHERE trip_id IN ({holes})"),
            duckdb::params_from_iter(ids.iter()),
        )?;
        Ok(conn.execute(
            &format!("DELETE FROM trips WHERE id IN ({holes})"),
            duckdb::params_from_iter(ids.iter()),
        )?)
    })();
    match result {
        Ok(n) => {
            // A failed `COMMIT` leaves the transaction open on a connection
            // every other caller shares, and DuckDB will not start another
            // inside it — see `delete_mail`, which closes the same door.
            if let Err(e) = conn.execute_batch("COMMIT") {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e.into());
            }
            for (id, account_id, name) in &doomed {
                tracing::info!(trip_id = id, account_id, name = %name, "empty placement draft collected");
            }
            Ok(n)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tempfile::TempDir;

    pub(crate) fn test_store() -> (Store, TempDir) {
        let dir = TempDir::new().unwrap();
        let store = Store::open(dir.path().join("test.duckdb")).unwrap();
        (store, dir)
    }

    /// The schema exactly as it stood before phase one, frozen. Do not
    /// update this when `MIGRATIONS` changes — its whole value is being an
    /// honest picture of the database the migration will actually meet.
    const LEGACY_SCHEMA: &str = r#"
CREATE SEQUENCE purchases_id_seq;
CREATE TABLE purchases (
    id BIGINT PRIMARY KEY DEFAULT nextval('purchases_id_seq'),
    user_id BIGINT NOT NULL, item TEXT NOT NULL, store TEXT NOT NULL,
    url TEXT, price DOUBLE, currency TEXT, notes TEXT, purchased_at TEXT,
    recorded_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE SEQUENCE reminders_id_seq;
CREATE TABLE reminders (
    id BIGINT PRIMARY KEY DEFAULT nextval('reminders_id_seq'),
    user_id BIGINT NOT NULL, chat_id BIGINT NOT NULL, item TEXT NOT NULL,
    interval_days BIGINT NOT NULL, next_due TEXT NOT NULL,
    active BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE user_facts (
    user_id BIGINT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    PRIMARY KEY (user_id, key)
);
CREATE TABLE request_log (
    user_id BIGINT NOT NULL, kind TEXT NOT NULL,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE users (
    user_id BIGINT PRIMARY KEY, display_name TEXT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE user_chats (
    user_id BIGINT PRIMARY KEY, chat_id BIGINT NOT NULL,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE invite_rounds (
    code TEXT PRIMARY KEY, capacity BIGINT NOT NULL,
    open BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE TABLE members (
    user_id BIGINT PRIMARY KEY, code TEXT NOT NULL,
    joined_at TIMESTAMP NOT NULL DEFAULT current_timestamp, revoked_at TIMESTAMP
);
CREATE TABLE waitlist (
    user_id BIGINT PRIMARY KEY, chat_id BIGINT NOT NULL, code TEXT NOT NULL,
    seen_at TIMESTAMP NOT NULL DEFAULT current_timestamp, invited_at TIMESTAMP
);
CREATE SEQUENCE trips_id_seq;
CREATE TABLE trips (
    id BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    user_id BIGINT NOT NULL, name TEXT NOT NULL, name_key TEXT NOT NULL,
    adults BIGINT NOT NULL DEFAULT 1, cabin_class TEXT,
    status TEXT NOT NULL DEFAULT 'planning',
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    UNIQUE (user_id, name_key)
);
CREATE TABLE trip_segments (
    trip_id BIGINT NOT NULL, position BIGINT NOT NULL, origin TEXT NOT NULL,
    destination TEXT NOT NULL, departure_date TEXT NOT NULL,
    next_candidate BIGINT NOT NULL DEFAULT 1, PRIMARY KEY (trip_id, position)
);
CREATE TABLE segment_candidates (
    trip_id BIGINT NOT NULL, position BIGINT NOT NULL, candidate BIGINT NOT NULL,
    chosen BOOLEAN NOT NULL DEFAULT false, airline TEXT NOT NULL,
    flight_numbers TEXT NOT NULL, itinerary TEXT NOT NULL,
    departing_at_local TEXT, arriving_at_local TEXT, duration_minutes BIGINT,
    quoted_price DOUBLE, quoted_currency TEXT, quoted_at TIMESTAMP, source TEXT,
    PRIMARY KEY (trip_id, position, candidate)
);
"#;

    /// Two Telegram users with data spread across every table, written into
    /// a database that has never seen a migration step. User 33 appears only
    /// on the waitlist, which is what catches a backfill that reads accounts
    /// from the wrong set of tables.
    pub(crate) fn legacy_db() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.duckdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(LEGACY_SCHEMA).unwrap();
        conn.execute_batch(
            "INSERT INTO purchases (user_id, item, store) VALUES (11,'beans','Amazon'),(22,'mouse','eBay');
             INSERT INTO reminders (user_id, chat_id, item, interval_days, next_due)
                 VALUES (11, 555, 'beans', 30, '2026-09-01');
             INSERT INTO user_facts VALUES (11,'currency','EUR',current_timestamp);
             INSERT INTO request_log (user_id, kind) VALUES (11,'text'),(11,'photo'),(22,'text');
             INSERT INTO users (user_id, display_name) VALUES (11,'Ann'),(22,'Bo');
             INSERT INTO user_chats (user_id, chat_id) VALUES (11,555),(22,666);
             INSERT INTO invite_rounds (code, capacity) VALUES ('spring', 5);
             INSERT INTO members (user_id, code) VALUES (22,'spring');
             INSERT INTO waitlist (user_id, chat_id, code) VALUES (33,777,'spring');
             INSERT INTO trips (user_id, name, name_key) VALUES (11,'Lisbon','lisbon');",
        )
        .unwrap();
        drop(conn);
        (dir, path)
    }

    #[test]
    fn missing_names_come_back_with_both_ids_because_they_are_different_numbers() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(8849043058).unwrap();
        let b = s.account_for_telegram(1980797790).unwrap();
        s.remember_user(a, "Ann").unwrap();

        // Only b is missing, and it comes back as (account, telegram) — two
        // very different numbers. An account id is a small counter; a
        // Telegram id is ten digits. Using one where the other belongs
        // matches nothing, silently.
        let missing = s.accounts_missing_display_names(10).unwrap();
        assert_eq!(missing, vec![(b, 1980797790)]);
        assert_ne!(b, 1980797790, "the two id spaces must not be confused");

        s.remember_user(b, "Bo").unwrap();
        assert!(s.accounts_missing_display_names(10).unwrap().is_empty());
    }

    #[test]
    fn an_identity_is_looked_up_or_created_whatever_its_kind() {
        let (s, _d) = test_store();
        let first = s.account_for_identity("email", "a@example.com").unwrap();
        let again = s.account_for_identity("email", "a@example.com").unwrap();
        assert_eq!(first, again, "the same identity produced two accounts");

        // The generalisation must not have changed what Telegram does.
        let tg = s.account_for_identity("telegram", "11").unwrap();
        assert_eq!(tg, s.account_for_telegram(11).unwrap());
        assert_ne!(tg, first, "two kinds collided into one account");
    }

    #[test]
    fn an_identity_owned_by_someone_else_is_never_moved() {
        let (s, _d) = test_store();
        let owner = s.account_for_identity("telegram", "11").unwrap();
        let other = s.account_for_identity("email", "b@example.com").unwrap();
        // Both have asked Scout something, so both are real people and
        // neither may be absorbed. Without this the two are empty and the
        // merge rule applies instead — which is the point of the rule, and
        // the reason this test has to say which case it is testing.
        s.log_request(owner, "text").unwrap();
        s.log_request(other, "text").unwrap();

        assert_eq!(
            s.link_identity(other, "telegram", "11").unwrap(),
            LinkOutcome::TakenByAnother
        );
        // The point of the test: the refusal left ownership alone.
        assert_eq!(s.account_for_identity("telegram", "11").unwrap(), owner);

        assert_eq!(
            s.link_identity(owner, "telegram", "11").unwrap(),
            LinkOutcome::AlreadyYours
        );
        assert_eq!(
            s.link_identity(owner, "email", "c@example.com").unwrap(),
            LinkOutcome::Linked
        );
        assert_eq!(s.account_for_identity("email", "c@example.com").unwrap(), owner);
    }

    #[test]
    fn signing_in_by_email_then_adding_telegram_lands_in_the_account_with_the_history() {
        // The shape every early user hits: they already talk to Scout on
        // Telegram, they sign in on the web with an address Scout has never
        // seen, and that mints a second account which takes a second seat.
        let (s, _d) = test_store();
        s.create_round("autumn", 5).unwrap();

        let telegram = s.account_for_identity("telegram", "11").unwrap();
        s.claim_seat(telegram, "autumn").unwrap();
        s.log_request(telegram, "text").unwrap();

        let web = s.account_for_identity("email", "a@example.com").unwrap();
        assert_eq!(s.claim_seat(web, "autumn").unwrap(), Claim::Admitted);
        assert_eq!(s.rounds().unwrap()[0].used, 2, "the second sign-in did not take a seat");

        // Pressing the Telegram button from that session.
        assert_eq!(
            s.link_identity(web, "telegram", "11").unwrap(),
            LinkOutcome::Merged { account_id: telegram },
            "the survivor must be the side holding the history, not the side that asked"
        );

        // Both ways in now reach the one account.
        assert_eq!(s.account_for_identity("telegram", "11").unwrap(), telegram);
        assert_eq!(s.account_for_identity("email", "a@example.com").unwrap(), telegram);
        assert_eq!(
            s.identity_kinds(telegram).unwrap(),
            vec!["email".to_string(), "telegram".to_string()]
        );
        assert!(s.is_member(telegram).unwrap());

        let survived: i64 = s
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM accounts WHERE id = ?", params![web], |r| r.get(0))
            .unwrap();
        assert_eq!(survived, 0, "the absorbed account row outlived the merge");

        // And the seat that phantom account was holding is back in the round.
        assert_eq!(s.rounds().unwrap()[0].used, 1, "the phantom seat was not returned");
    }

    #[test]
    fn an_empty_account_on_the_other_side_is_absorbed_into_mine() {
        // The mirror direction: I am the one with the history, and the
        // address I am adding was signed in with once and never used.
        let (s, _d) = test_store();
        let mine = s.account_for_identity("telegram", "11").unwrap();
        s.log_request(mine, "text").unwrap();
        let stray = s.account_for_identity("email", "a@example.com").unwrap();

        assert_eq!(
            s.link_identity(mine, "email", "a@example.com").unwrap(),
            LinkOutcome::Merged { account_id: mine }
        );

        let survived: i64 = s
            .conn
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM accounts WHERE id = ?", params![stray], |r| r.get(0))
            .unwrap();
        assert_eq!(survived, 0);
    }

    #[test]
    fn two_empty_accounts_merge_to_the_older_id_whichever_one_asks() {
        // Nothing distinguishes them, so the answer must not depend on who
        // clicked — otherwise the same pair merges two different ways.
        let (a, _da) = test_store();
        let older = a.account_for_identity("telegram", "11").unwrap();
        let newer = a.account_for_identity("email", "x@example.com").unwrap();
        assert!(older < newer, "test assumes ids are handed out in order");
        assert_eq!(
            a.link_identity(newer, "telegram", "11").unwrap(),
            LinkOutcome::Merged { account_id: older }
        );

        let (b, _db) = test_store();
        let older = b.account_for_identity("telegram", "11").unwrap();
        b.account_for_identity("email", "x@example.com").unwrap();
        assert_eq!(
            b.link_identity(older, "email", "x@example.com").unwrap(),
            LinkOutcome::Merged { account_id: older }
        );
    }

    #[test]
    fn a_merge_refuses_to_absorb_an_account_that_holds_something() {
        // The guard that stands between a bug in the caller and somebody
        // losing their purchases. `link_identity` already checks; this is
        // the second check, tested on its own.
        let (s, _d) = test_store();
        let keeper = s.account_for_identity("telegram", "11").unwrap();
        let busy = s.account_for_identity("email", "a@example.com").unwrap();
        s.log_request(busy, "text").unwrap();

        let conn = s.conn.lock().unwrap();
        let err = merge_accounts(&conn, busy, keeper).unwrap_err();
        assert!(err.to_string().contains("holds content"), "unexpected error: {err}");
    }

    #[test]
    fn a_merged_account_that_is_inside_is_not_left_on_the_waitlist() {
        // The absorbed side was queued and the survivor is a member, so the
        // waitlist row moves and must then be dropped: an announce that
        // chases a member is the failure this prevents.
        let (s, _d) = test_store();
        s.create_round("autumn", 1).unwrap();
        let member = s.account_for_identity("telegram", "11").unwrap();
        s.claim_seat(member, "autumn").unwrap();
        s.log_request(member, "text").unwrap();

        let web = s.account_for_identity("email", "a@example.com").unwrap();
        assert_eq!(s.claim_seat(web, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.waiting_count().unwrap(), 1);

        assert_eq!(
            s.link_identity(web, "telegram", "11").unwrap(),
            LinkOutcome::Merged { account_id: member }
        );
        assert_eq!(s.waiting_count().unwrap(), 0, "a member was left queued");
    }

    #[test]
    fn releasing_a_seat_hands_it_back_where_revoking_deliberately_does_not() {
        let (s, _d) = test_store();
        s.create_round("autumn", 1).unwrap();
        let a = s.account_for_identity("telegram", "11").unwrap();
        s.claim_seat(a, "autumn").unwrap();
        assert_eq!(s.rounds().unwrap()[0].used, 1);

        // Revoking keeps the row, so the seat stays spent. That is the
        // existing promise and this must not change it.
        assert!(s.revoke(a).unwrap());
        assert_eq!(s.rounds().unwrap()[0].used, 1, "revoking handed a seat back");

        assert!(s.release_seat(a).unwrap());
        assert_eq!(s.rounds().unwrap()[0].used, 0);
        assert!(!s.release_seat(a).unwrap(), "a seat nobody held was released anyway");
    }

    #[test]
    fn telegram_ids_come_back_as_numbers_and_skip_what_will_not_parse() {
        let (s, _d) = test_store();
        let a = s.account_for_identity("telegram", "11").unwrap();
        s.link_identity(a, "telegram", "22").unwrap();
        s.link_identity(a, "email", "x@example.com").unwrap();
        // `external_id` is TEXT, so nothing stops a row like this existing.
        s.link_identity(a, "telegram", "not-a-number").unwrap();

        let mut ids = s.telegram_ids(a).unwrap();
        ids.sort();
        assert_eq!(ids, vec![11, 22], "an address or an unparseable row leaked in");
    }

    #[test]
    fn two_identities_of_one_kind_are_one_way_in_not_two() {
        // A row is `(kind, external_id)`, so an account holding two
        // Telegram accounts holds two `telegram` rows — a legitimate
        // shape, and the shape the takeover path in the W2 review also
        // produced. What asks for this list asks which *methods* exist,
        // and writes the answer out in order: undeduplicated, the account
        // page reads "Signed in with Telegram, Telegram."
        let (s, _d) = test_store();
        let account = s.account_for_identity("telegram", "11").unwrap();
        assert_eq!(s.link_identity(account, "telegram", "22").unwrap(), LinkOutcome::Linked);
        assert_eq!(s.link_identity(account, "email", "a@example.com").unwrap(), LinkOutcome::Linked);

        assert_eq!(
            s.identity_kinds(account).unwrap(),
            vec!["email".to_string(), "telegram".to_string()],
            "a kind was named once per identity rather than once"
        );
    }

    #[test]
    fn a_fresh_database_has_somewhere_to_put_login_tokens() {
        let (s, _d) = test_store();
        assert_eq!(s.schema_version().unwrap(), 18);
        // A fresh database is built by MIGRATIONS and a migrated one by
        // steps(); this fails if only one of the two learned about the table.
        s.issue_login_token("hash-x", "a@example.com", None, 900).unwrap();
    }

    #[test]
    fn old_login_tokens_are_pruned_and_recent_ones_are_not() {
        // The table used to be append-only: one row per sign-in *attempt*,
        // kept forever, written by anybody who can reach the form. That is
        // a table whose size a stranger decides.
        let (s, _d) = test_store();
        const DAY: i64 = 86_400;

        // Expired a day and a bit ago — past what any advice is worth.
        s.issue_login_token("hash-ancient", "a@example.com", None, -(DAY + 900)).unwrap();
        // Expired an hour ago. Still inside the window on purpose: this is
        // the row that keeps "already used" and "expired" apart, and
        // pruning it would turn a stale link into "we have never seen that
        // token".
        s.issue_login_token("hash-recent", "b@example.com", None, -3600).unwrap();
        // Alive.
        s.issue_login_token("hash-live", "c@example.com", None, 900).unwrap();

        assert_eq!(s.prune_login_tokens(DAY).unwrap(), 1, "the wrong number of rows went");

        assert_eq!(s.consume_login_token("hash-ancient").unwrap(), TokenOutcome::Unknown);
        assert_eq!(s.consume_login_token("hash-recent").unwrap(), TokenOutcome::Expired);
        assert_eq!(
            s.consume_login_token("hash-live").unwrap(),
            TokenOutcome::Valid { email: "c@example.com".to_string(), account_id: None }
        );

        // And the thing the window exists to protect: a row that has been
        // spent recently survives a prune, so the second press of the same
        // button still reads as "already used" rather than as a token that
        // never existed.
        s.issue_login_token("hash-spent", "d@example.com", None, 900).unwrap();
        assert!(matches!(
            s.consume_login_token("hash-spent").unwrap(),
            TokenOutcome::Valid { .. }
        ));
        assert_eq!(s.prune_login_tokens(DAY).unwrap(), 0, "a live row was pruned");
        assert_eq!(s.consume_login_token("hash-spent").unwrap(), TokenOutcome::AlreadyUsed);
    }

    #[test]
    fn a_migrated_database_gets_login_tokens_too_not_just_a_fresh_one() {
        // `test_store` opens a fresh file; production opens one at schema
        // 5. This covers the second, which nothing else did.
        //
        // What it does NOT cover, established by breaking each in turn:
        // STEP_6 does not create this table. `Store::open` runs MIGRATIONS
        // unconditionally before applying any step, and MIGRATIONS is all
        // CREATE TABLE IF NOT EXISTS, so the table appears on every
        // database either way — renaming the table inside STEP_6 leaves
        // both tests green. What STEP_6 earns is the version bump, which
        // these tests do pin: dropping it from steps() fails both on
        // `schema_version`. That bump is not bookkeeping either. A pending
        // step is what makes the migration runner take a backup first, so
        // it is the reason this deploy copies the database before touching
        // it.
        let (_dir, path) = legacy_db();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 18);
        store.issue_login_token("hash-migrated", "m@example.com", None, 900).unwrap();
        assert_eq!(
            store.consume_login_token("hash-migrated").unwrap(),
            TokenOutcome::Valid { email: "m@example.com".to_string(), account_id: None }
        );
    }

    #[test]
    fn a_token_works_once_and_says_so_afterwards() {
        let (s, _d) = test_store();
        s.issue_login_token("hash-a", "a@example.com", None, 900).unwrap();

        assert_eq!(
            s.consume_login_token("hash-a").unwrap(),
            TokenOutcome::Valid { email: "a@example.com".to_string(), account_id: None }
        );
        // The whole reason the row survives consumption: the second visit
        // gets advice ("you may already be signed in"), not "expired".
        assert_eq!(s.consume_login_token("hash-a").unwrap(), TokenOutcome::AlreadyUsed);
        assert_eq!(s.consume_login_token("hash-never").unwrap(), TokenOutcome::Unknown);
    }

    #[test]
    fn a_spent_token_is_stamped_on_the_clock_that_expires_it() {
        // Every timestamp this table holds is UTC-naive, and the expiry
        // check compares against `current_timestamp AT TIME ZONE 'UTC'`.
        // `consumed_at` used to be written by bare `current_timestamp`,
        // which DuckDB resolves in the session's local zone — two clocks in
        // one table. Nothing reads the value today, only whether it is
        // null, which is exactly why this needs a test rather than a
        // reader to notice it.
        let (s, _d) = test_store();

        // Pinned to a zone that is not UTC, so this fails on a UTC machine
        // as well as on a laptop. The `requests_today` version of this bug
        // survived in production precisely because the container runs UTC
        // and the two clocks agreed there; a test that only bites off-UTC
        // would be a test that agrees with the machine it runs on.
        s.conn
            .lock()
            .unwrap()
            .execute("SET TimeZone = 'America/New_York'", [])
            .expect("the session's zone can be set");

        s.issue_login_token("hash-clock", "a@example.com", None, 900).unwrap();
        assert!(matches!(
            s.consume_login_token("hash-clock").unwrap(),
            TokenOutcome::Valid { .. }
        ));

        let written: String = {
            let conn = s.conn.lock().unwrap();
            conn.query_row(
                "SELECT consumed_at::TEXT FROM login_tokens WHERE token_hash = ?",
                params!["hash-clock"],
                |r| r.get(0),
            )
            .unwrap()
        };
        let stamped = chrono::NaiveDateTime::parse_from_str(&written, "%Y-%m-%d %H:%M:%S%.f")
            .unwrap_or_else(|e| panic!("consumed_at reads {written:?}, which does not parse: {e}"));
        let skew = (chrono::Utc::now().naive_utc() - stamped).num_seconds().abs();
        assert!(
            skew < 60,
            "consumed_at was written {skew}s from UTC now ({written:?}) — another clock wrote it"
        );
    }

    #[test]
    fn an_expired_token_is_refused_and_not_consumed() {
        let (s, _d) = test_store();
        s.issue_login_token("hash-b", "b@example.com", None, -1).unwrap();
        assert_eq!(s.consume_login_token("hash-b").unwrap(), TokenOutcome::Expired);
        // Expiry must not silently mark it used, or the advice above flips
        // to the wrong branch for anyone who clicks twice.
        assert_eq!(s.consume_login_token("hash-b").unwrap(), TokenOutcome::Expired);
    }

    #[test]
    fn a_link_issued_while_signed_in_remembers_whose_it_is() {
        let (s, _d) = test_store();
        let account = s.account_for_identity("telegram", "11").unwrap();
        s.issue_login_token("hash-c", "c@example.com", Some(account), 900).unwrap();
        assert_eq!(
            s.consume_login_token("hash-c").unwrap(),
            TokenOutcome::Valid { email: "c@example.com".to_string(), account_id: Some(account) }
        );
    }

    #[test]
    fn usage_is_counted_by_account_not_by_telegram_id() {
        let (s, _d) = test_store();
        let account = s.account_for_telegram(8849043058).unwrap();
        s.log_request(account, "text").unwrap();

        let cutoff = "1970-01-01 00:00:00";
        assert_eq!(s.usage_stats_for(cutoff, account).unwrap().len(), 1);
        // The trap: a Telegram id here is a valid i64 and matches nothing,
        // so /stat quietly reported an empty week instead of failing.
        assert!(s.usage_stats_for(cutoff, 8849043058).unwrap().is_empty());
    }

    #[test]
    fn a_conversation_round_trips_and_is_scoped() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();

        let direct = s.start_conversation(a, "direct").unwrap();
        let group = s.start_conversation(a, "telegram:-100").unwrap();
        assert_ne!(direct, group, "a group must not share the private thread");

        s.replace_messages(
            direct,
            &[
                r#"{"role":"user","content":"hi"}"#.to_string(),
                r#"{"role":"assistant","content":"hello"}"#.to_string(),
            ],
        )
        .unwrap();

        let bodies = s.conversation_messages(direct, 20).unwrap();
        assert_eq!(bodies.len(), 2);
        assert!(bodies[0].1.contains("hi"), "oldest first");

        assert!(s.conversation_messages(group, 20).unwrap().is_empty());
        // Each scope reports its own newest thread, never the other's.
        assert_eq!(s.latest_conversation(a, "direct", 0).unwrap().unwrap().0, direct);
        assert_eq!(s.latest_conversation(a, "telegram:-100", 0).unwrap().unwrap().0, group);
    }

    #[test]
    fn a_conversation_returns_only_the_last_n_messages_oldest_first() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();
        let bodies: Vec<String> = (0..25).map(|i| format!(r#"{{"n":{i}}}"#)).collect();
        s.replace_messages(c, &bodies).unwrap();

        let got = s.conversation_messages(c, 20).unwrap();
        assert_eq!(got.len(), 20);
        assert!(got[0].1.contains(r#""n":5"#), "should drop the oldest five");
        assert!(got[19].1.contains(r#""n":24"#), "and end at the newest");
    }

    #[test]
    fn appending_continues_after_what_is_already_stored() {
        // The `messages` table is the whole log now, written a run at a
        // time. An append that restarted `position` at zero would interleave
        // the runs when they are read back in position order — the thread
        // would come out shuffled rather than short.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        s.append_messages(c, None, &[r#"{"n":0}"#.to_string(), r#"{"n":1}"#.to_string()]).unwrap();
        s.append_messages(
            c,
            None,
            &[r#"{"n":2}"#.to_string(), r#"{"n":3}"#.to_string(), r#"{"n":4}"#.to_string()],
        )
        .unwrap();

        let all = s.conversation_messages(c, 20).unwrap();
        assert_eq!(all.len(), 5, "the first append was overwritten");
        for (i, (_, body)) in all.iter().enumerate() {
            assert!(body.contains(&format!(r#""n":{i}"#)), "out of order at {i}: {all:?}");
        }

        // And the window a reader asks for is still the newest end of it.
        let last_three = s.conversation_messages(c, 3).unwrap();
        assert_eq!(last_three.len(), 3);
        assert!(last_three[0].1.contains(r#""n":2"#), "the window is not the newest three: {last_three:?}");
        assert!(last_three[2].1.contains(r#""n":4"#));
    }

    #[test]
    fn a_log_past_the_cap_loses_its_oldest_rows_and_a_short_one_loses_nothing() {
        // A pinned thread is exempt from the 48-hour sweep, so nothing else
        // ever bounds its log — and the log holds every tool call and
        // result a run made, not just what was said.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let long = s.start_conversation(a, "direct").unwrap();
        let short = s.start_conversation(a, "telegram:-100").unwrap();

        let bodies: Vec<String> = (0..2005).map(|i| format!(r#"{{"n":{i}}}"#)).collect();
        s.append_messages(long, None, &bodies).unwrap();
        s.append_messages(short, None, &bodies[..10]).unwrap();

        assert_eq!(s.trim_message_logs(2000).unwrap(), 5, "the cut is not the overflow");

        // The newest 2000, still in order: the oldest five went, and the
        // thread reads from `n:5` onward rather than coming back shuffled.
        let kept = s.conversation_messages(long, 5000).unwrap();
        assert_eq!(kept.len(), 2000);
        assert!(kept[0].1.contains(r#""n":5"#), "the wrong end was cut: {:?}", &kept[..3]);
        assert!(kept[1999].1.contains(r#""n":2004"#), "the newest row went");

        // Per conversation: the quiet thread is not touched by the busy
        // one's overflow.
        assert_eq!(s.conversation_messages(short, 5000).unwrap().len(), 10);

        // And a second pass has nothing left to do.
        assert_eq!(s.trim_message_logs(2000).unwrap(), 0);
    }

    #[test]
    fn appending_marks_the_thread_as_just_used() {
        // The sidebar orders by `updated_at` and the 48-hour sweep reads it.
        // `replace_messages` bumped it; an append that did not would let a
        // thread being talked in right now expire underneath the reader.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE conversations
                 SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(3600)
                 WHERE id = ?",
                params![c],
            )
            .unwrap();
        }
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap(), Some((c, true)));

        s.append_messages(c, None, &["{}".to_string()]).unwrap();

        assert!(
            s.latest_conversation(a, "direct", 600).unwrap().is_some_and(|(id, aged)| id == c && !aged),
            "a thread just appended to still reads as stale"
        );
    }

    #[test]
    fn a_quiet_conversation_ages_out_and_a_live_one_does_not() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let first = s.start_conversation(a, "direct").unwrap();

        // Inside the TTL: reported as live, so the caller keeps using it.
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap(), Some((first, false)));

        // Push it back beyond the TTL and the same call reports it stale.
        {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "UPDATE conversations
                 SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(3600)
                 WHERE id = ?",
                params![first],
            )
            .unwrap();
        }
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap(), Some((first, true)));

        // Speaking in it makes it live again without starting a new one.
        assert!(s.touch_conversation(first).unwrap(), "touching a thread that still exists must report true");
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap(), Some((first, false)));

        // An account with nothing has nothing — the caller starts the thread.
        let b = s.account_for_telegram(22).unwrap();
        assert_eq!(s.latest_conversation(b, "direct", 600).unwrap(), None);
    }

    #[test]
    fn where_to_reach_someone_moves_into_deliveries() {
        let (_d, path) = legacy_db();
        let s = Store::open(&path).unwrap();
        let conn = s.conn.lock().unwrap();

        let account_of = |tg: &str| -> i64 {
            conn.query_row(
                "SELECT account_id FROM identities WHERE kind='telegram' AND external_id = ?",
                params![tg], |r| r.get(0),
            ).unwrap()
        };

        let addr: String = conn
            .query_row("SELECT address FROM deliveries WHERE account_id = ? AND channel='telegram'",
                params![account_of("11")], |r| r.get(0)).unwrap();
        assert_eq!(addr, "555");

        // The waitlisted person had a chat_id but no user_chats row; the
        // waitlist was the only record of where to reach them, and losing it
        // would silently break the next announce.
        let addr: String = conn
            .query_row("SELECT address FROM deliveries WHERE account_id = ? AND channel='telegram'",
                params![account_of("33")], |r| r.get(0)).unwrap();
        assert_eq!(addr, "777");

        let gone: i64 = conn
            .query_row("SELECT count(*) FROM information_schema.tables WHERE table_name='user_chats'",
                [], |r| r.get(0)).unwrap();
        assert_eq!(gone, 0, "user_chats should be gone");
    }

    #[test]
    fn constrained_tables_are_rebuilt_around_accounts() {
        let (_d, path) = legacy_db();
        let s = Store::open(&path).unwrap();
        let conn = s.conn.lock().unwrap();

        let account_of = |tg: &str| -> i64 {
            conn.query_row(
                "SELECT account_id FROM identities WHERE kind='telegram' AND external_id = ?",
                params![tg], |r| r.get(0),
            ).unwrap()
        };
        let (ann, bo, cy) = (account_of("11"), account_of("22"), account_of("33"));

        let fact: String = conn
            .query_row("SELECT value FROM user_facts WHERE account_id = ? AND key='currency'",
                params![ann], |r| r.get(0)).unwrap();
        assert_eq!(fact, "EUR");

        let name: String = conn
            .query_row("SELECT display_name FROM users WHERE account_id = ?", params![bo], |r| r.get(0)).unwrap();
        assert_eq!(name, "Bo");

        let code: String = conn
            .query_row("SELECT code FROM members WHERE account_id = ?", params![bo], |r| r.get(0)).unwrap();
        assert_eq!(code, "spring");

        let waiting: i64 = conn
            .query_row("SELECT count(*) FROM waitlist WHERE account_id = ?", params![cy], |r| r.get(0)).unwrap();
        assert_eq!(waiting, 1);

        let trip: String = conn
            .query_row("SELECT name FROM trips WHERE account_id = ?", params![ann], |r| r.get(0)).unwrap();
        assert_eq!(trip, "Lisbon");

        // The waitlisted person's chat id had to be rescued into deliveries
        // before the rebuild dropped the column.
        let addr: String = conn
            .query_row("SELECT address FROM deliveries WHERE account_id = ? AND channel='telegram'",
                params![cy], |r| r.get(0)).unwrap();
        assert_eq!(addr, "777");

        // The trips sequence still hands out fresh ids after the rebuild.
        conn.execute("INSERT INTO trips (account_id, name, name_key) VALUES (?, 'Porto', 'porto')",
            params![ann]).unwrap();
        let ids: i64 = conn.query_row("SELECT count(DISTINCT id) FROM trips", [], |r| r.get(0)).unwrap();
        assert_eq!(ids, 2, "a rebuilt table must not reuse an id");
    }

    #[test]
    fn unconstrained_tables_carry_account_ids_and_keep_their_rows() {
        let (_d, path) = legacy_db();
        let s = Store::open(&path).unwrap();
        let conn = s.conn.lock().unwrap();

        let ann: i64 = conn
            .query_row(
                "SELECT account_id FROM identities WHERE kind='telegram' AND external_id='11'",
                [], |r| r.get(0),
            )
            .unwrap();

        let purchases: i64 = conn
            .query_row("SELECT count(*) FROM purchases WHERE account_id = ?", params![ann], |r| r.get(0))
            .unwrap();
        assert_eq!(purchases, 1, "Ann's beans should have moved across");

        let logged: i64 = conn
            .query_row("SELECT count(*) FROM request_log WHERE account_id = ?", params![ann], |r| r.get(0))
            .unwrap();
        assert_eq!(logged, 2);

        // A reminder is delivered where it was created, so it carries its
        // own address rather than inheriting the account default.
        let addr: String = conn
            .query_row("SELECT address FROM reminders WHERE item = 'beans'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(addr, "555");

        // Ids and their sequences survive the rebuild.
        conn.execute("INSERT INTO purchases (account_id, item, store) VALUES (?, 'cable', 'eBay')",
            params![ann]).unwrap();
        let distinct: i64 = conn
            .query_row("SELECT count(DISTINCT id) FROM purchases", [], |r| r.get(0)).unwrap();
        let total: i64 = conn.query_row("SELECT count(*) FROM purchases", [], |r| r.get(0)).unwrap();
        assert_eq!(distinct, total, "a rebuilt table must not reuse an id");

        // Nothing orphaned anywhere.
        for table in ["purchases", "reminders", "request_log"] {
            let orphans: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table} WHERE account_id IS NULL"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(orphans, 0, "{table} has rows with no account");
        }
    }

    #[test]
    fn every_legacy_user_gets_exactly_one_account() {
        let (_d, path) = legacy_db();
        let s = Store::open(&path).unwrap();
        let conn = s.conn.lock().unwrap();

        // 11, 22 and 33 — 33 exists only on the waitlist.
        let accounts: i64 = conn.query_row("SELECT count(*) FROM accounts", [], |r| r.get(0)).unwrap();
        assert_eq!(accounts, 3);

        let ids: i64 = conn
            .query_row(
                "SELECT count(*) FROM identities WHERE kind = 'telegram' \
                 AND external_id IN ('11','22','33')",
                [], |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ids, 3);

        // No Telegram id claimed twice.
        let dupes: i64 = conn
            .query_row(
                "SELECT count(*) FROM (SELECT external_id FROM identities \
                 WHERE kind='telegram' GROUP BY external_id HAVING count(*) > 1)",
                [], |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dupes, 0);
    }

    #[test]
    fn the_legacy_fixture_has_no_accounts_yet() {
        let (_d, path) = legacy_db();
        let conn = Connection::open(&path).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM information_schema.tables WHERE table_name = 'accounts'",
                [], |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    fn new_purchase(item: &str, store: &str, purchased_at: Option<&str>) -> NewPurchase {
        NewPurchase {
            item: item.to_string(),
            store: store.to_string(),
            url: None,
            price: Some(9.99),
            currency: Some("EUR".to_string()),
            notes: None,
            purchased_at: purchased_at.map(str::to_string),
        }
    }

    #[test]
    fn the_new_tables_exist_after_opening() {
        let (s, _d) = test_store();
        let conn = s.conn.lock().unwrap();
        for table in ["accounts", "identities", "deliveries", "conversations", "messages"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM information_schema.tables WHERE table_name = ?",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table} should exist");
        }
    }

    #[test]
    fn a_fresh_store_has_somewhere_to_queue_a_mirror() {
        // Both tables are pure additions with nothing to migrate, so they
        // live in MIGRATIONS alone — `open` runs that batch every time, on
        // existing databases as well as new ones, and the numbered steps
        // exist only for transforms.
        let (s, _d) = test_store();
        let conn = s.conn.lock().unwrap();
        for table in ["outbox", "mirrored_accounts"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM information_schema.tables WHERE table_name = ?",
                    params![table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table} is missing");
        }
    }

    #[test]
    fn migration_steps_run_once_and_only_once() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.duckdb");

        let s = Store::open(&path).unwrap();
        let first = s.schema_version().unwrap();
        drop(s);

        // Re-opening must not re-run anything. A step that ran twice would
        // duplicate backfilled rows, so the version is the guard.
        let s = Store::open(&path).unwrap();
        assert_eq!(s.schema_version().unwrap(), first);
    }

    #[test]
    fn record_and_query_roundtrip() {
        let (s, _d) = test_store();
        let p = s
            .record_purchase(1, new_purchase("Lavazza coffee beans", "Amazon", Some("2026-06-28")))
            .unwrap();
        assert_eq!(p.id, 1);
        assert_eq!(p.item, "Lavazza coffee beans");

        let found = s.query_purchases(1, None, 10).unwrap();
        assert_eq!(found, vec![p]);
    }

    #[test]
    fn queries_are_scoped_per_user() {
        let (s, _d) = test_store();
        s.record_purchase(1, new_purchase("keyboard", "eBay", None)).unwrap();
        s.record_purchase(2, new_purchase("mouse", "eBay", None)).unwrap();

        let mine = s.query_purchases(1, None, 10).unwrap();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].item, "keyboard");
    }

    #[test]
    fn substring_match_is_case_insensitive_over_item_store_notes() {
        let (s, _d) = test_store();
        s.record_purchase(1, new_purchase("Coffee beans", "Amazon", None)).unwrap();
        s.record_purchase(1, new_purchase("Tea", "CoffeeCorner", None)).unwrap();
        let mut with_notes = new_purchase("Filters", "Bol", None);
        with_notes.notes = Some("for the coffee machine".to_string());
        s.record_purchase(1, with_notes).unwrap();
        s.record_purchase(1, new_purchase("Socks", "Zalando", None)).unwrap();

        let found = s.query_purchases(1, Some("COFFEE"), 10).unwrap();
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn newest_purchase_first_and_limit_respected() {
        let (s, _d) = test_store();
        s.record_purchase(1, new_purchase("old", "A", Some("2026-01-01"))).unwrap();
        s.record_purchase(1, new_purchase("new", "A", Some("2026-06-01"))).unwrap();
        s.record_purchase(1, new_purchase("undated", "A", None)).unwrap();

        let found = s.query_purchases(1, None, 2).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].item, "new");
        assert_eq!(found[1].item, "old");
    }

    #[test]
    fn create_list_cancel_reminder() {
        let (s, _d) = test_store();
        let r = s.create_reminder(1, "telegram", "10", "coffee", 30, "2026-08-01").unwrap();
        assert_eq!(r.id, 1);

        let listed = s.list_reminders(1).unwrap();
        assert_eq!(listed, vec![r.clone()]);
        assert!(s.list_reminders(2).unwrap().is_empty());

        assert!(s.cancel_reminder(1, r.id).unwrap());
        assert!(s.list_reminders(1).unwrap().is_empty());
        // second cancel is a no-op
        assert!(!s.cancel_reminder(1, r.id).unwrap());
    }

    #[test]
    fn cancel_is_scoped_to_owner() {
        let (s, _d) = test_store();
        let r = s.create_reminder(1, "telegram", "10", "coffee", 30, "2026-08-01").unwrap();
        assert!(!s.cancel_reminder(2, r.id).unwrap());
        assert_eq!(s.list_reminders(1).unwrap().len(), 1);
    }

    #[test]
    fn due_reminders_selects_past_and_today_only() {
        let (s, _d) = test_store();
        s.create_reminder(1, "telegram", "10", "overdue", 30, "2026-07-01").unwrap();
        s.create_reminder(1, "telegram", "10", "today", 30, "2026-07-22").unwrap();
        s.create_reminder(1, "telegram", "10", "future", 30, "2026-09-01").unwrap();
        let cancelled = s.create_reminder(1, "telegram", "10", "cancelled", 30, "2026-07-01").unwrap();
        s.cancel_reminder(1, cancelled.id).unwrap();
        s.create_reminder(2, "telegram", "20", "other-user", 30, "2026-07-02").unwrap();

        let due = s.due_reminders("2026-07-22").unwrap();
        let items: Vec<_> = due.iter().map(|r| r.item.as_str()).collect();
        assert_eq!(items, vec!["overdue", "other-user", "today"]);
    }

    #[test]
    fn set_next_due_updates() {
        let (s, _d) = test_store();
        let r = s.create_reminder(1, "telegram", "10", "coffee", 30, "2026-07-01").unwrap();
        s.set_next_due(r.id, "2026-08-01").unwrap();
        assert!(s.due_reminders("2026-07-22").unwrap().is_empty());
        assert_eq!(s.list_reminders(1).unwrap()[0].next_due, "2026-08-01");
    }

    #[test]
    fn facts_upsert_overwrites_and_lists_sorted() {
        let (s, _d) = test_store();
        s.upsert_fact(1, "shoe_size", "43").unwrap();
        s.upsert_fact(1, "delivery_country", "NL").unwrap();
        s.upsert_fact(1, "shoe_size", "44").unwrap();

        assert_eq!(
            s.list_facts(1).unwrap(),
            vec![
                ("delivery_country".to_string(), "NL".to_string()),
                ("shoe_size".to_string(), "44".to_string()),
            ]
        );
    }

    #[test]
    fn facts_are_scoped_per_user() {
        let (s, _d) = test_store();
        s.upsert_fact(1, "delivery_country", "NL").unwrap();
        assert!(s.list_facts(2).unwrap().is_empty());
        assert!(!s.forget_fact(2, "delivery_country").unwrap());
        assert_eq!(s.list_facts(1).unwrap().len(), 1);
    }

    #[test]
    fn usage_stats_for_is_scoped_to_one_user() {
        // /stat pulls from this method so a per-user query never sees
        // anyone else's request count.
        let (s, _d) = test_store();
        s.log_request_at(1, "text", "2026-07-25 10:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-25 11:00:00").unwrap();
        s.log_request_at(2, "photo", "2026-07-25 12:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-20 09:00:00").unwrap(); // before cutoff

        let mine = s.usage_stats_for("2026-07-25 00:00:00", 1).unwrap();
        assert_eq!(
            mine,
            vec![(1, "2026-07-25".to_string(), 2)],
            "user 1 should see only their own rows"
        );

        let theirs = s.usage_stats_for("2026-07-25 00:00:00", 2).unwrap();
        assert_eq!(
            theirs,
            vec![(2, "2026-07-25".to_string(), 1)],
            "user 2 must not see user 1's counts"
        );

        let empty = s.usage_stats_for("2026-07-25 00:00:00", 99).unwrap();
        assert!(empty.is_empty(), "unknown user sees nothing");
    }

    #[test]
    fn broadcast_targets_are_the_chats_people_actually_talk_in() {
        // A user id is not a chat id — they coincide in a private chat and
        // do not in a group. Announcements have to go where the
        // conversation happened, so the chat is recorded rather than
        // assumed.
        let (s, _d) = test_store();
        s.note_delivery(1, "telegram", "1").unwrap();
        s.note_delivery(2, "telegram", "-100200300").unwrap();
        assert_eq!(s.broadcast_targets().unwrap(), vec![(1, 1), (2, -100200300)]);

        // Moving chats replaces the old one: an announcement should follow
        // the person, not accumulate copies.
        s.note_delivery(1, "telegram", "-999").unwrap();
        assert_eq!(s.broadcast_targets().unwrap(), vec![(1, -999), (2, -100200300)]);
    }

    #[test]
    fn flight_searches_are_counted_apart_from_ordinary_requests() {
        // Every flight search is billed by Duffel, so these need their own
        // number rather than being buried in the request total.
        let (s, _d) = test_store();
        s.log_request_at(1, "text", "2026-07-25 10:00:00").unwrap();
        s.log_request_at(1, Store::FLIGHT_SEARCH, "2026-07-25 10:01:00").unwrap();
        s.log_request_at(1, Store::FLIGHT_SEARCH, "2026-07-25 10:02:00").unwrap();
        s.log_request_at(2, Store::FLIGHT_SEARCH, "2026-07-25 12:00:00").unwrap();
        s.log_request_at(1, Store::FLIGHT_SEARCH, "2026-07-20 09:00:00").unwrap(); // before cutoff

        let all = s.flight_searches_all("2026-07-25 00:00:00").unwrap();
        assert_eq!(all.get(&1), Some(&2), "the text request must not be counted");
        assert_eq!(all.get(&2), Some(&1));

        // Same access-control split as usage_stats: an ordinary caller sees
        // only their own.
        let mine = s.flight_searches_for("2026-07-25 00:00:00", 1).unwrap();
        assert_eq!(mine.get(&1), Some(&2));
        assert_eq!(mine.get(&2), None, "user 1 must not see user 2's searches");

        assert!(s
            .flight_searches_for("2026-07-25 00:00:00", 99)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn usage_stats_all_spans_every_user() {
        // The admin view. Same shape as the per-user query so /stat can
        // render either without knowing which it got.
        let (s, _d) = test_store();
        s.log_request_at(1, "text", "2026-07-25 10:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-25 11:00:00").unwrap();
        s.log_request_at(2, "photo", "2026-07-25 12:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-26 09:00:00").unwrap();
        s.log_request_at(1, "text", "2026-07-20 09:00:00").unwrap(); // before cutoff

        let rows = s.usage_stats_all("2026-07-25 00:00:00").unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "2026-07-25".to_string(), 2),
                (2, "2026-07-25".to_string(), 1),
                (1, "2026-07-26".to_string(), 1),
            ]
        );

        // live logging path writes with defaults and lands in stats
        s.log_request(3, "reaction").unwrap();
        let rows = s.usage_stats_all("2000-01-01 00:00:00").unwrap();
        assert!(rows.iter().any(|(u, _, _)| *u == 3));
    }

    #[test]
    fn display_names_track_the_latest_seen_name() {
        let (s, _d) = test_store();
        s.remember_user(1, "@alice").unwrap();
        s.remember_user(2, "Bob Jansen").unwrap();
        // Blank is not a name — the row is left absent so /stat falls back
        // to the bare id rather than printing an empty column.
        s.remember_user(4, "   ").unwrap();

        let names = s.display_names().unwrap();
        assert_eq!(names.get(&1).map(String::as_str), Some("@alice"));
        assert_eq!(names.get(&2).map(String::as_str), Some("Bob Jansen"));
        assert_eq!(names.get(&4), None);

        // Renaming overwrites rather than accumulating rows.
        s.remember_user(1, "@alice_new").unwrap();
        let names = s.display_names().unwrap();
        assert_eq!(names.len(), 2);
        assert_eq!(names.get(&1).map(String::as_str), Some("@alice_new"));
    }

    #[test]
    fn logging_a_request_does_not_invent_a_name() {
        // Requests and names are recorded independently: someone can appear
        // in /stat's counts long before the bot knows what to call them.
        let (s, _d) = test_store();
        s.log_request(7, "text").unwrap();
        assert!(s.display_names().unwrap().is_empty());
        assert_eq!(s.usage_stats_all("2000-01-01 00:00:00").unwrap().len(), 1);
    }

    #[test]
    fn forget_fact_removes_and_reports() {
        let (s, _d) = test_store();
        s.upsert_fact(1, "budget_style", "prefers cheap used gear").unwrap();
        assert!(s.forget_fact(1, "budget_style").unwrap());
        assert!(!s.forget_fact(1, "budget_style").unwrap());
        assert!(s.list_facts(1).unwrap().is_empty());
    }

    #[test]
    fn a_trip_is_found_by_name_case_insensitively_and_scoped_to_its_owner() {
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None, None).unwrap();
        assert_eq!(trip.name, "September");
        assert_eq!(trip.adults, 1, "one adult unless said otherwise");
        assert_eq!(trip.status, "planning");
        assert!(trip.items.is_empty());

        assert!(store.find_trip(7, "september").unwrap().is_some(), "names are not case-sensitive");
        assert!(store.find_trip(8, "September").unwrap().is_none(), "another user has no such trip");

        // Same name twice is the same trip, not a second one.
        store.upsert_trip(7, "SEPTEMBER", Some(2), Some("business"), None).unwrap();
        assert_eq!(store.list_trips(7).unwrap().len(), 1);
        let trip = store.find_trip(7, "September").unwrap().unwrap();
        assert_eq!(trip.adults, 2);
        assert_eq!(trip.cabin_class.as_deref(), Some("business"));
        assert_eq!(trip.name, "September", "the original spelling is kept");

        // Two users may each have a "September".
        store.upsert_trip(8, "September", None, None, None).unwrap();
        assert_eq!(store.list_trips(8).unwrap().len(), 1);
    }

    #[test]
    fn a_trip_carries_the_conversation_that_made_it() {
        // Nullable on purpose: NULL means orphaned, which is an ordinary
        // state a trip reaches by outliving its chat, not an error.
        let (store, _d) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        let owner: Option<i64> = store
            .conn()
            .query_row("SELECT conversation_id FROM trips WHERE id = ?", params![trip.id], |r| r.get(0))
            .unwrap();
        assert_eq!(owner, None, "a trip made outside a conversation has no owner");
    }

    #[test]
    fn the_chat_that_made_a_trip_keeps_it_and_an_orphan_is_adopted() {
        // Ownership is single because the composer needs exactly one place
        // to send to. A live owner is never displaced: otherwise deleting
        // your most recent chat would destroy a trip whose original
        // planning thread is still sitting there.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();

        let trip = store.upsert_trip(account, "Atlantic loop", None, None, Some(11)).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), Some(11));

        // A second chat extending the same trip must NOT take it.
        store.upsert_trip(account, "atlantic loop", None, None, Some(22)).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), Some(11), "a live owner is never displaced");

        // Orphaned, then touched again: adopted. Detached directly rather
        // than through `expire_conversations` because the owner ids here are
        // invented — this test is about adoption, and standing up real
        // conversations to age out would only obscure that.
        detach_trips_within(&store.conn(), &[11]).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), None);
        store.upsert_trip(account, "Atlantic loop", None, None, Some(33)).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), Some(33), "an orphan is adopted");
    }

    #[test]
    fn trip_chat_names_the_conversation_that_owns_the_trip() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let conversation_id = store.start_conversation(account, "direct").unwrap();
        store.set_thread_title(account, conversation_id, "Cheap flights in October").unwrap();
        let trip = store
            .upsert_trip(account, "Atlantic loop", None, None, Some(conversation_id))
            .unwrap();

        assert_eq!(
            store.trip_chat(trip.id).unwrap(),
            Some(TripChat {
                id: conversation_id,
                title: Some("Cheap flights in October".to_string()),
                scope: "direct".to_string(),
            }),
        );
    }

    #[test]
    fn trip_chat_is_none_for_an_orphan_and_for_a_conversation_row_that_is_gone() {
        // `conversation_id IS NULL` is the ordinary orphan. A `conversation_id`
        // that points at a row which no longer exists is not supposed to
        // happen — `delete_conversation` and expiry both clear it first — but
        // the read must not depend on that holding: a `LEFT JOIN` here would
        // silently promote the dangling id into a phantom chat in the UI, so
        // this proves the `JOIN` drops it instead.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let orphan = store.upsert_trip(account, "Orphaned", None, None, None).unwrap();
        assert_eq!(store.trip_chat(orphan.id).unwrap(), None);

        let conversation_id = store.start_conversation(account, "direct").unwrap();
        let dangling = store
            .upsert_trip(account, "Dangling", None, None, Some(conversation_id))
            .unwrap();
        store
            .conn()
            .execute("DELETE FROM conversations WHERE id = ?", params![conversation_id])
            .unwrap();
        assert_eq!(store.trip_chat(dangling.id).unwrap(), None);
    }

    #[test]
    fn detach_trips_within_an_empty_slice_is_a_no_op_not_a_syntax_error() {
        // `expire_conversations` calls this inside its own transaction with
        // a SELECT result that is empty on every sweep that expired nothing
        // — the ordinary hourly case, not an edge case. `IN ()` is a parser
        // error in DuckDB, so without the guard the routine hourly sweep is
        // the one that fails.
        let (store, _dir) = test_store();
        let conn = store.conn();
        assert_eq!(detach_trips_within(&conn, &[]).unwrap(), 0);
    }

    #[test]
    fn an_upsert_that_names_no_conversation_leaves_an_existing_owner_alone() {
        // Every current caller passes None here — no tool threads a real
        // conversation id yet, so this path runs on essentially every
        // upsert in production. If it unconditionally wrote NULL, it would
        // silently orphan a live trip on the very first edit after it was
        // created.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Japan in spring", None, None, Some(11)).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), Some(11));

        store.upsert_trip(account, "Japan in spring", Some(2), None, None).unwrap();
        assert_eq!(store.trip_owner(trip.id).unwrap(), Some(11), "an upsert naming no chat must not clear the owner");
    }

    /// `trips` exactly as it stood at schema version 8, before
    /// `conversation_id`. Frozen, like `LEGACY_SCHEMA`: its value is being
    /// an honest picture of the table step 9 will actually meet. Do not
    /// update this when `MIGRATIONS` widens `trips` again — the whole point
    /// is that this stays behind.
    const PRE_TRIP_CONVERSATION_TRIPS: &str = r#"
CREATE SEQUENCE trips_id_seq;
CREATE TABLE trips (
    id BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    account_id BIGINT NOT NULL, name TEXT NOT NULL,
    name_key TEXT NOT NULL, adults BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT, status TEXT NOT NULL DEFAULT 'planning',
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at TIMESTAMP NOT NULL DEFAULT current_timestamp,
    UNIQUE (account_id, name_key)
);
"#;

    #[test]
    fn an_existing_database_gains_the_column_by_migration() {
        // The fresh-database path and the upgrade path are different code.
        // A CREATE TABLE IF NOT EXISTS does nothing to a table that already
        // exists, so without step 9 every deployed database would be missing
        // this column while every test passed.
        //
        // The row is what matters: the step is trivially safe on an empty
        // table, and production will run it against a `trips` table that
        // already has rows in it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.duckdb");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            conn.execute_batch(PRE_TRIP_CONVERSATION_TRIPS).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version BIGINT NOT NULL);
                 INSERT INTO schema_version VALUES (8);
                 INSERT INTO trips (account_id, name, name_key) VALUES (1, 'Lisbon', 'lisbon');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let conversation_id: Option<i64> = store
            .conn()
            .query_row("SELECT conversation_id FROM trips WHERE name_key = 'lisbon'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(conversation_id, None, "a trip that predates the column must gain it, unset");
    }

    /// The `trips` table as it stood at schema 9, before `kept`. Do NOT update
    /// this when `MIGRATIONS` changes — its value is being an honest picture of
    /// the database step 10 will actually meet.
    const PRE_KEPT_TRIPS: &str = r#"
CREATE SEQUENCE IF NOT EXISTS trips_id_seq;
CREATE TABLE trips (
    id          BIGINT PRIMARY KEY DEFAULT nextval('trips_id_seq'),
    account_id     BIGINT NOT NULL,
    name        TEXT NOT NULL,
    name_key    TEXT NOT NULL,
    adults      BIGINT NOT NULL DEFAULT 1,
    cabin_class TEXT,
    status      TEXT NOT NULL DEFAULT 'planning',
    created_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at  TIMESTAMP NOT NULL DEFAULT current_timestamp,
    conversation_id BIGINT,
    UNIQUE (account_id, name_key)
);
"#;

    #[test]
    fn a_trip_the_model_builds_starts_as_a_draft() {
        // Building a trip is now free and automatic, so creating one is no
        // longer the act of intent it used to be. Keeping it is.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        assert!(!trip.kept, "a newly built trip is a draft until the traveller keeps it");
    }

    #[test]
    fn the_model_still_sees_the_draft_it_just_built() {
        // `trip_names` and `show_trip` feed the model. A specialist that
        // cannot find the trip it just built builds another one, and the
        // traveller ends up with two half-itineraries.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        store.upsert_trip(account, "Draft loop", None, None, None).unwrap();
        let names: Vec<String> =
            store.list_trips(account).unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["Draft loop".to_string()]);
        assert!(store.list_kept_trips(account).unwrap().is_empty());
    }

    #[test]
    fn keeping_a_trip_moves_it_from_the_draft_list_to_the_kept_one() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        store.upsert_trip(account, "Draft loop", None, None, None).unwrap();

        assert!(store.keep_trip(account, "Draft loop").unwrap());

        let kept: Vec<String> =
            store.list_kept_trips(account).unwrap().into_iter().map(|t| t.name).collect();
        assert_eq!(kept, vec!["Draft loop".to_string()]);
    }

    #[test]
    fn keeping_an_already_kept_trip_is_a_no_op_that_still_reports_success() {
        // "keep this" said twice is a traveller repeating themselves, not
        // an error — the tool should not surface a failure for it.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        store.upsert_trip(account, "Draft loop", None, None, None).unwrap();

        assert!(store.keep_trip(account, "Draft loop").unwrap());
        assert!(store.keep_trip(account, "Draft loop").unwrap());

        assert_eq!(store.list_kept_trips(account).unwrap().len(), 1);
    }

    #[test]
    fn keeping_a_trip_that_does_not_exist_reports_it_was_not_found() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        assert!(!store.keep_trip(account, "Nonexistent loop").unwrap());
    }

    #[test]
    fn every_trip_that_already_existed_is_kept_by_the_migration() {
        // Every trip in a database written before this column existed was
        // created under rules where making one *was* keeping it. Without the
        // backfill, this migration hides every trip a traveller already has
        // — which is the exact complaint the feature exists to answer,
        // inflicted on all their existing data.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.duckdb");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            conn.execute_batch(PRE_KEPT_TRIPS).unwrap();
            conn.execute_batch(
                "INSERT INTO trips (account_id, name, name_key) VALUES (1, 'Japan in spring', 'japan in spring');
                 CREATE TABLE schema_version (version BIGINT NOT NULL);
                 INSERT INTO schema_version VALUES (9);",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let kept: bool = store
            .conn()
            .query_row("SELECT kept FROM trips WHERE name_key = 'japan in spring'", [], |r| r.get(0))
            .unwrap();
        assert!(kept, "a trip that predates the column must survive as kept, not vanish");
    }

    #[test]
    fn upserting_one_field_leaves_the_other_field_untouched() {
        // Regression guard for the two-statement design: a naive single
        // `ON CONFLICT DO UPDATE SET adults = ?, cabin_class = ?` would
        // silently null out whichever field this call didn't mention.
        let (store, _d) = test_store();
        store.upsert_trip(7, "September", Some(2), Some("business"), None).unwrap();

        let trip = store.upsert_trip(7, "September", Some(3), None, None).unwrap();
        assert_eq!(trip.adults, 3);
        assert_eq!(
            trip.cabin_class.as_deref(),
            Some("business"),
            "cabin class must survive an upsert that didn't mention it"
        );

        let trip = store.upsert_trip(7, "September", None, Some("economy"), None).unwrap();
        assert_eq!(trip.adults, 3, "adults must survive an upsert that didn't mention it");
        assert_eq!(trip.cabin_class.as_deref(), Some("economy"));
    }

    #[test]
    fn changing_adults_or_cabin_class_resets_a_finalised_trip_to_planning() {
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", Some(2), Some("business"), None).unwrap();

        store.set_trip_status(trip.id, "finalised").unwrap();
        let trip = store.upsert_trip(7, "September", Some(3), None, None).unwrap();
        assert_eq!(
            trip.status, "planning",
            "changing the passenger count invalidates a finalised trip's prices"
        );

        store.set_trip_status(trip.id, "finalised").unwrap();
        let trip = store.upsert_trip(7, "September", None, Some("economy"), None).unwrap();
        assert_eq!(
            trip.status, "planning",
            "changing cabin class invalidates a finalised trip's prices"
        );

        // find-or-create — supplying neither field — must stay inert.
        store.set_trip_status(trip.id, "finalised").unwrap();
        let trip = store.upsert_trip(7, "September", None, None, None).unwrap();
        assert_eq!(
            trip.status, "finalised",
            "an upsert with nothing to change must not reset status"
        );
    }

    #[test]
    fn items_stay_contiguous_through_inserts_and_drops() {
        // Positions are how the traveller refers to an item ("drop the
        // second leg"), so a hole would make every later instruction target
        // the wrong row.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None, None).unwrap();
        for (o, d, date) in [("AMS", "LIS", "2026-09-03"), ("LIS", "FCO", "2026-09-07")] {
            store.add_flight(trip.id, o, d, date).unwrap();
        }
        let trip = store.add_flight(trip.id, "BCN", "MAD", "2026-09-05").unwrap();
        assert_eq!(
            trip.items.iter().map(|s| (s.position, s.origin.as_deref().unwrap_or(""))).collect::<Vec<_>>(),
            vec![(1, "AMS"), (2, "BCN"), (3, "LIS")],
            "a leg dated between the others lands between them and the rest renumber"
        );

        let trip = store.drop_item(trip.id, 1).unwrap();
        assert_eq!(
            trip.items.iter().map(|s| (s.position, s.origin.as_deref().unwrap_or(""))).collect::<Vec<_>>(),
            vec![(1, "BCN"), (2, "LIS")],
            "dropping the first renumbers what is left from 1"
        );

        // A position nobody has is refused rather than silently doing nothing.
        assert!(store.drop_item(trip.id, 9).is_err());
    }

    /// Every leg of a trip as (position, origin, date), for reading an
    /// assertion failure without decoding a struct.
    fn legs(trip: &Trip) -> Vec<(i64, &str, &str)> {
        trip.items
            .iter()
            .map(|s| (s.position, s.origin.as_deref().unwrap_or(""), s.date.as_str()))
            .collect()
    }

    #[test]
    fn a_leg_added_with_no_position_lands_where_its_date_belongs() {
        // The traveller who has already planned September 3rd and 7th and
        // then remembers the hop that gets them to AMS on the 1st. Appending
        // put it last, and dates_run_forwards then refused to price the trip
        // at all: the system could see the order was wrong and would not fix
        // it.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None, None).unwrap();
        for (o, d, date) in [("AMS", "LIS", "2026-09-03"), ("LIS", "FCO", "2026-09-07")] {
            store.add_flight(trip.id, o, d, date).unwrap();
        }

        let trip = store.add_flight(trip.id, "BER", "AMS", "2026-09-01").unwrap();
        assert_eq!(
            legs(&trip),
            vec![
                (1, "BER", "2026-09-01"),
                (2, "AMS", "2026-09-03"),
                (3, "LIS", "2026-09-07"),
            ],
            "a leg dated before the first one belongs first, not last"
        );

        // And in the middle, ahead of the first leg that leaves after it.
        let trip = store.add_flight(trip.id, "FCO", "MAD", "2026-09-05").unwrap();
        assert_eq!(
            legs(&trip),
            vec![
                (1, "BER", "2026-09-01"),
                (2, "AMS", "2026-09-03"),
                (3, "FCO", "2026-09-05"),
                (4, "LIS", "2026-09-07"),
            ],
            "a leg dated between two others belongs between them"
        );
    }

    #[test]
    fn a_leg_dated_after_every_other_one_still_goes_last() {
        // The ordinary case, and the reason this change is safe: a trip
        // built front to back gets exactly the positions appending gave it.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None, None).unwrap();
        for (o, d, date) in [
            ("AMS", "LIS", "2026-09-03"),
            ("LIS", "FCO", "2026-09-07"),
            ("FCO", "AMS", "2026-09-11"),
        ] {
            store.add_flight(trip.id, o, d, date).unwrap();
        }
        assert_eq!(
            legs(&store.find_trip(7, "September").unwrap().unwrap()),
            vec![
                (1, "AMS", "2026-09-03"),
                (2, "LIS", "2026-09-07"),
                (3, "FCO", "2026-09-11"),
            ],
            "building a trip in date order must be untouched by this"
        );
    }

    #[test]
    fn a_leg_sharing_a_date_goes_behind_the_legs_already_on_it() {
        // A same-day connection is an ordinary thing, and the tie cannot be
        // broken by time of day: a leg that has just been added carries no
        // candidates, so nothing about it says when it leaves. Last on its
        // date is the only answer available, and it is also the one the
        // traveller typing legs in order expects.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None, None).unwrap();
        for (o, d, date) in [
            ("AMS", "LIS", "2026-09-03"),
            ("LIS", "FCO", "2026-09-03"),
            ("FCO", "AMS", "2026-09-09"),
        ] {
            store.add_flight(trip.id, o, d, date).unwrap();
        }

        let trip = store.add_flight(trip.id, "FCO", "MAD", "2026-09-03").unwrap();
        assert_eq!(
            legs(&trip),
            vec![
                (1, "AMS", "2026-09-03"),
                (2, "LIS", "2026-09-03"),
                (3, "FCO", "2026-09-03"),
                (4, "FCO", "2026-09-09"),
            ],
            "the new leg goes after the legs already on its date, and before the later one"
        );
    }

    #[test]
    fn a_trip_built_in_any_order_never_reads_as_running_backwards() {
        // The failure this fixes, stated as the check that reported it:
        // dates_run_forwards is what made such a trip un-priceable, so the
        // legs arriving in the worst possible order must leave it silent.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None, None).unwrap();
        for (o, d, date) in [
            ("FCO", "AMS", "2026-09-11"),
            ("AMS", "LIS", "2026-09-03"),
            ("LIS", "FCO", "2026-09-07"),
            ("BER", "AMS", "2026-09-01"),
        ] {
            store.add_flight(trip.id, o, d, date).unwrap();
        }
        let trip = store.find_trip(7, "September").unwrap().unwrap();
        assert_eq!(
            crate::tools::trips::dates_run_forwards(&trip.items),
            Ok(()),
            "legs added in any order must still read as one journey: {:?}",
            legs(&trip)
        );
    }

    #[test]
    fn a_leg_inserted_by_its_date_carries_every_parked_option_with_its_segment() {
        // Every write renumbers, so a renumber that moved items and not
        // their candidates would hand somebody's chosen flight to a
        // different city pair while the trip still looked perfectly
        // well-formed. Candidates are keyed by item id for exactly this
        // reason; this is the check that they stay with their leg.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None, None).unwrap();
        for (o, d, date) in [("AMS", "LIS", "2026-09-03"), ("LIS", "FCO", "2026-09-07")] {
            store.add_flight(trip.id, o, d, date).unwrap();
        }
        store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "LIS", Some("2026-09-03")),
                candidate("KLM", "KL1693", "2026-09-03T10:05:00"),
                true,
            )
            .unwrap();
        store
            .add_candidate(
                trip.id,
                2,
                expected("LIS", "FCO", Some("2026-09-07")),
                candidate("TAP", "TP830", "2026-09-07T10:05:00"),
                true,
            )
            .unwrap();

        // No position given: the date puts this at the front, and both
        // existing legs shift down.
        let trip = store.add_flight(trip.id, "BER", "AMS", "2026-09-01").unwrap();
        let by_route: Vec<(&str, &str, Vec<&str>)> = trip
            .items
            .iter()
            .map(|s| {
                (
                    s.origin.as_deref().unwrap_or(""),
                    s.destination.as_deref().unwrap_or(""),
                    s.candidates.iter().map(|c| c.flight_numbers.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            by_route,
            vec![
                ("BER", "AMS", vec![]),
                ("AMS", "LIS", vec!["KL1693"]),
                ("LIS", "FCO", vec!["TP830"]),
            ],
            "each parked option must move with its own route, not stay pinned to its old position"
        );
    }

    #[test]
    fn a_stale_remove_refuses_rather_than_deleting_the_wrong_leg() {
        // `drop_segment` renumbers: removing position 1 shifts position 2
        // down to 1. A browser tab holding a trip drawn thirty seconds ago
        // is therefore one concurrent edit away from asking to delete
        // "leg 2" and destroying a leg that is no longer the one it drew.
        // `add_candidate` already guards this way and says why; this is the
        // same guard on the same hazard.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        store.add_flight(trip.id, "LIS", "FCO", "2026-10-14").unwrap();

        // The browser drew both legs, then someone removed the first.
        store.drop_item(trip.id, 1).unwrap();

        // The stale click: "remove leg 2", which the browser believes is
        // LIS→FCO. After the renumber, position 2 does not exist and
        // position 1 IS LIS→FCO.
        let stale = expected("LIS", "FCO", Some("2026-10-14"));
        assert!(!store.remove_item_checked(trip.id, 2, stale).unwrap(),
            "a position that no longer exists must refuse");

        let after = store.find_trip(account, "Atlantic loop").unwrap().unwrap();
        assert_eq!(after.items.len(), 1, "the surviving leg is untouched");
        assert_eq!(after.items[0].destination.as_deref(), Some("FCO"));
    }

    #[test]
    fn a_stale_remove_refuses_when_the_renumber_left_a_different_leg_at_that_position() {
        // The other half of the hazard: the position still exists, so an
        // existence check alone lets the delete through — onto whichever leg
        // the renumber slid into that slot.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        store.add_flight(trip.id, "LIS", "FCO", "2026-10-14").unwrap();

        store.drop_item(trip.id, 1).unwrap();

        // "Remove leg 1", which the browser drew as AMS→LIS. Position 1 is
        // now LIS→FCO, a leg the traveller never asked to lose.
        let stale = expected("AMS", "LIS", Some("2026-10-12"));
        assert!(!store.remove_item_checked(trip.id, 1, stale).unwrap(),
            "a position holding a different route must refuse");

        let after = store.find_trip(account, "Atlantic loop").unwrap().unwrap();
        assert_eq!(after.items.len(), 1, "the surviving leg is untouched");
        assert_eq!(after.items[0].destination.as_deref(), Some("FCO"));
    }

    #[test]
    fn a_remove_that_matches_what_the_reader_saw_goes_through() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        store.add_flight(trip.id, "LIS", "FCO", "2026-10-14").unwrap();

        let seen = expected("LIS", "FCO", Some("2026-10-14"));
        assert!(store.remove_item_checked(trip.id, 2, seen).unwrap());
        let after = store.find_trip(account, "Atlantic loop").unwrap().unwrap();
        assert_eq!(after.items.len(), 1);
        assert_eq!(after.items[0].destination.as_deref(), Some("LIS"));
    }

    #[test]
    fn a_remove_with_no_date_to_check_still_checks_the_route() {
        // `None` is "nothing to verify", exactly as `add_candidate` reads
        // it — not "verified", and not a way past the route check.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Atlantic loop", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();

        let wrong_route = expected("LIS", "FCO", None);
        assert!(!store.remove_item_checked(trip.id, 1, wrong_route).unwrap());

        let undated = expected("AMS", "LIS", None);
        assert!(store.remove_item_checked(trip.id, 1, undated).unwrap());
        assert!(store.find_trip(account, "Atlantic loop").unwrap().unwrap().items.is_empty());
    }

    #[test]
    fn editing_a_trip_puts_it_back_to_planning() {
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "September", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-09-03").unwrap();
        store.set_trip_status(trip.id, "finalised").unwrap();
        assert_eq!(store.find_trip(7, "September").unwrap().unwrap().status, "finalised");

        let trip = store.add_flight(trip.id, "LIS", "AMS", "2026-09-10").unwrap();
        assert_eq!(trip.status, "planning", "the trip changed, so its pricing no longer describes it");
    }

    #[test]
    fn adding_a_segment_to_a_nonexistent_trip_fails_without_writing_anything() {
        // The insert used to run before any existence check, so a bad
        // trip_id left a segment row that could never be read back — every
        // read path goes through a trip, and this trip does not exist.
        let (store, _d) = test_store();
        assert!(store.add_flight(999, "AMS", "LIS", "2026-09-03").is_err());

        let conn = store.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM trip_items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "a failed add_flight must not leave an orphaned row");
    }

    /// What a flight tool passes `add_candidate`: all three checked.
    fn expected<'a>(origin: &'a str, destination: &'a str, date: Option<&'a str>) -> ExpectedItem<'a> {
        ExpectedItem { origin: Some(origin), destination: Some(destination), title: None, date }
    }

    fn candidate(airline: &str, numbers: &str, departing: &str) -> NewCandidate {
        NewCandidate {
            airline: airline.to_string(),
            flight_numbers: numbers.to_string(),
            itinerary: format!("{numbers} somewhere"),
            departing_at_local: Some(departing.to_string()),
            arriving_at_local: Some("2026-09-03T12:15:00".to_string()),
            duration_minutes: Some(130),
            quoted_price: Some(100.0),
            quoted_currency: Some("EUR".to_string()),
            source: Some("duffel".to_string()),
        }
    }

    #[test]
    fn a_segment_holds_several_options_and_at_most_one_is_chosen() {
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None, None).unwrap();
        let trip = store.add_flight(trip.id, "AMS", "NRT", "2026-09-03").unwrap();

        // Parked undecided: the traveller is comparing a nonstop against a
        // one-stop through Hong Kong.
        store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                false,
            )
            .unwrap();
        let trip = store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("Cathay", "CX270,CX500", "2026-09-03T13:30:00"),
                false,
            )
            .unwrap();
        let options = &trip.items[0].candidates;
        assert_eq!(options.len(), 2);
        assert_eq!(options.iter().map(|c| c.candidate).collect::<Vec<_>>(), vec![1, 2]);
        assert!(options.iter().all(|c| !c.chosen), "nothing decided yet");

        let trip = store.choose_candidate(trip.id, 1, 2).unwrap();
        let options = &trip.items[0].candidates;
        assert!(!options[0].chosen);
        assert!(options[1].chosen);

        // Choosing again moves the flag rather than setting a second one.
        let trip = store.choose_candidate(trip.id, 1, 1).unwrap();
        let options = &trip.items[0].candidates;
        assert_eq!(options.iter().filter(|c| c.chosen).count(), 1);
        assert!(options[0].chosen);

        // A candidate the segment does not have.
        assert!(store.choose_candidate(trip.id, 1, 9).is_err());

        let trip = store.drop_candidate(trip.id, 1, 1).unwrap();
        assert_eq!(trip.items[0].candidates.len(), 1, "the segment survives losing an option");
        assert_eq!(
            trip.items[0].candidates[0].candidate, 2,
            "dropping the lower-numbered candidate must not renumber the one that is left"
        );
    }

    #[test]
    fn a_dropped_candidate_number_is_never_handed_to_a_later_one() {
        // Candidate numbers are traveller-facing: "go with option 2" refers
        // to a number, not a position in a list. Deriving the next number
        // from max(candidate) over the *live* rows recycles a number the
        // moment its holder is dropped — so a traveller who was shown
        // "option 2", dropped it, and later says "go with option 2" would
        // silently be given a different flight under the same name.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None, None).unwrap();
        let trip = store.add_flight(trip.id, "AMS", "NRT", "2026-09-03").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                false,
            )
            .unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("Cathay", "CX270,CX500", "2026-09-03T13:30:00"),
                false,
            )
            .unwrap();

        // Drop the highest-numbered candidate, then add a fresh one.
        store.drop_candidate(trip.id, 1, 2).unwrap();
        let trip = store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("ANA", "NH205", "2026-09-03T09:00:00"),
                false,
            )
            .unwrap();

        let numbers: Vec<i64> = trip.items[0].candidates.iter().map(|c| c.candidate).collect();
        assert_eq!(
            numbers,
            vec![1, 3],
            "the new candidate must take the next never-used number, not the one just dropped (2)"
        );
    }

    #[test]
    fn adding_a_candidate_reports_a_missing_trip_separately_from_a_missing_segment() {
        // Both counts can read 0, but they call for different fixes — one
        // means the trip id is wrong, the other that the segment position
        // is. Collapsing them into one message ("no segment N") would send
        // a caller with a bad trip id looking for a segment that was never
        // going to exist.
        let (store, _d) = test_store();
        let err = store
            .add_candidate(
                999,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                false,
            )
            .unwrap_err();
        assert_eq!(err.to_string(), "no such trip", "a nonexistent trip must not be reported as a missing segment");

        let trip = store.upsert_trip(7, "Japan", None, None, None).unwrap();
        let err = store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                false,
            )
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "this trip has no segment 1",
            "a real trip with no such segment must not be reported as no such trip"
        );
    }

    #[test]
    fn adding_a_decided_option_marks_it_chosen_in_one_call() {
        // The ordinary path — "book me on this one" — must not need a second
        // call to say what it obviously meant.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None, None).unwrap();
        let trip = store.add_flight(trip.id, "AMS", "NRT", "2026-09-03").unwrap();
        let trip = store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                true,
            )
            .unwrap();
        assert!(trip.items[0].candidates[0].chosen);
    }

    #[test]
    fn a_shifted_segment_keeps_its_own_options() {
        // Positions are recomputed on every write, so a renumber that moved
        // items but not their candidates would silently reattach somebody's
        // chosen flight to a different route while the trip still looked
        // perfectly well-formed. Candidates are keyed by item id for exactly
        // this reason; this is the check.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None, None).unwrap();
        for (o, d, date) in [
            ("AMS", "NRT", "2026-09-03"),
            ("NRT", "OSA", "2026-09-10"),
            ("OSA", "AMS", "2026-09-17"),
        ] {
            store.add_flight(trip.id, o, d, date).unwrap();
        }
        // One chosen candidate per segment, identifiable by flight number.
        store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                true,
            )
            .unwrap();
        store
            .add_candidate(
                trip.id,
                2,
                expected("NRT", "OSA", Some("2026-09-10")),
                candidate("ANA", "NH2001", "2026-09-10T08:00:00"),
                true,
            )
            .unwrap();
        store
            .add_candidate(
                trip.id,
                3,
                expected("OSA", "AMS", Some("2026-09-17")),
                candidate("KLM", "KL862", "2026-09-17T11:00:00"),
                true,
            )
            .unwrap();

        // A leg dated before every other one lands at position 1: AMS-NRT,
        // NRT-OSA, OSA-AMS all shift down one.
        let trip = store.add_flight(trip.id, "AMS", "HEL", "2026-09-02").unwrap();
        let by_route: Vec<(String, String, Vec<String>)> = trip
            .items
            .iter()
            .map(|s| {
                (
                    s.origin.clone().unwrap_or_default(),
                    s.destination.clone().unwrap_or_default(),
                    s.candidates.iter().map(|c| c.flight_numbers.clone()).collect(),
                )
            })
            .collect();
        assert_eq!(
            by_route,
            vec![
                ("AMS".to_string(), "HEL".to_string(), vec![]),
                ("AMS".to_string(), "NRT".to_string(), vec!["KL861".to_string()]),
                ("NRT".to_string(), "OSA".to_string(), vec!["NH2001".to_string()]),
                ("OSA".to_string(), "AMS".to_string(), vec!["KL862".to_string()]),
            ],
            "each segment's chosen flight must move with its own route, not stay pinned to its old position"
        );

        // Now drop a middle segment (NRT-OSA, now at position 3) and check
        // again: the remaining segments must still carry their own options.
        let trip = store.drop_item(trip.id, 3).unwrap();
        let by_route: Vec<(String, String, Vec<String>)> = trip
            .items
            .iter()
            .map(|s| {
                (
                    s.origin.clone().unwrap_or_default(),
                    s.destination.clone().unwrap_or_default(),
                    s.candidates.iter().map(|c| c.flight_numbers.clone()).collect(),
                )
            })
            .collect();
        assert_eq!(
            by_route,
            vec![
                ("AMS".to_string(), "HEL".to_string(), vec![]),
                ("AMS".to_string(), "NRT".to_string(), vec!["KL861".to_string()]),
                ("OSA".to_string(), "AMS".to_string(), vec!["KL862".to_string()]),
            ],
            "after closing the gap each surviving segment must still carry its own chosen flight, not a neighbour's"
        );
    }

    #[test]
    fn add_candidate_refuses_when_the_route_or_date_no_longer_matches_what_was_checked() {
        // The caller validates a flight against a `Trip` it read earlier,
        // but that read and this write are two separate lock acquisitions —
        // a concurrent add_trip_segment or drop_trip_segment could
        // renumber positions in between. So the check has to run again
        // here, inside the same lock as the insert, against what the
        // caller says it validated rather than trusting the earlier read
        // to still be true.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None, None).unwrap();
        let trip = store.add_flight(trip.id, "AMS", "NRT", "2026-09-03").unwrap();

        let err = store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "LIS", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                false,
            )
            .unwrap_err();
        assert!(err.to_string().contains("AMS") && err.to_string().contains("NRT"), "got: {err}");

        let err = store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-05")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                false,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("2026-09-03") && err.to_string().contains("2026-09-05"),
            "got: {err}"
        );

        // Matching values still insert.
        let trip = store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                false,
            )
            .unwrap();
        assert_eq!(trip.items[0].candidates.len(), 1);

        // No usable date to check is not the same as a checked mismatch.
        let trip = store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", None),
                candidate("ANA", "NH205", "2026-09-03T09:00:00"),
                false,
            )
            .unwrap();
        assert_eq!(trip.items[0].candidates.len(), 2, "None means nothing to check, not a refusal");
    }

    #[test]
    fn choosing_an_option_after_the_trip_is_deleted_says_so_rather_than_no_such_option() {
        // choose_within only ever checks the segment_candidates row, which
        // a deleted trip has none of — indistinguishable, from there, from
        // a numbering mistake on a trip that still exists. choose_candidate
        // has to check the trip itself first so the two are told apart.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None, None).unwrap();
        let trip = store.add_flight(trip.id, "AMS", "NRT", "2026-09-03").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                false,
            )
            .unwrap();
        store.delete_trip(7, "Japan").unwrap();

        let err = store.choose_candidate(trip.id, 1, 1).unwrap_err();
        assert_eq!(err.to_string(), "no such trip", "a deleted trip must not read as a bad option number");
    }

    #[test]
    fn moving_a_segment_to_another_date_drops_the_options_it_invalidates() {
        // "Make it the 26th" is the commonest edit there is, and without it
        // the only way to honour one was to delete the trip and rebuild —
        // which is exactly what happened in production.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None, None).unwrap();
        let trip = store.add_flight(trip.id, "HND", "AMS", "2026-09-27").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                expected("HND", "AMS", Some("2026-09-27")),
                candidate("China Southern", "CZ324,CZ307", "2026-09-27T12:00:00"),
                true,
            )
            .unwrap();

        let (trip, dropped, changed) =
            store.update_flight(trip.id, 1, None, None, Some("2026-09-26")).unwrap();
        assert!(changed);
        assert_eq!(trip.items[0].date, "2026-09-26");
        assert_eq!(dropped, 1, "an option for the 27th is not an option for the 26th");
        assert!(
            trip.items[0].candidates.is_empty(),
            "keeping it would leave a flight bound to a day it does not fly"
        );
        assert_eq!(trip.items[0].origin.as_deref(), Some("HND"), "what was not asked for does not change");
    }

    #[test]
    fn a_segment_edit_that_changes_nothing_keeps_the_options() {
        // Restating the same date must not throw away work, the same way an
        // upsert that supplies nothing leaves a trip alone.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Japan", None, None, None).unwrap();
        let trip = store.add_flight(trip.id, "HND", "AMS", "2026-09-27").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                expected("HND", "AMS", Some("2026-09-27")),
                candidate("China Southern", "CZ324,CZ307", "2026-09-27T12:00:00"),
                true,
            )
            .unwrap();

        let (trip, dropped, changed) =
            store.update_flight(trip.id, 1, Some("HND"), None, Some("2026-09-27")).unwrap();
        assert!(!changed, "nothing differed, so nothing was written");
        assert_eq!(dropped, 0);
        assert_eq!(trip.items[0].candidates.len(), 1, "nothing changed, so nothing is lost");

        // And a position that does not exist is refused rather than ignored.
        assert!(store.update_flight(trip.id, 9, None, None, Some("2026-09-26")).is_err());
    }

    #[test]
    fn deleting_a_trip_takes_its_segments_and_options_with_it() {
        // Creating a trip is a side effect of a typo, so a typo needs an undo.
        let (store, _d) = test_store();
        let trip = store.upsert_trip(7, "Setpember", None, None, None).unwrap();
        let trip = store.add_flight(trip.id, "AMS", "LIS", "2026-09-03").unwrap();
        store
            .add_candidate(
                trip.id,
                1,
                expected("AMS", "LIS", Some("2026-09-03")),
                candidate("TAP", "TP675", "2026-09-03T10:05:00"),
                true,
            )
            .unwrap();

        assert!(store.delete_trip(7, "setpember").unwrap());
        assert!(store.find_trip(7, "Setpember").unwrap().is_none());
        assert!(!store.delete_trip(7, "Setpember").unwrap(), "deleting twice is not an error");

        // The name said this and only `find_trip` was checked, which cannot
        // see either child table: both are reached by `trip_id` alone, so a
        // segment or a parked option left behind after its trip is gone is
        // unreachable by every read path there is and stays for good.
        let orphans: i64 = store
            .conn()
            .query_row(
                "SELECT (SELECT count(*) FROM trip_items WHERE trip_id = ?)
                      + (SELECT count(*) FROM item_candidates c
                         WHERE NOT EXISTS (SELECT 1 FROM trip_items i WHERE i.id = c.item_id))",
                params![trip.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0, "the trip's segments and parked options went with it");

        // Another user's trip of the same name is untouched.
        store.upsert_trip(8, "Setpember", None, None, None).unwrap();
        assert!(!store.delete_trip(7, "Setpember").unwrap());
        assert!(store.find_trip(8, "Setpember").unwrap().is_some());
    }

    #[test]
    fn a_version_13_trip_becomes_flight_items_with_its_options_in_date_order() {
        // Two legs stored out of date order (the return first), one chosen
        // option and one undecided pair. After the migration: two flight
        // items, positions by date, every option re-keyed to its item.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.duckdb");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            // The fresh schema still creates the old tables (they are dropped
            // in a later release), so the fixture is: everything, minus the
            // two new tables, recorded as version 13.
            conn.execute_batch(MIGRATIONS).unwrap();
            conn.execute_batch("DROP TABLE IF EXISTS item_candidates; DROP TABLE IF EXISTS trip_items;").unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL);
                 DELETE FROM schema_version; INSERT INTO schema_version VALUES (13);
                 INSERT INTO trips (id, account_id, name, name_key, kept) VALUES (1, 1, 'Lisbon', 'lisbon', true);
                 INSERT INTO trip_segments VALUES (1, 1, 'LIS', 'AMS', '2026-10-19', 3), (1, 2, 'AMS', 'LIS', '2026-10-12', 2);
                 INSERT INTO segment_candidates (trip_id, position, candidate, chosen, airline, flight_numbers, itinerary, departing_at_local)
                 VALUES (1, 1, 1, false, 'TAP', 'TP670', 'LIS 18:40 19.10 ✈ AMS 22:55 19.10', '2026-10-19T18:40:00'),
                        (1, 1, 2, false, 'KLM', 'KL1696', 'LIS 06:00 19.10 ✈ AMS 10:15 19.10', '2026-10-19T06:00:00'),
                        (1, 2, 1, true,  'TAP', 'TP671', 'AMS 07:15 12.10 ✈ LIS 09:30 12.10', '2026-10-12T07:15:00');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 18);
        let trip = store.find_trip(1, "Lisbon").unwrap().unwrap();
        assert_eq!(trip.items.len(), 2);
        assert_eq!((trip.items[0].position, trip.items[0].date.as_str(), trip.items[0].kind.as_str()), (1, "2026-10-12", "flight"));
        assert_eq!(trip.items[0].origin.as_deref(), Some("AMS"));
        assert_eq!(trip.items[0].title, "AMS → LIS");
        assert_eq!(trip.items[0].candidates.len(), 1);
        assert!(trip.items[0].candidates[0].chosen);
        assert_eq!(trip.items[0].starts_at.as_deref(), Some("2026-10-12T07:15:00"), "a flight's start is its chosen option's departure");
        assert_eq!((trip.items[1].position, trip.items[1].date.as_str()), (2, "2026-10-19"));
        assert_eq!(trip.items[1].candidates.len(), 2);
        assert!(trip.items[1].starts_at.is_none(), "undecided, so no start yet");
        let old: i64 = store.conn().query_row("SELECT count(*) FROM trip_segments", [], |r| r.get(0)).unwrap();
        assert_eq!(old, 2, "the old tables are left in place until a later release drops them");
    }

    fn stay(title: &str, date: &str, ends: &str) -> NewItem {
        NewItem {
            kind: "stay".into(), title: title.into(), place: Some("Lisbon".into()),
            date: date.into(), starts_at: None, ends_at: Some(ends.into()), notes: None,
            booked: false, confirmation_code: None, price: None, currency: None, arrival_id: None,
        }
    }

    #[test]
    fn items_of_every_kind_sort_by_date_then_time_then_kind() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        store.add_item(trip.id, stay("Hotel Alfama", "2026-10-12", "2026-10-15")).unwrap();
        // A kind the page has no card for is refused, not stored and lost.
        let mut cruise = stay("Boat", "2026-10-14", "2026-10-14");
        cruise.kind = "cruise".into();
        let err = store.add_item(trip.id, cruise).unwrap_err();
        assert!(err.to_string().contains("kind must be stay, activity or transport"), "got: {err}");
        store.add_item(trip.id, NewItem {
            kind: "activity".into(), title: "Azulejo museum".into(), place: Some("Lisbon".into()),
            date: "2026-10-13".into(), starts_at: Some("2026-10-13T10:00:00".into()), ends_at: None,
            notes: None, booked: true, confirmation_code: Some("GYG-1".into()), price: Some(12.0),
            currency: Some("EUR".into()), arrival_id: None,
        }).unwrap();
        let trip = store.add_flight(trip.id, "LIS", "AMS", "2026-10-19").unwrap();
        let order: Vec<(i64, &str, &str)> = trip.items.iter().map(|i| (i.position, i.kind.as_str(), i.title.as_str())).collect();
        // Same date: a flight (no time yet) sorts before a stay by kind rank.
        assert_eq!(order, vec![
            (1, "flight", "AMS → LIS"), (2, "stay", "Hotel Alfama"),
            (3, "activity", "Azulejo museum"), (4, "flight", "LIS → AMS"),
        ]);
        assert!(trip.items[2].booked);
        assert_eq!(trip.items[2].confirmation_code.as_deref(), Some("GYG-1"));
    }

    #[test]
    fn a_time_puts_an_item_before_an_untimed_one_on_the_same_day_and_a_chosen_flight_gets_a_time() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        let trip = store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        // Flight first by kind rank while it has no time.
        assert_eq!(trip.items[0].kind, "flight");
        let flight_position = trip.items[0].position;
        let trip = store.add_candidate(trip.id, flight_position, expected("AMS", "LIS", Some("2026-10-12")),
            candidate("TAP", "TP671", "2026-10-12T22:00:00"), true).unwrap();
        // The chosen departure is 22:00; the stay has no time; a time still
        // sorts before no time, so the flight stays first.
        assert_eq!(trip.items[0].kind, "flight");
        assert_eq!(trip.items[0].starts_at.as_deref(), Some("2026-10-12T22:00:00"));
    }

    #[test]
    fn dropping_and_re_dating_renumber_by_date() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        let trip = store.add_flight(trip.id, "LIS", "AMS", "2026-10-19").unwrap();
        let (trip, _, _) = store.update_flight(trip.id, 1, None, None, Some("2026-10-20")).unwrap();
        assert_eq!(trip.items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), vec!["Hotel", "LIS → AMS", "AMS → LIS"]);
        let trip = store.drop_item(trip.id, 2).unwrap();
        assert_eq!(trip.items.iter().map(|i| (i.position, i.title.as_str())).collect::<Vec<_>>(), vec![(1, "Hotel"), (2, "AMS → LIS")]);
    }

    #[test]
    fn options_go_on_flights_only() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        let trip = store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        let err = store.add_candidate(trip.id, 1, expected("AMS", "LIS", None), candidate("TAP", "TP1", "2026-10-12T07:00:00"), false).unwrap_err();
        assert!(err.to_string().contains("is a stay"), "got: {err}");
        // The same refusal from every door: a stay has no options to choose
        // or drop, and "no option 1" would send the caller looking for one.
        let err = store.choose_candidate(trip.id, 1, 1).unwrap_err();
        assert!(err.to_string().contains("is a stay"), "choose: {err}");
        let err = store.drop_candidate(trip.id, 1, 1).unwrap_err();
        assert!(err.to_string().contains("is a stay"), "drop: {err}");
        let err = store.choose_candidate_for_account(account, "Lisbon", 1, 1).unwrap_err();
        assert!(err.to_string().contains("is a stay"), "choose by name: {err}");
    }

    #[test]
    fn a_time_beats_kind_rank_and_two_times_sort_by_the_clock() {
        // The rule, pinned: on one day an item with a time sorts before an
        // item without one whatever their kinds, and two timed items sort
        // by the clock.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        let mut hotel = stay("Hotel", "2026-10-12", "2026-10-15");
        hotel.starts_at = Some("2026-10-12T15:00:00".into());
        store.add_item(trip.id, hotel).unwrap();
        let trip = store.add_item(trip.id, NewItem {
            kind: "activity".into(), title: "Tram 28".into(), place: Some("Lisbon".into()),
            date: "2026-10-12".into(), starts_at: Some("2026-10-12T09:00:00".into()), ends_at: None,
            notes: None, booked: false, confirmation_code: None, price: None, currency: None,
            arrival_id: None,
        }).unwrap();
        let order: Vec<(i64, &str)> = trip.items.iter().map(|i| (i.position, i.title.as_str())).collect();
        assert_eq!(order, vec![(1, "Tram 28"), (2, "Hotel"), (3, "AMS → LIS")],
            "09:00 before 15:00, and both before the flight that has no time yet");
    }

    #[test]
    fn a_note_belongs_to_the_item_and_not_to_where_it_sits() {
        // The whole point of a note: the traveller pastes a map link onto
        // the lunch, and it is still on the lunch after something earlier
        // in the week is added and every position is recomputed.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        let mut lunch = stay("Lunch with Stanley", "2026-10-14", "2026-10-14");
        lunch.kind = "activity".into();
        store.add_item(trip.id, lunch).unwrap();
        let (trip, changed) = store
            .note_item(trip.id, 1, Some("https://maps.example/lunch"))
            .unwrap();
        assert!(changed);
        assert_eq!(trip.items[0].notes.as_deref(), Some("https://maps.example/lunch"));

        // An earlier item renumbers everything; the note rides on the row.
        let trip = store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        assert_eq!(trip.items[0].title, "Hotel");
        assert_eq!(trip.items[0].notes, None);
        assert_eq!(trip.items[1].title, "Lunch with Stanley");
        assert_eq!(trip.items[1].notes.as_deref(), Some("https://maps.example/lunch"));

        // Writing what is already there is not a failure and says so.
        let (_, changed) = store
            .note_item(trip.id, 2, Some("https://maps.example/lunch"))
            .unwrap();
        assert!(!changed, "the same note twice changed nothing");
        let err = store.note_item(trip.id, 9, Some("x")).unwrap_err();
        assert!(err.to_string().contains("no segment 9"), "got: {err}");
    }

    #[test]
    fn a_cleared_note_and_a_note_never_written_are_the_same_state() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        let (trip, changed) = store.note_item(trip.id, 1, None).unwrap();
        assert_eq!(trip.items[0].notes, None);
        assert!(!changed, "there was nothing to clear");

        let (trip, _) = store.note_item(trip.id, 1, Some("ask for the terrace")).unwrap();
        assert_eq!(trip.items[0].notes.as_deref(), Some("ask for the terrace"));
        let (trip, changed) = store.note_item(trip.id, 1, None).unwrap();
        assert!(changed);
        assert_eq!(trip.items[0].notes, None, "cleared is empty, not an empty string");
        // Whitespace is not a note either, whichever door it arrives at.
        let (trip, _) = store.note_item(trip.id, 1, Some("ask for the terrace")).unwrap();
        assert_eq!(trip.items[0].notes.as_deref(), Some("ask for the terrace"));
        let (trip, changed) = store.note_item(trip.id, 1, Some("   ")).unwrap();
        assert!(changed);
        assert_eq!(trip.items[0].notes, None);
    }

    #[test]
    fn a_note_does_not_re_price_the_trip_it_is_written_on() {
        // `touch` puts a trip back to `planning` because an edit to what
        // would be priced invalidates the prices it was finalised at. A
        // note prices nothing, so a pasted map link must not quietly undo
        // a finalisation.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        store.set_trip_status(trip.id, "finalised").unwrap();
        let (trip, _) = store.note_item(trip.id, 1, Some("courtyard room")).unwrap();
        assert_eq!(trip.status, "finalised");
    }

    #[test]
    fn removing_an_item_checks_what_the_caller_saw() {
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Lisbon", None, None, None).unwrap();
        store.add_item(trip.id, stay("Hotel", "2026-10-12", "2026-10-15")).unwrap();
        let wrong = ExpectedItem { origin: None, destination: None, title: Some("Hostel"), date: Some("2026-10-12") };
        assert!(!store.remove_item_checked(trip.id, 1, wrong).unwrap());
        let right = ExpectedItem { origin: None, destination: None, title: Some("Hotel"), date: Some("2026-10-12") };
        assert!(store.remove_item_checked(trip.id, 1, right).unwrap());
    }

    #[test]
    fn a_note_is_written_only_onto_the_item_the_caller_still_sees() {
        // The same guard removing an item has, and it is worth more here
        // rather than less: a map link written onto the wrong item is
        // quieter than the wrong item disappearing, so nothing tells the
        // traveller to look.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Hong Kong", None, None, None).unwrap();
        store.add_item(trip.id, stay("Lunch with Stanley", "2026-09-24", "2026-09-24")).unwrap();
        let seen = |title| ExpectedItem { origin: None, destination: None, title: Some(title), date: Some("2026-09-24") };
        let link = "https://www.google.com/maps/search/?api=1&query=Queen%27s+Cafe";

        assert!(!store.note_item_checked(trip.id, 1, seen("Dinner with Stanley"), Some(link)).unwrap());
        assert_eq!(store.trip_by_id(account, trip.id).unwrap().unwrap().items[0].notes, None, "a note went onto an item nobody asked about");

        assert!(store.note_item_checked(trip.id, 1, seen("Lunch with Stanley"), Some(link)).unwrap());
        assert_eq!(store.trip_by_id(account, trip.id).unwrap().unwrap().items[0].notes.as_deref(), Some(link));

        // Clearing and never having had one end in the same place.
        assert!(store.note_item_checked(trip.id, 1, seen("Lunch with Stanley"), None).unwrap());
        assert_eq!(store.trip_by_id(account, trip.id).unwrap().unwrap().items[0].notes, None);
    }

    #[test]
    fn an_item_can_be_changed_where_it_stands_and_moves_if_its_date_does() {
        // Until this, the only way to fix a wrong time was to remove the
        // item and add it again — which loses its files, its link back to
        // the confirmation it came from, and renumbers everything after
        // it. From chat the model's only honest offer was a duplicate.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(account, "Hong Kong", None, None, None).unwrap();
        store.add_item(trip.id, stay("Lunch", "2026-09-24", "2026-09-24")).unwrap();
        store.add_item(trip.id, stay("Hotel", "2026-09-22", "2026-09-29")).unwrap();
        let unchanged = ItemEdit::default();
        assert!(!store.update_item(trip.id, 1, unchanged).unwrap().1, "asking for what is already true is not a change");

        // The hotel is position 1 — it starts first — and the lunch 2.
        let by_title = |trip: &Trip| trip.items.iter().map(|i| i.title.clone()).collect::<Vec<_>>();
        assert_eq!(by_title(&store.trip_by_id(account, trip.id).unwrap().unwrap()), ["Hotel", "Lunch"]);

        let (trip_after, changed) = store
            .update_item(
                trip.id,
                2,
                ItemEdit { title: Some("Lunch with Stanley"), place: Some("Queen's Cafe"), time: Some("14:30"), ..Default::default() },
            )
            .unwrap();
        assert!(changed);
        let lunch = trip_after.items.iter().find(|i| i.position == 2).unwrap();
        assert_eq!(lunch.title, "Lunch with Stanley");
        assert_eq!(lunch.place.as_deref(), Some("Queen's Cafe"));
        assert_eq!(lunch.starts_at.as_deref(), Some("2026-09-24T14:30:00"), "the clock hangs off the item's own day");

        // A date that moves the item past its neighbour renumbers the
        // trip, the way every other write does.
        let (moved, _) = store.update_item(trip.id, 2, ItemEdit { date: Some("2026-09-20"), ..Default::default() }).unwrap();
        assert_eq!(by_title(&moved), ["Lunch with Stanley", "Hotel"]);
        let lunch = moved.items.iter().find(|i| i.title == "Lunch with Stanley").unwrap();
        assert_eq!(lunch.date, "2026-09-20");
        assert_eq!(lunch.starts_at.as_deref(), Some("2026-09-20T14:30:00"), "the time followed its day rather than being left on the old one");

        // A blank clears what it names; the title is the one field that
        // cannot be cleared, since the item is drawn by it.
        let (cleared, _) = store.update_item(trip.id, 1, ItemEdit { place: Some(""), time: Some(""), ..Default::default() }).unwrap();
        let lunch = cleared.items.iter().find(|i| i.title == "Lunch with Stanley").unwrap();
        assert_eq!(lunch.place, None);
        assert_eq!(lunch.starts_at, None);

        // Held is the traveller's to say. A lunch arranged over WhatsApp
        // is as held as a hotel that sent a confirmation, and before this
        // the only way to mark one was to forward an email about it.
        let (marked, changed) = store
            .update_item(trip.id, 1, ItemEdit { booked: Some(true), confirmation_code: Some("WA-STANLEY"), ..Default::default() })
            .unwrap();
        assert!(changed);
        let lunch = marked.items.iter().find(|i| i.title == "Lunch with Stanley").unwrap();
        assert!(lunch.booked);
        assert_eq!(lunch.confirmation_code.as_deref(), Some("WA-STANLEY"));
        // And unsaid again, because plans fall through.
        let (off, _) = store
            .update_item(trip.id, 1, ItemEdit { booked: Some(false), confirmation_code: Some(""), ..Default::default() })
            .unwrap();
        let lunch = off.items.iter().find(|i| i.title == "Lunch with Stanley").unwrap();
        assert!(!lunch.booked);
        assert_eq!(lunch.confirmation_code, None);

        // A flight is not this tool's business: its route and date belong
        // to `update_flight`, which drops the options that were quoted for
        // the old one.
        store.add_flight(trip.id, "AMS", "HKG", "2026-09-21").unwrap();
        let leg = store.trip_by_id(account, trip.id).unwrap().unwrap();
        let position = leg.items.iter().find(|i| i.kind == "flight").unwrap().position;
        assert!(store.update_item(trip.id, position, ItemEdit { title: Some("Anything"), ..Default::default() }).is_err());
    }

    // ---- invite rounds, membership, waitlist ----

    #[test]
    fn a_round_admits_its_capacity_and_not_one_more() {
        let (s, _d) = test_store();
        assert!(s.create_round("autumn", 3).unwrap());
        // Through the resolver, so each member has a telegram identity —
        // which is what `active_members` reports back to the gate.
        for tg in 1..=3 {
            let account = s.account_for_telegram(tg).unwrap();
            assert_eq!(s.claim_seat(account, "autumn").unwrap(), Claim::Admitted);
        }
        let fourth = s.account_for_telegram(4).unwrap();
        assert_eq!(s.claim_seat(fourth, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.active_members().unwrap(), vec![1, 2, 3]);

        let rounds = s.rounds().unwrap();
        assert_eq!(rounds.len(), 1);
        assert_eq!((rounds[0].used, rounds[0].capacity, rounds[0].open), (3, 3, true));
    }

    #[test]
    fn claiming_a_seat_records_no_address_of_its_own() {
        let (s, _d) = test_store();
        s.create_round("autumn", 5).unwrap();
        let account = s.account_for_identity("email", "web@example.com").unwrap();

        assert_eq!(s.claim_seat(account, "autumn").unwrap(), Claim::Admitted);

        // The point of the split: a web visitor has no chat, and claiming a
        // seat must not invent one. If this fails, the delivery write has
        // crept back into claim_seat and the web path writes a lie.
        let conn = s.conn.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM deliveries WHERE account_id = ?",
                params![account],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "claim_seat wrote a delivery row");
    }

    #[test]
    fn concurrent_claims_never_oversell() {
        // The whole reason claiming is one method: check-and-insert happens
        // under a single lock, so a rush on a link cannot overfill a round.
        let (s, _d) = test_store();
        assert!(s.create_round("rush", 5).unwrap());

        let admitted: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (1..=40)
                .map(|tg| {
                    let s = s.clone();
                    scope.spawn(move || {
                        let account = s.account_for_telegram(tg).unwrap();
                        s.claim_seat(account, "rush").unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|claim| *claim == Claim::Admitted)
                .count()
        });

        assert_eq!(admitted, 5, "a round of 5 admits exactly 5");
        assert_eq!(s.active_members().unwrap().len(), 5);
        assert_eq!(s.rounds().unwrap()[0].used, 5);
    }

    #[test]
    fn a_second_claim_by_the_same_person_spends_no_seat() {
        let (s, _d) = test_store();
        s.create_round("autumn", 2).unwrap();
        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::Admitted);
        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::AlreadyIn);
        // A member opening a *later* round's link is also already in.
        s.create_round("winter", 5).unwrap();
        assert_eq!(s.claim_seat(1, "winter").unwrap(), Claim::AlreadyIn);

        assert_eq!(s.rounds().unwrap()[0].used, 1, "one person, one seat");
        assert_eq!(s.claim_seat(2, "autumn").unwrap(), Claim::Admitted);
    }

    #[test]
    fn a_revoked_member_cannot_rejoin_through_any_link() {
        // Without this, revoking is theatre.
        let (s, _d) = test_store();
        s.create_round("autumn", 10).unwrap();
        s.claim_seat(1, "autumn").unwrap();
        assert!(s.revoke(1).unwrap());
        assert!(s.active_members().unwrap().is_empty());

        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::Revoked);
        s.create_round("winter", 10).unwrap();
        assert_eq!(s.claim_seat(1, "winter").unwrap(), Claim::Revoked);

        // And being refused does not put them on the waitlist to be
        // announced back in later.
        assert_eq!(s.waiting_count().unwrap(), 0);
    }

    #[test]
    fn revoking_returns_no_seat_and_restoring_takes_none() {
        let (s, _d) = test_store();
        s.create_round("autumn", 2).unwrap();
        let (a1, a2) = (s.account_for_telegram(1).unwrap(), s.account_for_telegram(2).unwrap());
        s.claim_seat(a1, "autumn").unwrap();
        s.claim_seat(a2, "autumn").unwrap();

        assert!(s.revoke(1).unwrap());
        assert_eq!(s.rounds().unwrap()[0].used, 2, "a round of 2 admitted 2 people, once");
        assert_eq!(s.claim_seat(3, "autumn").unwrap(), Claim::NoRoom);

        assert!(s.restore(1).unwrap());
        assert_eq!(s.rounds().unwrap()[0].used, 2, "restoring consumes nothing either");
        assert_eq!(s.active_members().unwrap(), vec![1, 2]);

        // Neither is an error to repeat, and neither invents a member.
        assert!(!s.restore(1).unwrap(), "already restored");
        assert!(!s.revoke(999).unwrap(), "never a member");
        assert!(!s.restore(999).unwrap());
    }

    #[test]
    fn an_unknown_code_and_a_closed_round_are_refused_alike() {
        let (s, _d) = test_store();
        s.create_round("autumn", 10).unwrap();
        assert!(s.set_round_open("autumn", false).unwrap());

        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.claim_seat(2, "no-such-round").unwrap(), Claim::NoRoom);
        assert!(s.active_members().unwrap().is_empty());
        // Both are people who tried to reach us, so both are queued.
        assert_eq!(s.waiting_count().unwrap(), 2);

        assert!(!s.set_round_open("no-such-round", true).unwrap());
    }

    #[test]
    fn a_reopened_round_admits_again() {
        let (s, _d) = test_store();
        s.create_round("autumn", 10).unwrap();
        s.set_round_open("autumn", false).unwrap();
        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::NoRoom);

        assert!(s.set_round_open("autumn", true).unwrap());
        assert_eq!(s.claim_seat(1, "autumn").unwrap(), Claim::Admitted);
    }

    #[test]
    fn a_round_name_cannot_be_reused() {
        // Otherwise two rounds pool their seats under one capacity.
        let (s, _d) = test_store();
        assert!(s.create_round("autumn", 10).unwrap());
        assert!(!s.create_round("autumn", 500).unwrap());
        assert_eq!(s.rounds().unwrap()[0].capacity, 10, "the first round is untouched");
    }

    #[test]
    fn being_turned_away_queues_you_and_getting_in_clears_it() {
        let (s, _d) = test_store();
        s.create_round("autumn", 1).unwrap();
        s.claim_seat(1, "autumn").unwrap();

        assert_eq!(s.claim_seat(2, "autumn").unwrap(), Claim::NoRoom);
        // claim_seat no longer records where to reach them — that is the
        // channel's job now — so give the waitlist the address it needs.
        s.note_delivery(2, "telegram", "8002").unwrap();
        assert_eq!(s.waitlist_to_invite().unwrap(), vec![(2, 8002)]);

        // A second attempt keeps their place rather than costing it.
        assert_eq!(s.claim_seat(2, "autumn").unwrap(), Claim::NoRoom);
        assert_eq!(s.waiting_count().unwrap(), 1);

        // Getting in through a later round takes them off the queue, so a
        // future announce does not chase somebody already inside.
        s.create_round("winter", 5).unwrap();
        assert_eq!(s.claim_seat(2, "winter").unwrap(), Claim::Admitted);
        assert_eq!(s.waiting_count().unwrap(), 0);
        assert!(s.waitlist_to_invite().unwrap().is_empty());
    }

    #[test]
    fn an_announce_stamps_only_the_rows_it_reached() {
        let (s, _d) = test_store();
        s.create_round("autumn", 0).unwrap();
        for user in [1, 2, 3] {
            s.claim_seat(user, "autumn").unwrap();
            // claim_seat no longer records the address — the channel does.
            s.note_delivery(user, "telegram", &(9000 + user).to_string()).unwrap();
        }
        assert_eq!(
            s.waitlist_to_invite().unwrap(),
            vec![(1, 9001), (2, 9002), (3, 9003)],
            "oldest first: if the next round is smaller than the queue, the \
             people who waited longest hear first"
        );

        // One reached, one unreachable, one that simply failed this time.
        s.mark_invited(1).unwrap();
        s.forget_waitlist(2).unwrap();

        assert_eq!(
            s.waitlist_to_invite().unwrap(),
            vec![(3, 9003)],
            "a re-run reaches only the people the first run missed"
        );
        assert_eq!(s.waiting_count().unwrap(), 1);
    }

    #[test]
    fn someone_turned_away_after_being_announced_to_is_queued_again() {
        // They pressed START on the new link and it was full too. They are
        // still waiting, so the next announce has to reach them.
        let (s, _d) = test_store();
        s.create_round("autumn", 0).unwrap();
        s.claim_seat(1, "autumn").unwrap();
        s.note_delivery(1, "telegram", "9001").unwrap();
        s.mark_invited(1).unwrap();
        assert!(s.waitlist_to_invite().unwrap().is_empty());

        s.create_round("winter", 0).unwrap();
        assert_eq!(s.claim_seat(1, "winter").unwrap(), Claim::NoRoom);
        assert_eq!(s.waitlist_to_invite().unwrap(), vec![(1, 9001)]);
    }

    #[test]
    fn the_daily_cap_counts_messages_and_only_todays() {
        let (s, _d) = test_store();
        let today = chrono::Utc::now().date_naive();
        let yesterday = today - chrono::Duration::days(1);

        s.log_request(1, "text").unwrap();
        s.log_request(1, "photo").unwrap();
        // Not requests: a reaction is not one, and a flight search is a
        // sub-event of a message that was already counted.
        s.log_request(1, "reaction").unwrap();
        s.log_request(1, Store::FLIGHT_SEARCH).unwrap();
        // Another user's traffic is not this user's.
        s.log_request(2, "text").unwrap();
        // Yesterday is spent.
        s.log_request_at(1, "text", &format!("{yesterday} 23:59:59")).unwrap();

        assert_eq!(s.requests_today(1).unwrap(), 2);
        assert_eq!(s.requests_today(2).unwrap(), 1);
        assert_eq!(s.requests_today(999).unwrap(), 0);
    }
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

    #[test]
    fn a_pending_migration_backs_the_database_up_before_changing_it() {
        // The failure with no second chance. A migration cannot be undone, so
        // if this copy is missing — or is taken after the fact — a bad schema
        // step is unrecoverable. We took this by hand four times before
        // automating it, and remembering was the only protection.
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("legacy.duckdb");
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(LEGACY_SCHEMA).unwrap();
            conn.execute_batch(
                "INSERT INTO purchases (user_id, item, store) VALUES (7, 'detergent', 'bol.com')",
            )
            .unwrap();
        }

        let store = Store::open(&db).unwrap();
        assert!(store.schema_version().unwrap() >= 5, "the migration ran");

        let backups = crate::backup::dir_for(&db);
        let taken: Vec<_> = std::fs::read_dir(&backups)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(taken.len(), 1, "exactly one backup, taken before the steps");
        assert!(taken[0].file_name().unwrap().to_str().unwrap().contains("migration-v"));

        // The proof it was taken BEFORE rather than after: the copy still has
        // the column the migration replaces.
        let before = Connection::open(&taken[0]).unwrap();
        let legacy: i64 = before
            .query_row(
                "SELECT count(*) FROM information_schema.columns
                 WHERE table_name = 'purchases' AND column_name = 'user_id'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 1, "the backup should predate the column being replaced");
    }

    #[test]
    fn a_database_with_nothing_to_migrate_is_not_backed_up() {
        // Otherwise every restart writes one and the retention window stops
        // meaning two weeks.
        let dir = TempDir::new().unwrap();
        let db = dir.path().join("fresh.duckdb");
        drop(Store::open(&db).unwrap());
        drop(Store::open(&db).unwrap());

        let backups = crate::backup::dir_for(&db);
        let n = std::fs::read_dir(&backups).map(|d| d.count()).unwrap_or(0);
        assert_eq!(n, 0, "a fresh database has no pending steps and needs no copy");
    }

    #[test]
    fn enqueueing_the_same_turn_twice_leaves_one_row() {
        // Backfill and the live path both enqueue, and toggling off and on
        // backfills again. All three lean on this being a no-op.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        assert!(s.enqueue_mirror(a, "telegram", "11", "hello", "key-1", false).unwrap());
        assert!(!s.enqueue_mirror(a, "telegram", "11", "hello", "key-1", false).unwrap());
        assert_eq!(s.pending_mirror("telegram", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_turn_the_channel_already_delivered_is_never_pending() {
        // This is the echo guarantee. The browser and a 1:1 Telegram chat
        // share conversation scope "direct", so backfilling the thread would
        // send Telegram its own messages back. The channel that handled a
        // turn records it as delivered, and the backfill then skips it
        // because "already in the ledger" and "already sent" are one fact.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        assert!(s.enqueue_mirror(a, "telegram", "11", "from telegram", "key-2", true).unwrap());
        assert!(s.pending_mirror("telegram", 10).unwrap().is_empty());
        // And the backfill's later attempt at the same turn changes nothing.
        assert!(!s.enqueue_mirror(a, "telegram", "11", "from telegram", "key-2", false).unwrap());
        assert!(s.pending_mirror("telegram", 10).unwrap().is_empty());
    }

    #[test]
    fn two_channels_do_not_share_one_turns_key() {
        // The same turn goes to two places, and each has to queue it. With
        // the channel left out of the uniqueness key the second channel's
        // enqueue returns `false` because the first already holds the key,
        // and that channel silently never delivers.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        assert!(s.enqueue_mirror(a, "telegram", "11", "hello", "k1", false).unwrap());
        assert!(s.enqueue_mirror(a, "signal", "11", "hello", "k1", false).unwrap());
        assert_eq!(s.pending_mirror("telegram", 10).unwrap().len(), 1);
        assert_eq!(s.pending_mirror("signal", 10).unwrap().len(), 1);
    }

    #[test]
    fn pending_rows_come_back_oldest_first_and_marking_them_clears_them() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        s.enqueue_mirror(a, "telegram", "11", "first", "k1", false).unwrap();
        s.enqueue_mirror(a, "telegram", "11", "second", "k2", false).unwrap();
        let due = s.pending_mirror("telegram", 10).unwrap();
        assert_eq!(due.iter().map(|r| r.body.as_str()).collect::<Vec<_>>(), ["first", "second"]);
        s.mark_mirror_sent(due[0].id).unwrap();
        let due = s.pending_mirror("telegram", 10).unwrap();
        assert_eq!(due.iter().map(|r| r.body.as_str()).collect::<Vec<_>>(), ["second"]);
    }

    #[test]
    fn a_row_is_abandoned_after_five_attempts() {
        // The reminder path can retry forever safely because dates bound it.
        // This one has no such bound: block the bot and an uncapped retry
        // loops until a human notices.
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        s.enqueue_mirror(a, "telegram", "11", "doomed", "k1", false).unwrap();
        let id = s.pending_mirror("telegram", 10).unwrap()[0].id;
        for attempt in 1..=4 {
            // The count comes back so the drain can tell a retry from a row
            // it has just given up on — the log line said "it stays queued"
            // on the fifth failure, which was the one time it did not.
            assert_eq!(s.mark_mirror_failed(id).unwrap(), attempt);
            assert_eq!(s.pending_mirror("telegram", 10).unwrap().len(), 1, "gave up too early");
        }
        assert_eq!(s.mark_mirror_failed(id).unwrap(), MIRROR_ATTEMPTS);
        assert!(s.pending_mirror("telegram", 10).unwrap().is_empty(), "retried forever");
    }

    #[test]
    fn the_mirror_setting_is_the_presence_of_a_row() {
        let (s, _d) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        assert!(!s.mirror_enabled(a).unwrap());
        s.set_mirror(a, true).unwrap();
        assert!(s.mirror_enabled(a).unwrap());
        // Enabling twice is not an error -- the page can post it twice.
        s.set_mirror(a, true).unwrap();
        assert!(s.mirror_enabled(a).unwrap());
        s.set_mirror(a, false).unwrap();
        assert!(!s.mirror_enabled(a).unwrap());
    }

    /// The connection sits behind one mutex shared by every store call. A
    /// panic while holding it poisons the lock, and a store that then
    /// unwraps every `lock()` panics on every call for the life of the
    /// process — with `/healthz` still saying ok, because liveness
    /// deliberately does not look at the database.
    #[test]
    fn a_panic_under_the_lock_does_not_take_the_store_down() {
        let (s, _dir) = test_store();
        let conn = s.conn.clone();
        let _ = std::thread::spawn(move || {
            let _held = conn.lock().unwrap();
            panic!("something went wrong while the lock was held");
        })
        .join();

        s.log_request(1, "text").expect("the store must recover from a poisoned lock");
        assert_eq!(s.requests_today(1).unwrap(), 1);
    }

    /// A panic mid-transaction leaves that transaction open on the
    /// connection, and DuckDB refuses to `BEGIN` inside one. Recovery has
    /// to roll it back, or the first transactional write after the panic
    /// fails for a reason nobody will connect to it.
    #[test]
    fn a_panic_inside_a_transaction_leaves_the_next_one_able_to_begin() {
        let (s, _dir) = test_store();
        let conversation = s.start_conversation(1, "direct").unwrap();
        let conn = s.conn.clone();
        let _ = std::thread::spawn(move || {
            let held = conn.lock().unwrap();
            held.execute_batch("BEGIN").unwrap();
            panic!("something went wrong inside a transaction");
        })
        .join();

        s.replace_messages(conversation, &["{}".to_string()])
            .expect("a transactional write must work after a poisoned lock");
        assert_eq!(s.conversation_messages(conversation, 10).unwrap().len(), 1);
    }

    fn has_column(conn: &Connection, table: &str, column: &str) -> bool {
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM information_schema.columns
                 WHERE table_name = ? AND column_name = ?",
                params![table, column],
                |r| r.get(0),
            )
            .unwrap();
        n > 0
    }

    #[test]
    fn a_fresh_database_has_a_title_and_a_pin_on_every_conversation() {
        let (s, _dir) = test_store();
        let conn = s.conn();
        assert!(has_column(&conn, "conversations", "title"));
        assert!(has_column(&conn, "conversations", "pinned"));
    }

    /// `conversations` exactly as it stood at schema version 6, before the
    /// title and the pin. Frozen, like `LEGACY_SCHEMA`: its value is being
    /// an honest picture of the table the step will actually meet.
    const PRE_THREADS_CONVERSATIONS: &str = r#"
DROP TABLE conversations;
CREATE TABLE conversations (
    id            BIGINT PRIMARY KEY DEFAULT nextval('conversations_id_seq'),
    account_id    BIGINT NOT NULL,
    scope         TEXT NOT NULL,
    pending_draft TEXT,
    started_at    TIMESTAMP NOT NULL DEFAULT current_timestamp,
    updated_at    TIMESTAMP NOT NULL DEFAULT current_timestamp
);
"#;

    /// A database in the production shape: everything `MIGRATIONS` builds,
    /// with `conversations` rolled back to its version-6 form, the version
    /// recorded as 6, and a thread already in it. The row is what matters —
    /// the step is trivially safe on an empty table and refuses to run on a
    /// full one, which is the whole bug.
    fn version_six_db_with_a_thread() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v6.duckdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS).unwrap();
        conn.execute_batch(PRE_THREADS_CONVERSATIONS).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version BIGINT NOT NULL);
             INSERT INTO schema_version VALUES (6);
             INSERT INTO accounts (id) VALUES (nextval('accounts_id_seq'));
             INSERT INTO conversations (id, account_id, scope)
                 VALUES (nextval('conversations_id_seq'), 1, 'direct');",
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn a_database_at_version_six_with_threads_in_it_grows_the_columns_and_keeps_its_rows() {
        let (_dir, db) = version_six_db_with_a_thread();
        let s = Store::open(&db).unwrap();
        assert_eq!(s.schema_version().unwrap(), 18, "the migration did not reach the latest step");
        let conn = s.conn();
        assert!(has_column(&conn, "conversations", "title"));
        assert!(has_column(&conn, "conversations", "pinned"));
        let old: bool = conn
            .query_row("SELECT pinned FROM conversations WHERE id = 1", [], |r| r.get(0))
            .expect("the thread that was already there must survive");
        assert!(!old, "a thread that predates the pin is unpinned");
        let null_pins: i64 = conn
            .query_row("SELECT count(*) FROM conversations WHERE pinned IS NULL", [], |r| r.get(0))
            .unwrap();
        assert_eq!(null_pins, 0, "the backfill must leave no null pins behind");
        drop(conn);
        let id = s.start_conversation(1, "direct").unwrap();
        let conn = s.conn();
        let fresh: bool = conn
            .query_row("SELECT pinned FROM conversations WHERE id = ?", params![id], |r| r.get(0))
            .unwrap();
        assert!(!fresh, "a thread started after the migration is unpinned too");
        assert!(
            conn.execute_batch(
                "INSERT INTO conversations (id, account_id, scope, pinned)
                 VALUES (nextval('conversations_id_seq'), 1, 'direct', NULL)"
            )
            .is_err(),
            "the pin must be NOT NULL on a migrated file"
        );
    }

    #[test]
    fn a_database_interrupted_between_the_two_thread_steps_finishes_on_the_next_boot() {
        // The one interleaving a partial deploy can produce: step 7 landed,
        // the process died, step 8 never ran. `STEP_8_PINNED_NOT_NULL`'s
        // comment promises the next boot finishes the job.
        let (_dir, db) = version_six_db_with_a_thread();
        {
            let conn = Connection::open(&db).unwrap();
            conn.execute_batch(STEP_7_THREADS).unwrap();
            conn.execute_batch("UPDATE schema_version SET version = 7").unwrap();
        }
        let s = Store::open(&db).unwrap();
        assert_eq!(s.schema_version().unwrap(), 18);
        let conn = s.conn();
        assert!(
            conn.execute_batch(
                "INSERT INTO conversations (id, account_id, scope, pinned)
                 VALUES (nextval('conversations_id_seq'), 1, 'direct', NULL)"
            )
            .is_err(),
            "step 8 did not run on the second boot"
        );
    }

    /// Column name, order, type, nullability and default — the whole of
    /// what a query can see. `MIGRATIONS` and the steps are two
    /// descriptions of one table, and nothing else keeps them in step; this
    /// is how a test proves they still agree.
    fn shape(conn: &Connection, table: &str) -> Vec<(String, i64, String, String, Option<String>)> {
        let mut stmt = conn
            .prepare(
                "SELECT column_name, ordinal_position, data_type, is_nullable, column_default
                 FROM information_schema.columns
                 WHERE table_name = ? ORDER BY ordinal_position",
            )
            .unwrap();
        stmt.query_map(params![table], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn a_migrated_conversations_table_has_exactly_the_shape_a_fresh_one_has() {
        let (fresh, _d1) = test_store();
        let (_d2, db) = version_six_db_with_a_thread();
        let migrated = Store::open(&db).unwrap();
        assert_eq!(shape(&fresh.conn(), "conversations"), shape(&migrated.conn(), "conversations"));
    }

    #[test]
    fn a_migrated_trips_table_has_exactly_the_shape_a_fresh_one_has() {
        // `STEP_4_REBUILDS` rewrites `trips` from a fixed column list that
        // predates `conversation_id`, and step 9 adds the column afterwards
        // by `ALTER TABLE`. That ordering is what makes today's rebuild
        // correct, and nothing enforces it — the next constrained change to
        // `trips` could copy the `STEP_4_REBUILDS` pattern, list the columns
        // it can see, and quietly drop a later one on every deployed
        // database while the rest of the suite stayed green. `legacy_db`
        // rather than `version_six_db_with_a_thread`: it is the fixture
        // that actually runs step 4 before step 9, which is the ordering
        // this depends on.
        let (fresh, _d1) = test_store();
        let (_d2, db) = legacy_db();
        let migrated = Store::open(&db).unwrap();
        assert_eq!(shape(&fresh.conn(), "trips"), shape(&migrated.conn(), "trips"));
    }

    #[test]
    fn a_migrated_arrivals_table_has_exactly_the_shape_a_fresh_one_has() {
        // Step 17 appends its columns; `MIGRATIONS` lists them last and in
        // the same order. Put them anywhere else in the DDL and a fresh
        // database and a migrated one disagree about ordinal positions —
        // the drift the `conversations` and `trips` tests above exist for.
        let (fresh, _d1) = test_store();
        let (_d2, path) = version_sixteen_db();
        let migrated = Store::open(&path).unwrap();
        assert_eq!(shape(&fresh.conn(), "arrivals"), shape(&migrated.conn(), "arrivals"));
    }

    #[test]
    fn threads_are_listed_pinned_first_and_then_by_last_use() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let old = s.start_conversation(a, "direct").unwrap();
        let pinned = s.start_conversation(a, "direct").unwrap();
        let newest = s.start_conversation(a, "direct").unwrap();
        s.conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(3600) WHERE id = {old};
                 UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(7200) WHERE id = {pinned};"
            ))
            .unwrap();
        assert!(s.set_thread_pinned(a, pinned, true).unwrap());
        // Not this account's thread, and not `direct`: neither may appear.
        let b = s.account_for_telegram(22).unwrap();
        s.start_conversation(b, "direct").unwrap();
        s.start_conversation(a, "telegram:-100").unwrap();

        let ids: Vec<i64> = s.threads_of(a).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![pinned, newest, old]);
    }

    #[test]
    fn a_thread_row_carries_what_the_sidebar_shows() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let id = s.start_conversation(a, "direct").unwrap();
        assert!(s.set_thread_title(a, id, "wasmiddel per kilo").unwrap());
        let rows = s.threads_of(a).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title.as_deref(), Some("wasmiddel per kilo"));
        assert!(!rows[0].pinned);
        assert!(rows[0].updated_at.ends_with('Z'), "not RFC 3339 UTC: {}", rows[0].updated_at);
        let stamped = chrono::NaiveDateTime::parse_from_str(&rows[0].updated_at, "%Y-%m-%dT%H:%M:%SZ")
            .unwrap()
            .and_utc()
            .timestamp();
        let skew = (chrono::Utc::now().timestamp() - stamped).abs();
        assert!(skew < 5, "updated_at not close to now: skew {skew}s");
    }

    #[test]
    fn opening_a_thread_makes_it_the_newest_and_only_for_its_owner() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let first = s.start_conversation(a, "direct").unwrap();
        let second = s.start_conversation(a, "direct").unwrap();
        s.conn()
            .execute(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(60) WHERE id = ?",
                params![first],
            )
            .unwrap();
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap().map(|(id, _)| id), Some(second));

        assert!(s.open_conversation(a, first).unwrap());
        assert_eq!(s.latest_conversation(a, "direct", 600).unwrap().map(|(id, _)| id), Some(first));

        let stranger = s.account_for_telegram(22).unwrap();
        assert!(!s.open_conversation(stranger, first).unwrap(), "opened someone else's thread");
        assert!(!s.open_conversation(a, 999_999).unwrap(), "opened a thread that does not exist");

        let group = s.start_conversation(a, "telegram:-100").unwrap();
        assert!(!s.open_conversation(a, group).unwrap(), "opened a group thread as a browser thread");
        assert!(!s.set_thread_pinned(a, group, true).unwrap(), "pinned a group thread, which would make it immortal");
        assert!(!s.set_thread_title(a, group, "x").unwrap(), "renamed a group thread from the browser");
        assert!(!s.delete_conversation(a, group).unwrap(), "deleted a group thread from the browser");
        assert_eq!(s.thread_title(a, group).unwrap(), None);
        // `owns_thread` is the same rule without the bump, so it draws the
        // same line: a group thread is not the browser's to name.
        assert!(!s.owns_thread(a, group).unwrap(), "a group thread counted as a browser thread");
        assert!(s.owns_thread(a, first).unwrap());
    }

    #[test]
    fn a_thread_can_only_be_renamed_pinned_or_deleted_by_its_owner() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let stranger = s.account_for_telegram(22).unwrap();
        let id = s.start_conversation(a, "direct").unwrap();
        s.replace_messages(id, &["{}".to_string()]).unwrap();

        assert!(!s.set_thread_title(stranger, id, "mine now").unwrap());
        assert!(!s.set_thread_pinned(stranger, id, true).unwrap());
        assert!(!s.delete_conversation(stranger, id).unwrap());
        assert_eq!(s.threads_of(a).unwrap().len(), 1, "a stranger changed something");
        assert_eq!(s.thread_title(a, id).unwrap(), None);

        assert!(s.delete_conversation(a, id).unwrap());
        assert!(s.threads_of(a).unwrap().is_empty());
        assert!(s.conversation_messages(id, 10).unwrap().is_empty(), "messages outlived their thread");
    }

    #[test]
    fn deleting_a_thread_deletes_the_trip_it_owns_and_nothing_else() {
        // Pressing Delete is a decision, so it takes the plan with it. The
        // trip owned by another thread is the control: a cascade that is
        // too wide is worse than none.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let doomed = store.start_conversation(account, "direct").unwrap();
        let spared = store.start_conversation(account, "direct").unwrap();

        let a = store.upsert_trip(account, "Atlantic loop", None, None, Some(doomed)).unwrap();
        store.upsert_trip(account, "Japan in spring", None, None, Some(spared)).unwrap();
        store.add_flight(a.id, "AMS", "LIS", "2026-10-12").unwrap();

        assert!(store.delete_conversation(account, doomed).unwrap());

        assert!(store.find_trip(account, "Atlantic loop").unwrap().is_none(), "its trip goes with it");
        assert!(store.find_trip(account, "Japan in spring").unwrap().is_some(), "another thread's trip stays");
        let orphans: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM trip_items WHERE trip_id = ?", params![a.id], |r| r.get(0))
            .unwrap();
        assert_eq!(orphans, 0, "a deleted trip leaves no items behind");
    }

    #[test]
    fn deleting_a_thread_leaves_an_unowned_trip_alone() {
        // `conversation_id IS NULL` is not what `= ?` matches — a trip
        // nobody owns must not vanish just because some other thread on the
        // same account got deleted.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let id = store.start_conversation(account, "direct").unwrap();
        store.upsert_trip(account, "Orphaned already", None, None, None).unwrap();

        assert!(store.delete_conversation(account, id).unwrap());

        assert!(store.find_trip(account, "Orphaned already").unwrap().is_some(), "an unowned trip was swept up");
    }

    #[test]
    fn a_title_written_only_when_missing_never_covers_a_rename() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let id = s.start_conversation(a, "direct").unwrap();
        assert!(s.set_thread_title_if_missing(id, "first message").unwrap());
        assert!(!s.set_thread_title_if_missing(id, "second message").unwrap());
        assert_eq!(s.thread_title(a, id).unwrap().as_deref(), Some("first message"));
        s.set_thread_title(a, id, "renamed").unwrap();
        assert!(!s.set_thread_title_if_missing(id, "third").unwrap());
        assert_eq!(s.thread_title(a, id).unwrap().as_deref(), Some("renamed"));
    }

    #[test]
    fn expiry_takes_an_idle_unpinned_thread_and_leaves_a_pinned_or_recent_one() {
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let idle = s.start_conversation(a, "direct").unwrap();
        let pinned = s.start_conversation(a, "direct").unwrap();
        let recent = s.start_conversation(a, "direct").unwrap();
        let group = s.start_conversation(a, "telegram:-100").unwrap();
        for id in [idle, pinned, recent, group] {
            s.replace_messages(id, &["{}".to_string()]).unwrap();
        }
        s.set_thread_pinned(a, pinned, true).unwrap();
        s.conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(49 * 3600) WHERE id IN ({idle}, {pinned}, {group});
                 UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(47 * 3600) WHERE id = {recent};"
            ))
            .unwrap();

        let gone = s.expire_conversations(48 * 3600, &[]).unwrap();

        assert_eq!(gone, 2, "the idle direct thread and the idle group thread");
        let left: Vec<i64> = s.threads_of(a).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(left, vec![pinned, recent]);
        assert!(s.conversation_messages(idle, 10).unwrap().is_empty(), "messages outlived their thread");
        assert!(s.conversation_messages(group, 10).unwrap().is_empty(), "the group's messages outlived it");
        assert_eq!(s.conversation_messages(pinned, 10).unwrap().len(), 1);
    }

    #[test]
    fn expiry_leaves_alone_a_thread_named_as_still_running() {
        // A run takes minutes and a thread can cross 48h idle in the middle
        // of one — `updated_at` moves when the answer is written, not when
        // the question is asked. Deleting it there loses the question, the
        // answer lands in a thread that no longer exists, and the reader
        // watches their conversation disappear as they wait for it.
        //
        // Two ids in `except`, not one: `expire_conversations` builds its
        // `NOT IN (...)` placeholder list with `.join(", ")`, and a single
        // id never exercises the separator — the query text would come out
        // the same with the join dropped entirely.
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let running = s.start_conversation(a, "direct").unwrap();
        let also_running = s.start_conversation(a, "direct").unwrap();
        let idle = s.start_conversation(a, "direct").unwrap();
        for id in [running, also_running, idle] {
            s.replace_messages(id, &["{}".to_string()]).unwrap();
        }
        s.conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(49 * 3600) WHERE id IN ({running}, {also_running}, {idle});"
            ))
            .unwrap();

        let gone = s.expire_conversations(48 * 3600, &[running, also_running]).unwrap();

        assert_eq!(gone, 1, "only the thread with nothing running in it");
        let left: Vec<i64> = s.threads_of(a).unwrap().iter().map(|t| t.id).collect();
        assert_eq!(left.len(), 2, "both running threads survive");
        assert!(left.contains(&running));
        assert!(left.contains(&also_running));
        // The transcript matters as much as the row: the orphan sweep in the
        // same transaction would take the messages even if the row survived.
        assert_eq!(s.conversation_messages(running, 10).unwrap().len(), 1, "the running thread lost its transcript");
        assert_eq!(
            s.conversation_messages(also_running, 10).unwrap().len(),
            1,
            "the second running thread lost its transcript"
        );
    }

    #[test]
    fn expiry_also_sweeps_messages_whose_thread_is_already_gone() {
        // A run that finishes after its thread was deleted still writes its
        // messages: an append inserts regardless. Nothing else would ever
        // collect them.
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let id = s.start_conversation(a, "direct").unwrap();
        assert!(s.delete_conversation(a, id).unwrap());
        s.replace_messages(id, &["{}".to_string(), "{}".to_string()]).unwrap();
        assert_eq!(s.conversation_messages(id, 10).unwrap().len(), 2, "the orphans must exist for this test to mean anything");

        assert_eq!(s.expire_conversations(48 * 3600, &[]).unwrap(), 0, "no conversation expired");

        assert!(s.conversation_messages(id, 10).unwrap().is_empty(), "orphaned messages were not swept");
    }

    #[test]
    fn an_expired_thread_releases_its_trip_instead_of_destroying_it() {
        // The single most important test in this feature. Threads expire on
        // a 48-hour timer; trips are built over weeks. If expiry ever
        // cascades the way `delete_conversation` does, every travel plan
        // disappears two days after its chat goes quiet — silently, with
        // nothing to undo. This test is what stands in the way.
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let stale = s.start_conversation(a, "direct").unwrap();
        let trip = s.upsert_trip(a, "Japan in spring", None, None, Some(stale)).unwrap();
        // Kept, because that is now what makes a trip a plan. When this test
        // was written every trip was one; since drafts arrived, a trip
        // nobody kept is scratch work the timer is *supposed* to clear — see
        // `expiry_deletes_the_draft_and_lets_the_kept_trip_go_free`. Drop
        // this line and the test fails for the right reason: it would be
        // asking the timer to spare scratch work, not a plan.
        s.keep_trip(a, "Japan in spring").unwrap();
        s.conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(72 * 3600) WHERE id = {stale};"
            ))
            .unwrap();

        let gone = s.expire_conversations(48 * 3600, &[]).unwrap();

        assert_eq!(gone, 1, "the stale thread expired");
        assert!(s.find_trip(a, "Japan in spring").unwrap().is_some(), "a timer must never destroy a travel plan");
        assert_eq!(s.trip_owner(trip.id).unwrap(), None, "and the dead link is released");
    }

    #[test]
    fn expiry_deletes_the_draft_and_lets_the_kept_trip_go_free() {
        // Both outcomes in one test on purpose: split into two, either can
        // be satisfied by treating every trip alike, which is precisely the
        // mistake this distinction exists to prevent. A timer must never
        // destroy a plan — and a draft nobody kept is exactly what a timer
        // should clean up.
        let (store, _dir) = test_store();
        let account = store.account_for_telegram(1).unwrap();
        let stale = store.start_conversation(account, "direct").unwrap();

        let draft = store.upsert_trip(account, "Draft loop", None, None, Some(stale)).unwrap();
        let plan = store.upsert_trip(account, "Japan in spring", None, None, Some(stale)).unwrap();
        store.keep_trip(account, "Japan in spring").unwrap();
        // The draft gets a leg with an option parked on it, because a delete
        // that takes the `trips` row and leaves its children behind is a leak
        // no read path can ever reach — `find_trip` alone would not notice.
        store.add_flight(draft.id, "AMS", "NRT", "2026-09-03").unwrap();
        store
            .add_candidate(
                draft.id,
                1,
                expected("AMS", "NRT", Some("2026-09-03")),
                candidate("KLM", "KL861", "2026-09-03T10:05:00"),
                false,
            )
            .unwrap();
        // A formatted literal interval rather than a bound one: a
        // parameterised `to_seconds(CAST(? AS INTEGER))` fails to bind on a
        // cold connection — the failure `issue_login_token` documents.
        store
            .conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(72 * 3600) WHERE id = {stale};"
            ))
            .unwrap();

        assert_eq!(store.expire_conversations(48 * 3600, &[]).unwrap(), 1);

        assert!(
            store.find_trip(account, "Draft loop").unwrap().is_none(),
            "an unkept draft goes with the thread that made it"
        );
        let kept = store.find_trip(account, "Japan in spring").unwrap();
        assert!(kept.is_some(), "a timer must never destroy a plan the traveller kept");
        assert_eq!(store.trip_owner(plan.id).unwrap(), None, "and its dead link is released");

        let conn = store.conn();
        let orphans: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM trip_items WHERE trip_id = ?)
                      + (SELECT count(*) FROM item_candidates c
                         WHERE NOT EXISTS (SELECT 1 FROM trip_items i WHERE i.id = c.item_id))",
                params![draft.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphans, 0, "the draft's segments and parked options went with it");
    }

    #[test]
    fn a_pinned_thread_past_the_idle_window_keeps_its_trip() {
        // The release only applies to threads that actually die. A `SELECT`
        // "simplified" by dropping `NOT pinned` would still name this thread
        // — the DELETE would spare it, and its trip would be cut loose from
        // a conversation that is very much alive.
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let pinned = s.start_conversation(a, "direct").unwrap();
        let trip = s.upsert_trip(a, "Japan in spring", None, None, Some(pinned)).unwrap();
        s.set_thread_pinned(a, pinned, true).unwrap();
        s.conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(72 * 3600) WHERE id = {pinned};"
            ))
            .unwrap();

        assert_eq!(s.expire_conversations(48 * 3600, &[]).unwrap(), 0, "a pinned thread does not expire");

        assert_eq!(s.trip_owner(trip.id).unwrap(), Some(pinned), "a thread that is still alive kept its trip");
    }

    #[test]
    fn a_thread_with_a_run_in_flight_keeps_its_trip() {
        // Same trap from the other side: a `SELECT` that ignores `except`
        // names a thread the DELETE spares, and the trip of a conversation
        // that is mid-answer is orphaned under it.
        //
        // Two ids in `except` for the reason
        // `expiry_leaves_alone_a_thread_named_as_still_running` gives: one id
        // never exercises the placeholder join.
        let (s, _dir) = test_store();
        let a = s.account_for_telegram(11).unwrap();
        let running = s.start_conversation(a, "direct").unwrap();
        let also_running = s.start_conversation(a, "direct").unwrap();
        let trip = s.upsert_trip(a, "Japan in spring", None, None, Some(running)).unwrap();
        s.conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(72 * 3600) WHERE id IN ({running}, {also_running});"
            ))
            .unwrap();

        assert_eq!(s.expire_conversations(48 * 3600, &[running, also_running]).unwrap(), 0, "both threads were named as running");

        assert_eq!(s.trip_owner(trip.id).unwrap(), Some(running), "a thread still writing its answer kept its trip");
    }

    #[test]
    fn a_web_only_account_is_named_by_its_email_in_the_stats() {
        // Display names come from Telegram. An account that only ever
        // signed in by email had no row there and showed as a dash.
        let (s, _dir) = test_store();
        let by_email = s.account_for_identity("email", "ada@example.com").unwrap();
        let by_telegram = s.account_for_telegram(11).unwrap();
        s.remember_user(by_telegram, "Grace").unwrap();
        // Both ways in: the Telegram name wins, the email is the fallback.
        let both = s.account_for_telegram(22).unwrap();
        s.remember_user(both, "Ada L").unwrap();
        s.link_identity(both, "email", "ada.l@example.com").unwrap();

        let names = s.display_names().unwrap();

        assert_eq!(names.get(&by_email).map(String::as_str), Some("ada@example.com"));
        assert_eq!(names.get(&by_telegram).map(String::as_str), Some("Grace"));
        assert_eq!(names.get(&both).map(String::as_str), Some("Ada L"));
    }

    /// `accounts` and `messages` exactly as they stood at schema 11, before
    /// `debug` and `run_id`. Do NOT update when `MIGRATIONS` changes.
    const PRE_TRACE_TABLES: &str = r#"
CREATE SEQUENCE IF NOT EXISTS accounts_id_seq;
CREATE TABLE accounts (
    id         BIGINT PRIMARY KEY DEFAULT nextval('accounts_id_seq'),
    created_at TIMESTAMP NOT NULL DEFAULT current_timestamp
);
CREATE SEQUENCE IF NOT EXISTS messages_id_seq;
CREATE TABLE messages (
    id              BIGINT PRIMARY KEY DEFAULT nextval('messages_id_seq'),
    conversation_id BIGINT NOT NULL,
    position        BIGINT NOT NULL,
    body            TEXT NOT NULL,
    created_at      TIMESTAMP NOT NULL DEFAULT current_timestamp
);
"#;

    #[test]
    fn a_version_11_database_gains_runs_traces_and_the_debug_flag() {
        // Both columns land on tables that already hold rows in production,
        // and `debug` must be NOT NULL at the end without DuckDB refusing
        // the constraint in the transaction that backfilled it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.duckdb");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            conn.execute_batch(PRE_TRACE_TABLES).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_version (version BIGINT NOT NULL);
                 INSERT INTO schema_version VALUES (11);
                 INSERT INTO accounts (id) VALUES (1);
                 INSERT INTO messages (conversation_id, position, body) VALUES (1, 0, '{}');",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 18);
        assert!(!store.debug_of(1).unwrap(), "backfilled to off");
        let run_id: Option<i64> = store
            .conn()
            .query_row("SELECT run_id FROM messages WHERE position = 0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(run_id, None);
        let nullable: bool = store
            .conn()
            .query_row("SELECT is_nullable = 'YES' FROM information_schema.columns WHERE table_name = 'accounts' AND column_name = 'debug'", [], |r| r.get(0))
            .unwrap();
        assert!(!nullable, "debug must end up NOT NULL");
    }

    fn a_row(seq: i64, tool: &str, result: Option<&str>) -> scout_api::TraceRow {
        scout_api::TraceRow {
            seq, kind: "tool".into(), tool: Some(tool.into()), args: Some(serde_json::json!({"q": seq})),
            nested: false, started_at: "2026-09-13T10:00:00Z".into(), duration_ms: Some(120),
            status: Some("ok".into()), detail: None, result: result.map(str::to_string), truncated: false,
        }
    }

    #[test]
    fn a_run_is_opened_traced_closed_and_read_back_by_its_owner_only() {
        let (store, _dir) = test_store();
        let me = store.account_for_telegram(1).unwrap();
        let other = store.account_for_telegram(2).unwrap();
        let conv = store.start_conversation(me, "direct").unwrap();

        let run_id = store.open_run(me, conv).unwrap();
        store.append_traces(run_id, &[a_row(0, "search_web", Some(r#"{"hits":3}"#)), a_row(1, "fetch_page", None)]).unwrap();
        store.close_run(run_id, "answered", None).unwrap();

        let (run, rows) = store.trace_of(run_id, me).unwrap().expect("the owner reads it");
        assert_eq!(run.outcome.as_deref(), Some("answered"));
        assert!(run.ended_at.is_some());
        assert_eq!(rows.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(rows[0].result.as_deref(), Some(r#"{"hits":3}"#));
        assert!(store.trace_of(run_id, other).unwrap().is_none(), "someone else's run is not found");
        assert!(store.trace_of(run_id + 100, me).unwrap().is_none());
    }

    #[test]
    fn a_result_past_the_cap_is_cut_and_says_so() {
        let (store, _dir) = test_store();
        let me = store.account_for_telegram(1).unwrap();
        let conv = store.start_conversation(me, "direct").unwrap();
        let run_id = store.open_run(me, conv).unwrap();
        // 3 bytes each: over the cap, and since the cap is even the cut
        // lands mid-character, so the boundary loop has to move it.
        let long = "€".repeat(TRACE_RESULT_CAP);
        store.append_traces(run_id, &[a_row(0, "search_web", Some(&long))]).unwrap();
        let (_, rows) = store.trace_of(run_id, me).unwrap().unwrap();
        let kept = rows[0].result.as_deref().unwrap();
        assert!(rows[0].truncated);
        assert!(kept.len() <= TRACE_RESULT_CAP && kept.chars().all(|c| c == '€'), "cut on a character boundary");
    }

    #[test]
    fn messages_remember_their_run_and_the_debug_flag_flips() {
        let (store, _dir) = test_store();
        let me = store.account_for_telegram(1).unwrap();
        let conv = store.start_conversation(me, "direct").unwrap();
        let run_id = store.open_run(me, conv).unwrap();
        store.append_messages(conv, None, &["{\"a\":1}".into()]).unwrap();
        store.append_messages(conv, Some(run_id), &["{\"b\":2}".into()]).unwrap();
        let got = store.conversation_messages(conv, 10).unwrap();
        assert_eq!(got, vec![(None, "{\"a\":1}".to_string()), (Some(run_id), "{\"b\":2}".to_string())]);

        assert!(!store.debug_of(me).unwrap());
        store.set_debug(me, true).unwrap();
        assert!(store.debug_of(me).unwrap());
        store.set_debug(me, false).unwrap();
        assert!(!store.debug_of(me).unwrap());
    }

    #[test]
    fn trimming_keeps_the_newest_runs_and_their_rows() {
        let (store, _dir) = test_store();
        let me = store.account_for_telegram(1).unwrap();
        let conv = store.start_conversation(me, "direct").unwrap();
        let ids: Vec<i64> = (0..5).map(|_| store.open_run(me, conv).unwrap()).collect();
        for id in &ids {
            store.append_traces(*id, &[a_row(0, "search_web", None)]).unwrap();
        }
        let gone = store.trim_traces(2).unwrap();
        assert_eq!(gone, 3);
        assert!(store.trace_of(ids[0], me).unwrap().is_none());
        assert!(store.trace_of(ids[4], me).unwrap().is_some());
        let rows: i64 = store.conn().query_row("SELECT count(*) FROM run_traces", [], |r| r.get(0)).unwrap();
        assert_eq!(rows, 2, "rows go with their runs");
    }

    #[test]
    fn a_version_15_database_gains_the_inbox_tables_and_a_handle_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scout.duckdb");
        {
            let conn = duckdb::Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS).unwrap();
            conn.execute_batch("DROP TABLE IF EXISTS arrivals; DROP TABLE IF EXISTS attachments; DROP TABLE IF EXISTS inbound_mail; ALTER TABLE accounts DROP COLUMN handle;").unwrap();
            conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL); DELETE FROM schema_version; INSERT INTO schema_version VALUES (15); INSERT INTO accounts (id) VALUES (1);").unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 18);
        assert_eq!(store.handle_of(1).unwrap(), None);
        // Written through the step-16 tables, read through the same code
        // that reads a fresh database: drift between the two DDLs shows here.
        let m = mail(&store, 1, "re_1");
        store.insert_attachment(m, "ticket.pdf", "application/pdf", Some(b"%PDF"), Some("Row 12")).unwrap();
        let id = store.insert_arrival(1, m, &NewArrival { booking: true, summary: "x".into(), ..Default::default() }).unwrap();
        assert!(store.arrival_of(id, 1).unwrap().is_some());
    }

    /// A database in the shape step 16 left it: `arrivals` without the
    /// three columns step 17 adds. Built by dropping them from the finished
    /// shape rather than by restating the old DDL, so it cannot drift from
    /// what `MIGRATIONS` says the rest of the table is.
    fn version_sixteen_db() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v16.duckdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS).unwrap();
        conn.execute_batch(
            "ALTER TABLE arrivals DROP COLUMN airline;
             ALTER TABLE arrivals DROP COLUMN flight_number;
             ALTER TABLE arrivals DROP COLUMN stops;
             CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL);
             DELETE FROM schema_version;
             INSERT INTO schema_version VALUES (16);
             INSERT INTO accounts (id) VALUES (1);",
        )
        .unwrap();
        drop(conn);
        (dir, path)
    }

    #[test]
    fn a_version_16_database_gains_the_flight_columns_and_takes_a_flight_arrival() {
        // Every other fixture builds `arrivals` from `MIGRATIONS`, which
        // already carries these columns, so without this nothing ever runs
        // step 17's ALTERs against the shape a deployed database is in —
        // and a broken step would pass the whole suite.
        let (_dir, path) = version_sixteen_db();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 18);
        let m = mail(&store, 1, "re_1");
        let id = store
            .insert_arrival(1, m, &NewArrival {
                booking: true,
                kind: Some("flight".into()),
                origin: Some("AMS".into()),
                destination: Some("HKG".into()),
                airline: Some("KLM".into()),
                flight_number: Some("KL887".into()),
                stops: Some("CDG".into()),
                date: Some("2026-11-02".into()),
                summary: "AMS → HKG".into(),
                ..Default::default()
            })
            .unwrap();
        let got = store.arrival_of(id, 1).unwrap().expect("written through the migrated table");
        assert_eq!((got.airline.as_deref(), got.flight_number.as_deref()), (Some("KLM"), Some("KL887")));
        assert_eq!(got.stops, vec!["CDG".to_string()]);
    }

    /// A database in the shape step 17 left it: no `mail_parts` at all.
    /// Built by dropping the table from the finished shape rather than by
    /// restating the old DDL, so it cannot drift from what `MIGRATIONS`
    /// says the rest of the database is.
    fn version_seventeen_db() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("v17.duckdb");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS).unwrap();
        conn.execute_batch(
            "DROP TABLE IF EXISTS mail_parts;
             DROP SEQUENCE IF EXISTS mail_parts_id_seq;
             CREATE TABLE IF NOT EXISTS schema_version (version BIGINT NOT NULL);
             DELETE FROM schema_version;
             INSERT INTO schema_version VALUES (17);
             INSERT INTO accounts (id) VALUES (1);",
        )
        .unwrap();
        drop(conn);
        (dir, path)
    }

    #[test]
    fn the_step_that_adds_mail_parts_builds_the_table_a_fresh_database_has() {
        // Two descriptions of one table — `MIGRATIONS` and step 18 — and
        // nothing but this keeps them in step. Comparing a fresh database
        // with a migrated one, the way the `arrivals` test above does,
        // would not: `Store::open` runs `MIGRATIONS` before the steps, so
        // on a database that has no `mail_parts` the fresh DDL creates it
        // and step 18 finds it there. That is what makes a new table safe
        // to add — and exactly why a step whose DDL had drifted would
        // never be caught by opening anything. So run the step by itself,
        // against a database that has never seen `MIGRATIONS`.
        let (fresh, _d1) = test_store();
        let dir = TempDir::new().unwrap();
        let conn = Connection::open(dir.path().join("step18.duckdb")).unwrap();
        conn.execute_batch(STEP_18_MAIL_PARTS).unwrap();
        assert_eq!(shape(&fresh.conn(), "mail_parts"), shape(&conn, "mail_parts"));
    }

    #[test]
    fn a_migrated_database_comes_up_at_18_and_takes_a_mails_parts_too() {
        // `test_store` opens a fresh file; production opens one at schema
        // 17. This covers the second, and it is named for what it checks
        // rather than for step 18, which is not what puts the table there:
        // `Store::open` runs `MIGRATIONS` unconditionally before applying
        // any step, and `MIGRATIONS` is all CREATE TABLE IF NOT EXISTS, so
        // `mail_parts` appears either way — `Step::Sql("")` leaves this
        // green. What step 18 earns is the version bump, which this does
        // pin, and the backup that a pending step makes the runner take
        // before it touches anything. The same as step 6; see
        // `a_migrated_database_gets_login_tokens_too_not_just_a_fresh_one`.
        // For the DDLs agreeing, see the test above.
        let (_dir, path) = version_seventeen_db();
        let store = Store::open(&path).unwrap();
        assert_eq!(store.schema_version().unwrap(), 18);
        let part = mail_part("att_1", Some("inline"), None);
        let m = store
            .insert_mail(1, "re_1", "hotel@example.com", None, None, None, false, std::slice::from_ref(&part))
            .unwrap()
            .expect("new");
        assert_eq!(store.mail_parts_of(m).unwrap(), vec![part]);
    }

    #[test]
    fn a_handle_is_unique_and_can_be_retired() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let b = store.account_for_telegram(2).unwrap();
        assert!(store.set_handle(a, "sasha").unwrap(), "free");
        assert!(!store.set_handle(b, "sasha").unwrap(), "taken");
        assert_eq!(store.account_for_handle("sasha").unwrap(), Some(a));
        assert!(store.set_handle(a, "sasha.k").unwrap(), "changed");
        assert_eq!(store.account_for_handle("sasha").unwrap(), None, "retired, not redirected");
        assert!(store.set_handle(b, "sasha").unwrap(), "free again");
        assert!(store.set_handle(a, "SASHA").unwrap(), "case is normalise_handle's job, not the store's");
    }

    fn mail(store: &Store, account: i64, provider_id: &str) -> i64 {
        store.insert_mail(account, provider_id, "hotel@example.com", Some("Your booking"), Some("Check-in 12 Oct"), None, false, &[]).unwrap().expect("new")
    }

    /// One part as the webhook described it.
    fn mail_part(provider_id: &str, disposition: Option<&str>, cid: Option<&str>) -> MailPart {
        MailPart {
            provider_id: provider_id.into(),
            content_disposition: disposition.map(Into::into),
            content_id: cid.map(Into::into),
        }
    }

    #[test]
    fn a_mails_parts_are_stored_with_it_and_a_redelivery_adds_none() {
        // The two fields arrive with the mail and nowhere else, so they
        // are written in the same breath as the row they belong to. A
        // redelivery stores no mail, and so must store no parts: Resend
        // retries a webhook freely, and a second copy of every part would
        // be a table that grows with the retries rather than the mail.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let parts =
            [mail_part("att_tkt", Some("attachment"), None), mail_part("att_logo", Some("inline"), Some("<logo@m>"))];
        let m = store
            .insert_mail(a, "re_1", "hotel@example.com", None, None, None, false, &parts)
            .unwrap()
            .expect("new");
        assert_eq!(store.mail_parts_of(m).unwrap(), parts.to_vec());
        assert_eq!(
            store.insert_mail(a, "re_1", "x", None, None, None, false, &[mail_part("att_x", None, None)]).unwrap(),
            None,
            "a redelivery is a no-op",
        );
        assert_eq!(store.mail_parts_of(m).unwrap(), parts.to_vec(), "the redelivery wrote parts of its own");
        // Another mail's parts are its own, and a mail that arrived with
        // none reads back as none rather than as everyone's.
        let n = mail(&store, a, "re_2");
        assert!(store.mail_parts_of(n).unwrap().is_empty());
    }

    #[test]
    fn mail_is_stored_once_per_provider_id_and_worked_oldest_first() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let first = mail(&store, a, "re_1");
        assert_eq!(store.insert_mail(a, "re_1", "x", None, None, None, false, &[]).unwrap(), None, "a redelivery is a no-op");
        let second = mail(&store, a, "re_2");
        let due = store.mail_to_work(10).unwrap();
        assert_eq!(due.iter().map(|m| m.id).collect::<Vec<_>>(), vec![first, second]);
        store.mail_attempted(first).unwrap();
        store.mail_done(first).unwrap();
        assert_eq!(store.mail_to_work(10).unwrap().len(), 1);
        store.mail_attempted(second).unwrap();
        store.age_attempts(second).unwrap();
        store.mail_attempted(second).unwrap();
        store.age_attempts(second).unwrap();
        assert_eq!(store.mail_to_work(10).unwrap().len(), 1, "two attempts leave one more");
        store.mail_attempted(second).unwrap();
        store.age_attempts(second).unwrap();
        assert!(store.mail_to_work(10).unwrap().is_empty(), "the third attempt is the last");
        store.mail_failed(second, "the model said no").unwrap();
        assert!(store.mail_to_work(10).unwrap().is_empty(), "failed mail is not retried");
    }

    #[test]
    fn a_mail_just_attempted_waits_its_turn_and_an_aged_one_is_served() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        store.mail_attempted(m).unwrap();
        assert!(store.mail_to_work(10).unwrap().is_empty(), "attempted a moment ago: not yet");
        store.age_attempts(m).unwrap();
        assert_eq!(store.mail_to_work(10).unwrap().iter().map(|m| m.id).collect::<Vec<_>>(), vec![m]);
    }

    #[test]
    fn an_attempt_handed_back_is_not_spent_but_the_wait_still_stands() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        store.mail_attempted(m).unwrap();
        store.mail_unattempted(m).unwrap();
        assert!(store.mail_to_work(10).unwrap().is_empty(), "handed back a moment ago: the spacing still applies");
        store.age_attempts(m).unwrap();
        assert_eq!(store.mail_to_work(10).unwrap()[0].attempts, 0, "the attempt was not spent");
        store.mail_unattempted(m).unwrap();
        store.age_attempts(m).unwrap();
        assert_eq!(store.mail_to_work(10).unwrap()[0].attempts, 0, "and never goes below nothing");
        // Three outages in a row leave the mail as fresh as it came.
        for _ in 0..MAIL_ATTEMPTS {
            store.mail_attempted(m).unwrap();
            store.mail_unattempted(m).unwrap();
            store.age_attempts(m).unwrap();
        }
        assert_eq!(store.mail_to_work(10).unwrap().len(), 1);
    }

    #[test]
    fn a_mail_knows_whether_it_was_read() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        assert!(!store.mail_has_arrival(m).unwrap());
        store.insert_arrival(a, m, &NewArrival { booking: false, summary: "ad".into(), ..Default::default() }).unwrap();
        assert!(store.mail_has_arrival(m).unwrap());
        assert!(!store.mail_has_arrival(m + 1).unwrap(), "no such mail, no such reading");
    }

    #[test]
    fn an_arrival_moves_from_pending_to_added_or_ignored_and_is_read_by_its_owner() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let b = store.account_for_telegram(2).unwrap();
        let m = mail(&store, a, "re_1");
        let id = store.insert_arrival(a, m, &NewArrival {
            booking: true, kind: Some("stay".into()), title: Some("Hotel Alfama".into()), place: Some("Lisbon".into()),
            origin: None, destination: None, airline: None, flight_number: None, stops: None, date: Some("2026-10-12".into()), starts_at: None, ends_at: Some("2026-10-15".into()),
            timezone: None, confirmation_code: Some("ABC".into()), price: Some(320.0), currency: Some("EUR".into()),
            travellers: None, confidence: Some(0.9), summary: "Hotel Alfama, 12–15 Oct".into(), trip_id: None,
        }).unwrap();
        let view = store.inbox_view(a).unwrap();
        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.pending[0].title.as_deref(), Some("Hotel Alfama"));
        assert!(store.inbox_view(b).unwrap().pending.is_empty());
        assert!(store.arrival_of(id, b).unwrap().is_none(), "not theirs");
        assert!(!store.decide_arrival(id, b, "ignored", None).unwrap(), "not theirs to decide");
        assert_eq!(store.inbox_view(a).unwrap().pending.len(), 1);
        assert!(store.decide_arrival(id, a, "ignored", None).unwrap());
        let view = store.inbox_view(a).unwrap();
        assert!(view.pending.is_empty());
        assert_eq!(view.other[0].reason, "ignored");
        // The decision is the claim: a second one, whatever it says, finds
        // nothing pending to decide. And a non-booking was never pending.
        assert!(!store.decide_arrival(id, a, "added", Some(1)).unwrap(), "already decided");
        assert_eq!(store.arrival_of(id, a).unwrap().unwrap().status, "ignored");
        let n = mail(&store, a, "re_2");
        let spam = store.insert_arrival(a, n, &NewArrival { booking: false, summary: "ad".into(), ..Default::default() }).unwrap();
        assert!(!store.decide_arrival(spam, a, "ignored", None).unwrap(), "nothing to decide on a non-booking");
        // Reopened, it can be decided again; the item it became is noted
        // by its own write.
        store.reopen_arrival(id).unwrap();
        assert!(store.decide_arrival(id, a, "added", None).unwrap());
        store.note_arrival_item(id, 7).unwrap();
        let item_of = |id: i64| -> (String, Option<i64>) {
            store.conn().query_row("SELECT status, item_id FROM arrivals WHERE id = ?", params![id], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
        };
        assert_eq!(item_of(id), ("added".to_string(), Some(7)));
        // A reopen forgets the item too, not just the status.
        store.reopen_arrival(id).unwrap();
        assert_eq!(item_of(id), ("pending".to_string(), None));
    }

    #[test]
    fn a_trip_is_read_by_id_only_by_its_owner() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let b = store.account_for_telegram(2).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        let got = store.trip_by_id(a, trip.id).unwrap().expect("theirs");
        assert_eq!((got.name.as_str(), got.items.len()), ("Lisbon", 1));
        assert!(store.trip_by_id(b, trip.id).unwrap().is_none(), "not theirs");
        assert!(store.trip_by_id(a, trip.id + 100).unwrap().is_none());
    }

    #[test]
    fn a_trip_carries_the_tickets_its_bookings_arrived_with() {
        // The other half of what `attach_to_item` is for: the file survives
        // the mail, and the trip it survived onto has to show it. Without
        // this the owner has a ticket nothing on the page draws.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        // Named so that the order they were written in is not the order
        // their names sort in: the read has to be pinned to one of them,
        // and it is the ids — the order the mail carried them in.
        let boarding = store.insert_attachment(m, "zulu-boarding.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let receipt = store.insert_attachment(m, "alpha-receipt.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let loose = store.insert_attachment(m, "logo.png", "image/png", Some(&[1, 2]), None).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        let trip = store.add_item(trip.id, NewItem {
            kind: "stay".into(), title: "Hotel Alfama".into(), place: None, date: "2026-10-12".into(),
            starts_at: None, ends_at: None, notes: None, booked: true, confirmation_code: None,
            price: None, currency: None, arrival_id: None,
        }).unwrap();
        let trip = store.add_item(trip.id, NewItem {
            kind: "activity".into(), title: "Museu do Azulejo".into(), place: None, date: "2026-10-13".into(),
            starts_at: None, ends_at: None, notes: None, booked: false, confirmation_code: None,
            price: None, currency: None, arrival_id: None,
        }).unwrap();
        let hotel = trip.items.iter().find(|i| i.title == "Hotel Alfama").expect("the stay").id;
        // Attached out of order on purpose: the read orders them, so a
        // reload does not shuffle the links under the reader's cursor.
        store.attach_to_item(receipt, hotel).unwrap();
        store.attach_to_item(boarding, hotel).unwrap();

        let got = store.trip_by_id(a, trip.id).unwrap().expect("theirs");
        let stay = got.items.iter().find(|i| i.title == "Hotel Alfama").expect("the stay");
        assert!(boarding < receipt, "the ids are the order the mail carried them in");
        assert_eq!(
            stay.attachments.iter().map(|f| f.filename.as_str()).collect::<Vec<_>>(),
            vec!["zulu-boarding.pdf", "alpha-receipt.pdf"],
            "read back in id order, not by name and not by when they were attached",
        );
        assert_eq!(
            stay.attachments[0],
            scout_api::AttachmentRef { id: boarding, filename: "zulu-boarding.pdf".into(), mime: "application/pdf".into() },
        );
        let museum = got.items.iter().find(|i| i.title == "Museu do Azulejo").expect("the activity");
        assert!(museum.attachments.is_empty(), "nobody attached anything to it");
        // Still the mail's, so it belongs to no item — it shows under Other
        // mail, not on the trip.
        assert!(
            got.items.iter().flat_map(|i| &i.attachments).all(|f| f.id != loose),
            "a file that never joined an item was drawn on one",
        );
    }

    #[test]
    fn the_sweep_deletes_old_mail_but_keeps_an_attachment_that_belongs_to_an_item() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        let kept = store.insert_attachment(m, "ticket.pdf", "application/pdf", Some(b"%PDF"), Some("Row 12")).unwrap();
        let loose = store.insert_attachment(m, "logo.png", "image/png", Some(&[1, 2]), None).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        let trip = store.add_item(trip.id, NewItem {
            kind: "stay".into(), title: "Hotel Alfama".into(), place: None, date: "2026-10-12".into(),
            starts_at: None, ends_at: None, notes: None, booked: true, confirmation_code: None,
            price: None, currency: None, arrival_id: None,
        }).unwrap();
        store.attach_to_item(kept, trip.items[0].id).unwrap();
        // A file joins the first item that claims it and stays there.
        store.attach_to_item(kept, trip.items[0].id + 1).unwrap();
        assert_eq!(store.attachment(kept).unwrap().unwrap().1, Some(trip.items[0].id), "first wins");
        store.conn().execute("UPDATE inbound_mail SET received_at = received_at - INTERVAL 40 DAY WHERE id = ?", params![m]).unwrap();
        let gone = store.sweep_inbox(30).unwrap();
        assert_eq!(gone, 1, "one mail row");
        assert!(store.attachment(kept).unwrap().is_some());
        assert!(store.attachment(loose).unwrap().is_none());
        assert_eq!(store.attachment_owner(kept).unwrap(), Some(a), "owned through the item once the mail is gone");
    }

    /// A stay on `trip`, booked, with a ticket already on it: the shape
    /// every test below starts from. Returns the trip as written and the
    /// new item's id.
    fn stay_with(store: &Store, trip_id: i64, title: &str) -> (Trip, i64) {
        let trip = store
            .add_item(trip_id, NewItem {
                kind: "stay".into(), title: title.into(), place: None, date: "2026-10-12".into(),
                starts_at: None, ends_at: None, notes: None, booked: true, confirmation_code: None,
                price: None, currency: None, arrival_id: None,
            })
            .unwrap();
        let id = trip.items.iter().find(|i| i.title == title).expect("just added").id;
        (trip, id)
    }

    /// Deletes a mail the way its owner's x does, refusing to pretend it
    /// worked: the delete has preconditions, and a test that skipped them
    /// would be testing nothing.
    fn delete_mail_now(store: &Store, account: i64, mail_id: i64) {
        store.mail_done(mail_id).unwrap();
        assert_eq!(store.delete_mail(account, mail_id).unwrap(), MailGone::Gone);
    }

    #[test]
    fn removing_an_item_gives_its_files_back_to_the_mail_and_takes_the_ones_no_mail_is_left_for() {
        // The item is the second of an attachment's two owners, so taking
        // it away decides the file's fate. If the mail it came with is
        // still there the file is still perfectly reachable under Other
        // mail, and destroying a traveller's ticket because they took one
        // leg off a trip would be indefensible. If the mail has already
        // gone, the item was the only thing that could reach those bytes:
        // left behind, the row answers `attachment_owner` with nothing,
        // which no reader may download and no sweep can find — the sweeps
        // are keyed on mail ids that no longer exist.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let live = mail(&store, a, "re_1");
        let doomed = mail(&store, a, "re_2");
        let ticket = store.insert_attachment(live, "ticket.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let boarding = store.insert_attachment(doomed, "boarding.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        let (trip, hotel) = stay_with(&store, trip.id, "Hotel Alfama");
        store.attach_to_item(ticket, hotel).unwrap();
        store.attach_to_item(boarding, hotel).unwrap();
        delete_mail_now(&store, a, doomed);
        assert_eq!(store.attachment_owner(boarding).unwrap(), Some(a), "kept by the item, which is the state this is about");

        store.drop_item(trip.id, 1).unwrap();

        assert_eq!(
            store.attachment(ticket).unwrap().expect("the mail it came with is still there").1,
            None,
            "the ticket was not handed back to its mail, so nothing will ever sweep it",
        );
        assert_eq!(store.attachment_owner(ticket).unwrap(), Some(a), "handed back but unreachable");
        assert!(
            store.attachment(boarding).unwrap().is_none(),
            "a file whose mail is gone outlived the only item that could reach it",
        );
    }

    #[test]
    fn deleting_a_trip_decides_every_item_s_files_the_same_way_removing_one_item_would() {
        // `delete_trip` takes every item at once, so it is the same rule
        // applied to a set — and the set is what makes it worth its own
        // test: one statement over many items must not quietly take the
        // files of the ones whose mail is alive along with the rest.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let live = mail(&store, a, "re_1");
        let doomed = mail(&store, a, "re_2");
        let ticket = store.insert_attachment(live, "ticket.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let boarding = store.insert_attachment(doomed, "boarding.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        let (_, hotel) = stay_with(&store, trip.id, "Hotel Alfama");
        let (_, museum) = stay_with(&store, trip.id, "Museu do Azulejo");
        store.attach_to_item(ticket, hotel).unwrap();
        store.attach_to_item(boarding, museum).unwrap();
        delete_mail_now(&store, a, doomed);

        assert!(store.delete_trip(a, "Lisbon").unwrap());

        assert_eq!(
            store.attachment(ticket).unwrap().expect("its mail is still there").1,
            None,
            "the ticket was left pointing at an item the trip took with it",
        );
        assert!(store.attachment(boarding).unwrap().is_none(), "a file no mail and no item can reach was kept");
    }

    #[test]
    fn deleting_a_thread_decides_its_trips_files_the_way_removing_one_item_does() {
        // The third path that deletes `trip_items`, and a reachable one: a
        // booking Added from the inbox lands on whichever trip the reader
        // chose, which can be a thread's own trip, and deleting the thread
        // cascades to it. Same rule, because the rule is about the file,
        // not about which button deleted the item.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let chat = store.start_conversation(a, "direct").unwrap();
        let live = mail(&store, a, "re_1");
        let doomed = mail(&store, a, "re_2");
        let ticket = store.insert_attachment(live, "ticket.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let boarding = store.insert_attachment(doomed, "boarding.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, Some(chat)).unwrap();
        let (_, hotel) = stay_with(&store, trip.id, "Hotel Alfama");
        store.attach_to_item(ticket, hotel).unwrap();
        store.attach_to_item(boarding, hotel).unwrap();
        delete_mail_now(&store, a, doomed);

        assert!(store.delete_conversation(a, chat).unwrap());

        assert_eq!(
            store.attachment(ticket).unwrap().expect("its mail is still there").1,
            None,
            "the ticket was left pointing at an item the thread's cascade took",
        );
        assert!(store.attachment(boarding).unwrap().is_none(), "a file no mail and no item can reach was kept");
    }

    #[test]
    fn a_draft_collected_by_thread_expiry_decides_its_files_too() {
        // The fourth path, and the one nothing reaches today: a draft is
        // kept the moment a booking lands on it, so a draft holding a
        // ticket is a state only a future edit could produce. It follows
        // the same rule anyway — this delete is on a timer with nobody
        // watching, and discovering the difference later would mean
        // discovering it as rows nobody can account for.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let stale = store.start_conversation(a, "direct").unwrap();
        let live = mail(&store, a, "re_1");
        let doomed = mail(&store, a, "re_2");
        let ticket = store.insert_attachment(live, "ticket.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let boarding = store.insert_attachment(doomed, "boarding.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let draft = store.upsert_trip(a, "Draft loop", None, None, Some(stale)).unwrap();
        let (_, hotel) = stay_with(&store, draft.id, "Hotel Alfama");
        store.attach_to_item(ticket, hotel).unwrap();
        store.attach_to_item(boarding, hotel).unwrap();
        delete_mail_now(&store, a, doomed);
        // A formatted literal interval, for the binding failure
        // `expiry_deletes_the_draft_and_lets_the_kept_trip_go_free` names.
        store
            .conn()
            .execute_batch(&format!(
                "UPDATE conversations SET updated_at = CAST(current_timestamp AS TIMESTAMP) - to_seconds(72 * 3600) WHERE id = {stale};"
            ))
            .unwrap();

        assert_eq!(store.expire_conversations(48 * 3600, &[]).unwrap(), 1);

        assert!(store.find_trip(a, "Draft loop").unwrap().is_none(), "the draft must go for this test to mean anything");
        assert_eq!(
            store.attachment(ticket).unwrap().expect("its mail is still there").1,
            None,
            "the ticket was left pointing at an item the expiry took",
        );
        assert!(store.attachment(boarding).unwrap().is_none(), "a file no mail and no item can reach was kept");
    }

    #[test]
    fn the_orphan_sweep_takes_only_the_files_nothing_can_reach() {
        // This sweep deletes rows on a schedule with nobody watching, so
        // what matters is everything it must not touch: a loose file whose
        // mail is alive, a file on an item of a live trip, and a file whose
        // mail is gone but whose item still holds it — the exact state the
        // inbox sweep is careful to leave behind. Only the fourth, which
        // answers `attachment_owner` with nothing and which no other path
        // can ever delete, is the sweep's.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let live = mail(&store, a, "re_1");
        let doomed = mail(&store, a, "re_2");
        let loose = store.insert_attachment(live, "logo.png", "image/png", Some(&[1, 2]), None).unwrap();
        let on_item = store.insert_attachment(live, "ticket.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let outlived_its_mail = store.insert_attachment(doomed, "boarding.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        let (_, hotel) = stay_with(&store, trip.id, "Hotel Alfama");
        store.attach_to_item(on_item, hotel).unwrap();
        store.attach_to_item(outlived_its_mail, hotel).unwrap();
        delete_mail_now(&store, a, doomed);
        // The orphan is made the way the bug made the ones already in the
        // live database: the item goes out from under a file whose mail has
        // gone. Written as raw SQL on purpose — the delete paths are fixed
        // now, so nothing above this line can produce this row any more,
        // and it is precisely the rows they left behind that this sweep is
        // here to clear.
        let orphan = store.insert_attachment(doomed, "old.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        store.conn().execute("UPDATE attachments SET item_id = ? WHERE id = ?", params![hotel + 500, orphan]).unwrap();
        assert_eq!(store.attachment_owner(orphan).unwrap(), None, "the row must be unreachable for this test to mean anything");

        assert_eq!(store.sweep_orphaned_attachments().unwrap(), 1);

        assert!(store.attachment(orphan).unwrap().is_none(), "the unreachable row survived the sweep");
        assert!(store.attachment(loose).unwrap().is_some(), "the sweep took a loose file whose mail is still there");
        assert!(store.attachment(on_item).unwrap().is_some(), "the sweep took a file on an item of a live trip");
        assert!(
            store.attachment(outlived_its_mail).unwrap().is_some(),
            "the sweep took a ticket the inbox sweep deliberately kept — owned through its item",
        );
        assert_eq!(store.sweep_orphaned_attachments().unwrap(), 0, "a second pass found work that was already done");
    }

    #[test]
    fn a_mails_parts_go_with_it_whether_the_sweep_or_its_owner_takes_it() {
        // A part is never anything but its mail's — there is no `item_id`
        // to make it a trip's, the way a ticket becomes one — so both
        // paths that delete a mail take its parts with it. Left behind,
        // they would be rows describing a mail nobody can name, growing
        // by one mail a day forever.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let parts = [mail_part("att_tkt", Some("attachment"), None)];
        let swept = store
            .insert_mail(a, "re_1", "hotel@example.com", None, None, None, false, &parts)
            .unwrap()
            .expect("new");
        let by_hand = store
            .insert_mail(a, "re_2", "hotel@example.com", None, None, None, false, &parts)
            .unwrap()
            .expect("new");
        store.conn().execute("UPDATE inbound_mail SET received_at = received_at - INTERVAL 40 DAY WHERE id = ?", params![swept]).unwrap();
        assert_eq!(store.sweep_inbox(30).unwrap(), 1);
        assert!(store.mail_parts_of(swept).unwrap().is_empty(), "the sweep left the parts of a mail it forgot");

        store.mail_done(by_hand).unwrap();
        assert_eq!(store.delete_mail(a, by_hand).unwrap(), MailGone::Gone);
        assert!(store.mail_parts_of(by_hand).unwrap().is_empty(), "the x on the row left the parts behind");
    }

    #[test]
    fn a_pending_arrival_keeps_its_old_mail_through_the_sweep() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        let arrival = NewArrival {
            booking: true, kind: None, title: None, place: None, origin: None, destination: None,
            airline: None, flight_number: None, stops: None, date: None, starts_at: None, ends_at: None, timezone: None, confirmation_code: None,
            price: None, currency: None, travellers: None, confidence: None, summary: "x".into(), trip_id: None,
        };
        let id = store.insert_arrival(a, m, &arrival).unwrap();
        let ticket = store.insert_attachment(m, "ticket.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        store.conn().execute("UPDATE inbound_mail SET received_at = received_at - INTERVAL 40 DAY WHERE id = ?", params![m]).unwrap();
        assert_eq!(store.sweep_inbox(30).unwrap(), 0, "undecided, so kept");
        assert!(store.arrival_of(id, a).unwrap().is_some());
        assert!(store.attachment(ticket).unwrap().is_some(), "the ticket waits with its booking");
        assert!(store.decide_arrival(id, a, "added", Some(5)).unwrap());
        assert_eq!(store.sweep_inbox(30).unwrap(), 1);
        assert!(store.arrival_of(id, a).unwrap().is_none());
    }

    #[test]
    fn a_mail_deleted_by_hand_goes_the_way_the_sweep_would_have_taken_it() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let b = store.account_for_telegram(2).unwrap();
        let m = mail(&store, a, "re_1");
        let kept = store.insert_attachment(m, "ticket.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let loose = store.insert_attachment(m, "logo.png", "image/png", Some(&[1, 2]), None).unwrap();
        let reading = store
            .insert_arrival(a, m, &NewArrival { booking: false, summary: "a newsletter".into(), ..Default::default() })
            .unwrap();
        // The worker writes the reading and then marks the mail, in that
        // order; a mail it has not finished with is refused below.
        assert_eq!(store.delete_mail(a, m).unwrap(), MailGone::Unsettled, "still being read");
        store.mail_done(m).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        let trip = store.add_item(trip.id, NewItem {
            kind: "stay".into(), title: "Hotel Alfama".into(), place: None, date: "2026-10-12".into(),
            starts_at: None, ends_at: None, notes: None, booked: true, confirmation_code: None,
            price: None, currency: None, arrival_id: None,
        }).unwrap();
        store.attach_to_item(kept, trip.items[0].id).unwrap();
        assert_eq!(store.inbox_view(a).unwrap().other.len(), 1, "it is on the page to begin with");
        // An id from the page proves nothing: the owner is half the key.
        assert_eq!(store.delete_mail(b, m).unwrap(), MailGone::NotFound, "not theirs");
        assert_eq!(store.inbox_view(a).unwrap().other.len(), 1, "and nothing of it went");
        assert_eq!(store.delete_mail(a, m).unwrap(), MailGone::Gone);
        assert!(store.inbox_view(a).unwrap().other.is_empty(), "off the page at once");
        assert!(store.arrival_of(reading, a).unwrap().is_none(), "its reading went with it");
        assert!(store.attachment(loose).unwrap().is_none(), "a loose file was only the mail's");
        // The ticket belongs to the trip now, and is still answered for
        // through the item — the same survival the sweep was built for.
        assert!(store.attachment(kept).unwrap().is_some());
        assert_eq!(store.attachment_owner(kept).unwrap(), Some(a));
        // A second press from a tab that has not repainted: already gone.
        assert_eq!(store.delete_mail(a, m).unwrap(), MailGone::NotFound);
    }

    #[test]
    fn a_mail_whose_booking_is_still_waiting_is_refused() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        store.mail_done(m).unwrap();
        let ticket = store.insert_attachment(m, "ticket.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let id = store
            .insert_arrival(a, m, &NewArrival { booking: true, summary: "Hotel Alfama".into(), ..Default::default() })
            .unwrap();
        assert_eq!(store.delete_mail(a, m).unwrap(), MailGone::Waiting);
        assert!(store.arrival_of(id, a).unwrap().is_some(), "nothing was deleted");
        assert!(store.attachment(ticket).unwrap().is_some());
        assert_eq!(store.inbox_view(a).unwrap().pending.len(), 1, "still on the timeline");
        // Decided, it is ordinary Other mail and goes like any other.
        assert!(store.decide_arrival(id, a, "ignored", None).unwrap());
        assert_eq!(store.delete_mail(a, m).unwrap(), MailGone::Gone);
        assert!(store.attachment(ticket).unwrap().is_none());
    }

    #[test]
    fn a_mail_the_worker_has_not_finished_with_is_refused_until_it_has() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        // Freshly delivered, and then part-way through a read: neither has
        // a reading yet, so the pending-booking rule says nothing about
        // them, and deleting either would strand the row about to be
        // written against it.
        assert_eq!(store.delete_mail(a, m).unwrap(), MailGone::Unsettled, "new");
        store.mail_attempted(m).unwrap();
        assert_eq!(store.delete_mail(a, m).unwrap(), MailGone::Unsettled, "extracting, attempts left");
        // Out of attempts is the worker having crashed mid-call, over and
        // over: the mail is listed as failed and must be deletable, or the
        // × refuses on exactly the rows most worth dismissing.
        for _ in 1..MAIL_ATTEMPTS {
            store.mail_attempted(m).unwrap();
        }
        assert_eq!(store.inbox_view(a).unwrap().other[0].reason, "failed", "it is on the page");
        assert_eq!(store.delete_mail(a, m).unwrap(), MailGone::Gone);
        // The other two ends: read, and refused outright.
        let read = mail(&store, a, "re_2");
        store.mail_done(read).unwrap();
        assert_eq!(store.delete_mail(a, read).unwrap(), MailGone::Gone);
        let refused = mail(&store, a, "re_3");
        store.mail_failed(refused, "unreadable").unwrap();
        assert_eq!(store.delete_mail(a, refused).unwrap(), MailGone::Gone);
    }

    #[test]
    fn an_item_an_added_booking_became_outlives_the_mail_it_came_from() {
        // The asymmetry with the refused pending case, stated: a booking
        // still waiting holds its mail, but one that has been added has
        // already become a trip item, and the item is the trip's — it does
        // not depend on the mail for anything.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        store.mail_done(m).unwrap();
        let ticket = store.insert_attachment(m, "ticket.pdf", "application/pdf", Some(b"%PDF"), None).unwrap();
        let id = store
            .insert_arrival(a, m, &NewArrival { booking: true, summary: "Hotel Alfama".into(), ..Default::default() })
            .unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        let trip = store.add_item(trip.id, NewItem {
            kind: "stay".into(), title: "Hotel Alfama".into(), place: None, date: "2026-10-12".into(),
            starts_at: None, ends_at: None, notes: None, booked: true, confirmation_code: Some("ABC".into()),
            price: None, currency: None, arrival_id: Some(id),
        }).unwrap();
        let item = trip.items[0].id;
        store.attach_to_item(ticket, item).unwrap();
        assert!(store.decide_arrival(id, a, "added", Some(item)).unwrap());
        assert_eq!(store.delete_mail(a, m).unwrap(), MailGone::Gone);
        let trip = store.trip_by_id(a, trip.id).unwrap().expect("theirs");
        assert_eq!(trip.items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(), vec!["Hotel Alfama"]);
        assert!(trip.items[0].booked, "and still booked, with its code");
        assert_eq!(trip.items[0].confirmation_code.as_deref(), Some("ABC"));
        // The ticket is still downloadable: the item answers for it now.
        assert!(store.attachment(ticket).unwrap().is_some());
        assert_eq!(store.attachment_owner(ticket).unwrap(), Some(a));
    }

    #[test]
    fn the_inbox_view_lists_failed_mail_and_non_bookings_under_other() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let failed = mail(&store, a, "re_1");
        store.mail_failed(failed, "unreadable").unwrap();
        let spam = mail(&store, a, "re_2");
        store.insert_attachment(spam, "logo.png", "image/png", Some(&[1, 2, 3]), None).unwrap();
        let arrival = NewArrival {
            booking: false, kind: None, title: None, place: None, origin: None, destination: None,
            airline: None, flight_number: None, stops: None,
            date: None, starts_at: None, ends_at: None, timezone: None, confirmation_code: None,
            price: None, currency: None, travellers: None, confidence: None, summary: "A newsletter".into(), trip_id: None,
        };
        let id = store.insert_arrival(a, spam, &arrival).unwrap();
        store.mail_forwarded(spam).unwrap();
        let view = store.inbox_view(a).unwrap();
        assert!(view.pending.is_empty(), "a non-booking never waits on the tab");
        let reasons: Vec<(i64, &str)> = view.other.iter().map(|r| (r.mail_id, r.reason.as_str())).collect();
        assert_eq!(reasons, vec![(spam, "not_booking"), (failed, "failed")], "newest first");
        assert_eq!(view.other[0].arrival_id, Some(id));
        assert!(view.other[0].forwarded);
        assert!(view.other[0].received_at.contains('T') && view.other[0].received_at.ends_with('Z'), "{}", view.other[0].received_at);
        assert_eq!(view.other[0].attachments[0].filename, "logo.png");
        assert!(!view.other[1].forwarded);
        assert_eq!(view.other[1].arrival_id, None);
    }

    #[test]
    fn an_arrival_carries_its_trip_name_and_its_attachments() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        let m = mail(&store, a, "re_1");
        let att = store.insert_attachment(m, "ticket.pdf", "application/pdf", None, Some("Row 12")).unwrap();
        let arrival = NewArrival {
            booking: true, kind: Some("flight".into()), title: None, place: None, origin: Some("AMS".into()), destination: Some("LIS".into()),
            airline: Some("KLM".into()), flight_number: Some("KL1691".into()), stops: Some("CDG, DXB".into()),
            date: Some("2026-10-12".into()), starts_at: None, ends_at: None, timezone: None, confirmation_code: None,
            price: None, currency: None, travellers: None, confidence: None, summary: "AMS → LIS".into(), trip_id: Some(trip.id),
        };
        let id = store.insert_arrival(a, m, &arrival).unwrap();
        let got = store.arrival_of(id, a).unwrap().expect("theirs");
        assert_eq!(got.trip_name.as_deref(), Some("Lisbon"));
        // Which flight it was, kept for the leg's own option row. The stops
        // are stored as one comma-separated column and read back as the
        // list the itinerary strip is built from.
        assert_eq!((got.airline.as_deref(), got.flight_number.as_deref()), (Some("KLM"), Some("KL1691")));
        assert_eq!(got.stops, vec!["CDG".to_string(), "DXB".to_string()]);
        // ISO UTC with the `Z`, the shape `threads_of` sends and the page
        // parses without a date library.
        assert!(got.received_at.contains('T') && got.received_at.ends_with('Z'), "{}", got.received_at);
        assert_eq!(got.attachments, vec![scout_api::AttachmentRef { id: att, filename: "ticket.pdf".into(), mime: "application/pdf".into() }]);
        assert_eq!(store.attachment_owner(att).unwrap(), Some(a));
        assert_eq!(store.attachments_of_mail(m).unwrap().len(), 1);
        assert_eq!(store.attachment(att).unwrap().map(|(mail_id, item_id, _, _, bytes)| (mail_id, item_id, bytes)), Some((m, None, None)));
    }

    #[test]
    fn mail_that_ran_out_of_attempts_is_listed_as_failed() {
        // The worker marks a mail failed itself when the model refuses; a
        // crash between attempts never does, and the mail must not vanish.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        for _ in 0..MAIL_ATTEMPTS {
            store.mail_attempted(m).unwrap();
        }
        let view = store.inbox_view(a).unwrap();
        assert_eq!(view.other.iter().map(|r| (r.mail_id, r.reason.as_str())).collect::<Vec<_>>(), vec![(m, "failed")]);
        let fresh = mail(&store, a, "re_2");
        store.mail_attempted(fresh).unwrap();
        assert_eq!(store.inbox_view(a).unwrap().other.len(), 1, "one still being worked is not failed");
    }

    #[test]
    fn a_second_reading_of_a_mail_replaces_the_undecided_one() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        let reading = NewArrival { booking: true, summary: "first".into(), ..Default::default() };
        let first = store.insert_arrival(a, m, &reading).unwrap();
        let second = store.insert_arrival(a, m, &NewArrival { summary: "second".into(), ..reading.clone() }).unwrap();
        let view = store.inbox_view(a).unwrap();
        assert_eq!(view.pending.iter().map(|p| (p.id, p.summary.as_str())).collect::<Vec<_>>(), vec![(second, "second")]);
        assert!(store.arrival_of(first, a).unwrap().is_none());
        // A decided reading is history, not a draft: a retry sits beside it.
        assert!(store.decide_arrival(second, a, "ignored", None).unwrap());
        store.insert_arrival(a, m, &reading).unwrap();
        assert!(store.arrival_of(second, a).unwrap().is_some());
    }

    #[test]
    fn the_several_readings_of_one_mail_are_written_together_and_replaced_together() {
        // One email can confirm a round trip. The delete that keeps a retry
        // from doubling the rows has to run once for the batch, or each
        // insert wipes the one before it and a leg is lost.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        let legs = |tag: &str| {
            vec![
                NewArrival { booking: true, date: Some("2026-11-02".into()), summary: format!("out {tag}"), ..Default::default() },
                NewArrival { booking: true, date: Some("2026-11-23".into()), summary: format!("back {tag}"), ..Default::default() },
            ]
        };
        let first = store.insert_arrivals(a, m, &legs("first")).unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(store.inbox_view(a).unwrap().pending.len(), 2, "both legs are on the page");
        let second = store.insert_arrivals(a, m, &legs("second")).unwrap();
        let view = store.inbox_view(a).unwrap();
        assert_eq!(view.pending.len(), 2, "a retry replaces the two, it does not add two more");
        assert_eq!(
            view.pending.iter().map(|p| p.id).collect::<std::collections::HashSet<_>>(),
            second.iter().copied().collect::<std::collections::HashSet<_>>()
        );
        assert!(first.iter().all(|id| store.arrival_of(*id, a).unwrap().is_none()));
    }

    #[test]
    fn a_mail_with_several_decided_readings_is_one_row_under_other_mail() {
        // A round trip both legs of which the owner ignored is still one
        // email, and Other mail lists mail.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        let ids = store
            .insert_arrivals(a, m, &[
                NewArrival { booking: false, summary: "an ad".into(), ..Default::default() },
                NewArrival { booking: false, summary: "more of the ad".into(), ..Default::default() },
            ])
            .unwrap();
        let view = store.inbox_view(a).unwrap();
        assert_eq!(view.other.len(), 1, "one mail, one row: {view:?}");
        assert_eq!((view.other[0].mail_id, view.other[0].arrival_id), (m, Some(ids[0])), "named by its first reading");
        assert_eq!(view.other[0].reason, "not_booking");
    }

    #[test]
    fn the_row_a_mail_collapses_to_keeps_the_reason_it_would_have_shown() {
        // One mail read as two things: a booking the owner ignored, and
        // something that was never a booking. The CASE ranks not-a-booking
        // above ignored, so the collapse has to keep that row rather than
        // whichever arrived first, or the reason on the page depends on the
        // order the readings happened to be written in.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        let ids = store
            .insert_arrivals(a, m, &[
                NewArrival {
                    booking: true,
                    kind: Some("stay".into()),
                    date: Some("2026-10-12".into()),
                    summary: "a room".into(),
                    ..Default::default()
                },
                NewArrival { booking: false, summary: "an ad under it".into(), ..Default::default() },
            ])
            .unwrap();
        assert!(store.decide_arrival(ids[0], a, "ignored", None).unwrap());
        let view = store.inbox_view(a).unwrap();
        assert_eq!(view.other.len(), 1, "one mail, one row: {view:?}");
        assert_eq!(view.other[0].reason, "not_booking", "the ranked reason, not the older row's");
        assert_eq!(view.other[0].arrival_id, Some(ids[1]));
    }

    #[test]
    fn a_trips_waiting_bookings_are_readable_without_its_items() {
        // A draft holds no items until somebody presses Add, so its dates
        // live only on the arrivals placed on it. Placement reads these to
        // know the draft is about November at all.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let b = store.account_for_telegram(2).unwrap();
        let m = mail(&store, a, "re_1");
        let draft = store.upsert_trip(a, "HKG, November", None, None, None).unwrap();
        let waiting = NewArrival {
            booking: true,
            kind: Some("flight".into()),
            place: Some("Hong Kong".into()),
            date: Some("2026-11-02".into()),
            summary: "out".into(),
            trip_id: Some(draft.id),
            ..Default::default()
        };
        // One batch: a second `insert_arrivals` on the same mail is a retry
        // and replaces the pending rows of the first.
        let ids = store
            .insert_arrivals(a, m, &[
                waiting.clone(),
                // Not a mark: decided, not a booking, no date, or on no trip.
                NewArrival { date: Some("2027-01-01".into()), summary: "decided".into(), ..waiting.clone() },
                NewArrival { booking: false, date: Some("2027-02-02".into()), summary: "an ad".into(), ..waiting.clone() },
                NewArrival { date: None, summary: "no date".into(), ..waiting.clone() },
                NewArrival { trip_id: None, summary: "unplaced".into(), ..waiting.clone() },
            ])
            .unwrap();
        let (id, decided) = (ids[0], ids[1]);
        assert!(store.decide_arrival(decided, a, "ignored", None).unwrap());

        assert_eq!(
            store.pending_arrival_marks(a).unwrap(),
            vec![(draft.id, "2026-11-02".to_string(), Some("Hong Kong".to_string()))]
        );
        assert_eq!(store.pending_arrival_marks(b).unwrap(), vec![], "not theirs");
        assert!(store.decide_arrival(id, a, "added", None).unwrap());
        assert_eq!(store.pending_arrival_marks(a).unwrap(), vec![], "a decided booking waits for nothing");
    }

    #[test]
    fn a_draft_with_nothing_on_it_and_nothing_waiting_is_collected() {
        let (store, _dir) = test_store();
        // Pinned off UTC, the way `a_spent_token_is_stamped_on_the_clock_
        // that_expires_it` is: the grace compares `created_at` — written by
        // the column's own default — against a cutoff, and two clocks in
        // that comparison agree only on a UTC machine. A test that ran in
        // the host's zone would agree with the host and prove nothing.
        store
            .conn
            .lock()
            .unwrap()
            .execute("SET TimeZone = 'America/New_York'", [])
            .expect("the session's zone can be set");
        let a = store.account_for_telegram(1).unwrap();
        let b = store.account_for_telegram(2).unwrap();
        let m = mail(&store, a, "re_1");
        let empty = store.upsert_trip(a, "Empty draft", None, None, None).unwrap();
        let with_item = store.upsert_trip(a, "Has an item", None, None, None).unwrap();
        store.add_flight(with_item.id, "AMS", "HKG", "2026-11-02").unwrap();
        let kept = store.upsert_trip(a, "Kept", None, None, None).unwrap();
        store.keep_trip(a, "Kept").unwrap();
        let waited_on = store.upsert_trip(a, "Waited on", None, None, None).unwrap();
        let waiting = store
            .insert_arrival(a, m, &NewArrival {
                booking: true,
                date: Some("2026-11-02".into()),
                summary: "waiting".into(),
                trip_id: Some(waited_on.id),
                ..Default::default()
            })
            .unwrap();
        // A chat's own draft is the thread sweep's business, not this one.
        let chat = store.start_conversation(a, "direct").unwrap();
        let theirs = store.upsert_trip(a, "A chat draft", None, None, Some(chat)).unwrap();
        let strangers = store.upsert_trip(b, "Somebody else's", None, None, None).unwrap();
        // Every one of them is seconds old, and the grace spares a draft
        // that new — a mail being placed is making one right now. Backdated
        // past it, so what is under test is the rest of the rule.
        for trip in [empty.id, with_item.id, kept.id, waited_on.id, theirs.id, strangers.id] {
            store.age_trip(trip).unwrap();
        }

        assert_eq!(store.sweep_empty_drafts(a).unwrap(), 1);
        assert_eq!(store.trip_by_id(a, empty.id).unwrap(), None);
        for still_there in [with_item.id, kept.id, waited_on.id, theirs.id] {
            assert!(store.trip_by_id(a, still_there).unwrap().is_some(), "trip {still_there} went");
        }
        assert!(store.trip_by_id(b, strangers.id).unwrap().is_some(), "not theirs to collect");
        assert_eq!(store.sweep_empty_drafts(a).unwrap(), 0, "nothing left to collect");
        // Once that booking is decided — added to some other trip — its
        // draft has nothing left waiting on it, and the arrival stops
        // naming a trip that is gone rather than pointing at nothing.
        assert!(store.decide_arrival(waiting, a, "added", None).unwrap());
        assert_eq!(store.sweep_empty_drafts(a).unwrap(), 1);
        assert_eq!(store.trip_by_id(a, waited_on.id).unwrap(), None);
        assert_eq!(store.arrival_of(waiting, a).unwrap().unwrap().trip_id, None, "no dangling trip");
        // A draft made a moment ago is spared however empty it looks: a
        // mail being placed right now has one in exactly that state.
        let fresh = store.upsert_trip(a, "Just made", None, None, None).unwrap();
        assert_eq!(store.sweep_empty_drafts(a).unwrap(), 0, "the grace spares a new draft");
        store.age_trip(fresh.id).unwrap();
        assert_eq!(store.sweep_empty_drafts(a).unwrap(), 1, "and takes it once it is stale");
        // The account-less pass the hourly maintenance runs reaches the
        // drafts already sitting in somebody's list.
        assert_eq!(store.sweep_all_empty_drafts().unwrap(), 1);
        assert_eq!(store.trip_by_id(b, strangers.id).unwrap(), None);
    }

    #[test]
    fn emails_of_are_the_addresses_the_account_signed_in_with_oldest_first() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        assert_eq!(store.emails_of(a).unwrap(), Vec::<String>::new());
        let b = store.account_for_identity("email", "sasha@example.com").unwrap();
        assert_eq!(store.emails_of(b).unwrap(), ["sasha@example.com"]);
        // A second address on the same account: both are read, and the
        // first stays first so a forward keeps going where it went.
        store.link_identity(b, "email", "sasha@work.example").unwrap();
        assert_eq!(store.emails_of(b).unwrap(), ["sasha@example.com", "sasha@work.example"]);
        // A third, linked last and sorting first by address: only the
        // time can put it where it belongs, so the order is the order
        // they were linked in and not the alphabet's. The address the
        // account has always been forwarded at stays the destination.
        store.link_identity(b, "email", "a.later@example.com").unwrap();
        // Aged by hand rather than by the clock: `current_timestamp` is
        // the transaction's, and three links in one millisecond could
        // tie and leave the alphabet deciding after all.
        store
            .conn()
            .execute("UPDATE identities SET created_at = created_at + INTERVAL 1 HOUR WHERE external_id = ?", params!["a.later@example.com"])
            .unwrap();
        assert_eq!(store.emails_of(b).unwrap(), ["sasha@example.com", "sasha@work.example", "a.later@example.com"]);
    }

    #[test]
    fn a_fetched_mail_is_cut_at_the_cap_and_the_sender_that_came_with_it_is_kept() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = store.insert_mail(a, "re_1", "x", None, None, None, false, &[]).unwrap().unwrap();
        store.mail_fetched(m, Some("airline@example.com"), Some("héllo wörld"), Some("<p>hi</p>"), 5).unwrap();
        let row = &store.mail_to_work(1).unwrap()[0];
        assert_eq!((row.text.as_deref(), row.html.as_deref()), (Some("héllo"), Some("<p>hi")), "chars, not bytes");
        assert_eq!(row.from.as_str(), "airline@example.com", "the record's sender is what the row now says");
        let truncated: bool = store.conn().query_row("SELECT truncated FROM inbound_mail WHERE id = ?", params![m], |r| r.get(0)).unwrap();
        assert!(truncated);
        // Within the cap nothing is cut, and a body that was already marked
        // truncated on the way in stays so.
        let n = store.insert_mail(a, "re_2", "x", None, None, None, true, &[]).unwrap().unwrap();
        store.mail_fetched(n, Some("   "), Some("short"), None, 50).unwrap();
        let row = &store.mail_to_work(2).unwrap()[1];
        assert_eq!((row.text.as_deref(), row.html.as_deref()), (Some("short"), None));
        // A blank sender is not an answer: it leaves the one the webhook
        // stored alone rather than emptying the row.
        assert_eq!(row.from.as_str(), "x", "a blank from the record overwrote the webhook's sender");
        let truncated: bool = store.conn().query_row("SELECT truncated FROM inbound_mail WHERE id = ?", params![n], |r| r.get(0)).unwrap();
        assert!(truncated, "the webhook's verdict is not undone");
    }

    #[test]
    fn attachment_texts_and_bytes_are_read_per_mail() {
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let m = mail(&store, a, "re_1");
        let other = mail(&store, a, "re_2");
        store.insert_attachment(m, "ticket.pdf", "application/pdf", Some(b"%PDF"), Some("Row 12")).unwrap();
        store.insert_attachment(m, "note.txt", "text/plain", None, Some("see you")).unwrap();
        store.insert_attachment(m, "logo.png", "image/png", Some(&[1, 2]), None).unwrap();
        store.insert_attachment(other, "elsewhere.pdf", "application/pdf", Some(b"x"), Some("no")).unwrap();
        assert_eq!(
            store.attachment_texts_of(m).unwrap(),
            vec![
                ("ticket.pdf".to_string(), Some("Row 12".to_string())),
                ("note.txt".to_string(), Some("see you".to_string())),
                ("logo.png".to_string(), None),
            ]
        );
        assert_eq!(
            store.attachment_bytes_of(m).unwrap(),
            vec![("ticket.pdf".to_string(), b"%PDF".to_vec()), ("logo.png".to_string(), vec![1, 2])],
            "only rows that kept their bytes"
        );
    }

    #[test]
    fn a_flight_leg_can_be_booked_after_the_fact() {
        // `add_flight` knows nothing of bookings; a forwarded ticket does.
        let (store, _dir) = test_store();
        let a = store.account_for_telegram(1).unwrap();
        let trip = store.upsert_trip(a, "Lisbon", None, None, None).unwrap();
        let trip = store.add_flight(trip.id, "AMS", "LIS", "2026-10-12").unwrap();
        let leg = &trip.items[0];
        assert!(!leg.booked);
        store.book_item(leg.id, Some("PNR123"), Some(184.0), Some("EUR"), Some(7)).unwrap();
        let leg = &store.find_trip(a, "Lisbon").unwrap().unwrap().items[0];
        assert!(leg.booked);
        assert_eq!(leg.confirmation_code.as_deref(), Some("PNR123"));
        assert_eq!((leg.price, leg.currency.as_deref(), leg.arrival_id), (Some(184.0), Some("EUR"), Some(7)));
    }
}
