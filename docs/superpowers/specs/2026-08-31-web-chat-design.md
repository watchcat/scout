# The Web Chat — Design

## Purpose

Scout answers questions in Telegram. W2 gave the website identity; W3a gave
the agent a streaming protocol a browser can consume. This is the browser
client: a page where a member asks Scout something and watches the answer
arrive, in the same conversation their Telegram chat uses.

It depends on W3a and adds no protocol of its own.

## Scope

Send a message, stream the answer, show the recent history on load, and start
a fresh thread. No photo upload. The conversation is the `direct` scope the
`conversations` table has reserved for "the 1:1 chat and the web app, which
share" since phase one, so a question asked on a laptop continues the thread
from a phone.

**Members only.** `/chat` requires the seat `identity::standing` already
reports; anyone signed in without one is redirected to `/account`, which
already tells them where they stand. The daily cap is account-keyed, so the
web and Telegram draw on one allowance rather than two.

## Routes

| route | does |
|---|---|
| `GET /chat` | the page |
| `GET /chat/history` | recent turns of the `direct` conversation, JSON |
| `POST /chat/messages` | send a message; responds `text/event-stream` |
| `POST /chat/reset` | start a fresh thread |
| `GET /chat.js` | the client script |

They live in `crates/scout-web/src/routes/chat.rs`, on the authenticated
router beside `/account`, so the session cookie and the security headers
apply without new plumbing.

## The client is a file, not a script tag

`/chat.js` exists because the Content-Security-Policy says
`script-src 'self' https://telegram.org` with no `'unsafe-inline'`. A
`<script>` block in the page would be refused by the browser, silently as far
as the server is concerned. So the client is served from our own origin,
embedded with `include_str!` exactly as `index.html` already is, and reached
by a route exactly as `/icon.svg` already is.

`style-src` does allow `'unsafe-inline'`, so the page's CSS stays inline with
the markup, matching the existing pages.

The page template is `crates/scout-web/src/chat.html` rather than another
function in `pages.rs`, which is already 524 lines and would not be improved
by holding a whole application.

## What core has to gain

Two small additions, both read-only.

**A transcript.** History is stored as serialised `rig` messages. Nothing
returns them in a form a page can render — `last_messages_text` does the
extraction but flattens everything to one string for the continuation
classifier. So:

```rust
pub struct Turn { pub role: Role, pub text: String }
pub enum Role { You, Scout }
```

`Turn` lives in `scout-api`, for the same reason `ReplyTo` and `RunContext`
do: W4 puts it on a wire. `Role` is an enum rather than a string so a client
cannot invent a third role, and it is named for what a reader sees rather
than for `rig`'s vocabulary.

`session::transcript(core, conversation_id) -> Vec<Turn>` filters tool calls
and tool results the way `last_messages_text` already does, and drops turns
whose text is empty. It returns at most `HISTORY_CAP` (20) messages, because
that is what the store keeps — the page shows the same window the agent
itself has, which is the honest thing to show.

**A read-only conversation lookup.** `resolve_conversation` is wrong for page
load twice over: it takes the message text, because after a long gap it asks
the model whether the new message continues the old thread, and it *creates*
a conversation when none exists. Opening a page must create nothing. So the
history handler uses `Store::latest_conversation(account_id, "direct", ttl)`,
which reads and returns `Option<(id, aged_out)>`, and renders an empty
transcript when there is none.

## Rendering

Plain text: HTML-escaped, newlines preserved, bare URLs turned into links.

No markdown parser. The agent's preamble instructs it to "reply in plain text
without markdown formatting", so vendoring a parser to interpret output we
have told the model not to produce would be solving a problem we created. If
that instruction ever changes, this decision changes with it.

Escaping is not optional and not cosmetic: the answer contains text the model
wrote from web pages it fetched, which is untrusted content arriving in our
own page.

## Streaming

`fetch` with a `ReadableStream`, not `EventSource`, because `EventSource`
cannot issue a `POST` and the message has to go somewhere. The cost is losing
its automatic reconnection, which the failure handling below makes
unnecessary.

Each SSE event carries one JSON `AgentEvent`. The client holds an `answer`
string and applies each `TextUpdate` exactly as `render_events` does for
Telegram — `Append` pushes, `Replace` assigns. **A client that only appended
would keep reasoning on screen after the run retracted it**; that is the
whole reason W3a's protocol has a `Replace`, and the browser is the client it
was designed for.

`Tool` and `Thinking` events render as transient status above the answer,
which is the role they play in Telegram: a minute of silence while the agent
searches looks broken otherwise.

## Cross-site protection

The page embeds `session::csrf_for(account_id)`; `chat.js` returns it in an
`X-Scout-Csrf` request header, checked with `csrf_ok_for`. A header rather
than a body field so that `POST /chat/reset`, which has no body, is protected
the same way as `POST /chat/messages`, which has one. This is the token the account
page's forms already use, in the one form a `fetch` can carry.

The Origin refusal added in the W2 security review still applies to these
posts, and still refuses a request that names no origin at all.

## Failure handling

**The stream drops.** Laptops sleep and wifi flaps. The run continues on the
server and writes its answer to the conversation when it finishes, so nothing
is lost — only the live view. The page says exactly that and offers a reload
button. There is no replay to reconnect to, deliberately: W3a has no `seq`
numbers, and a replay would have to reproduce `Replace` precisely or it would
resurrect retracted reasoning.

**`Busy`.** Another client holds this conversation — the phone is mid-answer.
Shown inline in the page's own words. It is not an error and is not styled as
one.

**Over the daily cap.** `over_daily_cap` already returns the sentence to
show; the page shows it.

**The agent fails.** `agent_error_message` already turns a run failure into
something a person can act on. The page uses it rather than inventing a
second vocabulary for the same failures.

## Reaching it

`/account` gains a link to `/chat`. Without one the feature is unreachable,
so this is the entry point rather than a copy change.

The landing page still says "Scout lives in Telegram today — that is where
you ask it things", which becomes stale the day this ships. Left alone here
on purpose: it is public positioning rather than plumbing, and it deserves a
decision of its own rather than being edited in passing.

## Testing

- **The transcript drops tool traffic and keeps the exchange.** Build a
  history containing a tool call and its result, and assert the transcript is
  the user turn and the assistant turn only.
- **History creates nothing.** Call the history route for an account that has
  never spoken and assert the response is empty *and* that no conversation
  row was written — the failure this exists to prevent is a page visit
  minting rows.
- **A non-member is redirected.** `/chat` for a signed-in queued account
  returns a redirect to `/account`, not the page.
- **A signed-out visitor is redirected**, matching every other page on the
  authenticated router.
- **The stream applies a retraction.** Drive the client's accumulation logic
  over `Append("secret")`, `Replace("")`, `Append("The answer")` and assert
  the result is `"The answer"`. This is the browser-side half of W3a's
  security property, and it is the one test here that is about safety rather
  than behaviour.
- **A post without the CSRF header is refused**, and one from another origin
  is refused.
- **The page names `/chat.js` and carries no inline script**, so a CSP that
  would break the client in a browser breaks a test first.

## The risk worth naming before it bites

**Server-sent events through the ingress.** Traefik should not buffer
`text/event-stream`, but a reverse proxy is exactly where streaming quietly
dies: buffering turns a stream into one delivery at the end, which looks
perfect on localhost and broken in production. This gets verified against
`goodscout.fyi` with a real request, not against a local port, before the
feature is called done.

## Deferred, and why

- **Photo upload.** A shopping assistant should accept a photograph of a
  product, and `describe_photo` already exists. It needs multipart handling,
  a size limit, and storage decisions, which is its own spec.
- **A stop button.** Cancelling a run needs an endpoint and a way to abort
  the stream mid-flight. Worth having; not worth blocking a first chat on.
- **Marking where a thread restarts.** Because history loads read-only and
  `resolve_conversation` decides afterwards, a visitor can see history, ask a
  question, and have the gap-check start a fresh thread — leaving the agent
  without the context on screen. Telegram already behaves exactly this way,
  invisibly, so this is a pre-existing wrinkle the web makes visible rather
  than a new one. Showing a divider needs the send path to report that it
  started a new conversation, which is a protocol addition.
- **Changing the landing page.** See above.
