# One Way In — Design

## Purpose

A member arriving on a laptop takes four hops to reach the chat: the landing
page, whose only call to action is a Telegram deep link and whose sign-in is a
caption beneath it; then `/sign-in`; then `/account`; then `/chat`.

Every one of those pages is correct on its own. Together they are a corridor.
This makes the way in one step for someone who has been here before, and one
page for someone who has not.

## What changes, in one sentence each

- `/` learns whether the visitor has a session, and offers the chat instead of
  a door they have already walked through.
- `/` starts sending cache headers, because a page that varies must not be
  stored by anything shared.
- Signing in lands on `/chat` rather than `/account`.
- `/sign-in` with a session already in hand redirects rather than asking again.
- The gate offers a browser door beside the Telegram one.

## What the page learns, and what it refuses to

`/` is served by the public router, which `lib.rs` deliberately gives a cached
admission and no `Core`: *"giving it a `Core` it does not use would be an
invitation to query the database from the one path that exists to avoid doing
that."*

That boundary holds here, because the page needs no database. `session::verify`
is an HMAC comparison and an expiry check — no store, no `Core`, no I/O. W2
chose a signed cookie over a session table precisely so that knowing who
someone is costs nothing, and this is the first thing to collect on that.

So the public router gains **the session key and nothing else**, and the page
learns exactly one bit: whether a valid session exists.

The key arrives the same way `sign_in` already does — from the `AuthState`
that `router` is handed, or not. A deployment with no auth keys gets
`router(cache, None)`, has no session key, can have no sessions, and shows the
signed-out gate without a sign-in link, exactly as it does today. The page's
knowledge is `Option<Vec<u8>>`, and `None` is not a special case so much as
the honest answer.

It deliberately does **not** learn whether that account is a member, because
that needs `is_member`, which needs the database. The edge case this leaves —
a queued visitor pressing "Open Scout" — is already handled downstream:
`/chat` redirects a non-member to `/account`, which explains where they stand.
An existing redirect absorbs it, and the landing page stays ignorant.

## Cache headers are part of the feature, not a nicety

`/` sends **no cache directives at all** today. That is safe only because every
visitor receives identical bytes.

The moment the page varies by cookie, that becomes a way to serve one
visitor's page to another: a shared cache with no instructions may store the
signed-in version and hand it to a stranger, or store a stranger's and hand it
back to someone signed in. Neither leaks account data — the page holds no
name, no address, no history, and the session lives in the cookie rather than
the markup — but both show the wrong door, and the second is the kind of thing
that reads as a session bug and gets debugged for an afternoon.

So `/` gains `Cache-Control: private, no-store` and `Vary: Cookie`.

`Vary: Cookie` alone would be enough for a correct cache, and `no-store` is
belt and braces for the ones that are not. The test asserts both, because this
is the failure that no other test in the suite would notice.

`/icon.svg` keeps its `public, max-age=86400`: it does not vary, and it is the
one thing on the page worth caching.

## What each visitor sees

| visitor | the gate |
|---|---|
| signed in | **Open Scout** → `/chat` |
| signed out, a round has room | **Start in your browser** → `/sign-in`, with *Start on Telegram* beside it |
| signed out, no round has room | today's "Currently full", and the existing sign-in link |

The browser door is new only on the page. It has worked end to end since W2:
`identity::sign_in` claims a seat in the newest open round with room, and the
magic-link path calls it. A stranger can land, give an address, be seated and
start chatting without touching Telegram. The page has simply never said so.

The `Admission::Open { join_url: None }` branch — a bot that could not read its
own name at startup, and so has no deep link to offer — currently shows only
"Read the source". It gains the browser door, which does not depend on knowing
the bot's name. That branch exists for a state that persists until someone
restarts the process, so an hours-long stretch of offering nothing usable is
worth closing.

## Where signing in lands you

`signed_in` redirects to `/chat` rather than `/account`. Both the emailed link
and the Telegram widget go through it, so both land in the same place.

A queued account is redirected onward to `/account` by `/chat`'s own gate, the
same absorption the landing page relies on. The W2 rule that *queued is signed
in too* is unchanged: they still get a session, they simply land on the page
that explains their standing rather than one that would refuse them.

`/sign-in` gains a check at the top: a valid session redirects to `/chat`
rather than rendering a form. That is what collapses four hops to one for
someone who has been here before — they press "Open Scout", and if their
cookie has expired since, they get the form instead.

`/account` stops being a waypoint. It becomes what its own module doc already
calls it: where you attach the way in you do not have yet, and where you leave.

## Testing

- **A signed-in landing page is not cacheable.** Assert `Vary: Cookie` and a
  `private`/`no-store` `Cache-Control` on `/`. This is the one that matters:
  getting it wrong hands one visitor's page to another, and nothing else in
  the suite would notice.
- **The gate differs on the cookie.** The same request with and without a
  valid session produces the two different calls to action.
- **A forged or expired cookie is the signed-out gate**, not an error and not
  the signed-in one — the same "signed out and tampered with look alike" rule
  the rest of the site follows.
- **`/sign-in` with a session redirects to `/chat`** rather than rendering a
  form.
- **Signing in lands on `/chat`.** Both the emailed-link path and the widget
  path, since both go through `signed_in`.
- **A queued account pressing through still reaches `/account`**, which is the
  absorption both changes depend on. Without this test the whole "the page
  need not know about membership" argument is unverified.

## Deferred, and why

- **Showing membership on `/`.** It would let the page say "you are on the
  list" rather than sending a queued visitor on a redirect, and it costs a
  database read on the one page built to avoid them. The redirect is cheaper
  and already exists.
- **An email field in the hero.** Genuinely one page, but it would give the
  public router CSRF state and a `POST` handler, and the Telegram widget can
  never appear there without loading a third party's script onto the public
  page.
- **The landing page's prose.** It never claims Scout is Telegram-only —
  checked — so only the call to action needed changing.
