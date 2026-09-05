use crate::agent::HISTORY_CAP;
use crate::core::Core;
use rig::completion::Message as LlmMessage;

/// A conversation is set aside after this long without a word, and a quick
/// LLM check decides whether the next message resumes it.
pub const SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The account behind a Telegram user, created on first sight.
///
/// The one place the two id spaces meet. Everything below is keyed by
/// account id, and the argument type is now what stops a caller passing the
/// wrong number: it used to be an `i64` with a hopeful name.
pub async fn account_of(core: &Core, id: crate::ids::TelegramId) -> anyhow::Result<i64> {
    let store = core.deps.store.clone();
    crate::core::blocking(move || store.account_for_telegram(id.0)).await
}

/// Which conversation this message belongs to.
///
/// After a long gap the old thread is set aside and a quick LLM check
/// decides whether the new message continues it — the same rule the
/// in-memory session used, now applied to stored conversations so it
/// survives a restart.
pub async fn resolve_conversation(
    core: &Core,
    account_id: i64,
    scope: &str,
    text: &str,
) -> anyhow::Result<i64> {
    let ttl = SESSION_TTL.as_secs() as i64;
    let store = core.deps.store.clone();
    let (scope_owned, latest) = {
        let scope_owned = scope.to_string();
        let s = scope_owned.clone();
        let store = store.clone();
        (
            scope_owned,
            crate::core::blocking(move || store.latest_conversation(account_id, &s, ttl)).await?,
        )
    };

    let Some((id, aged_out)) = latest else {
        let store = core.deps.store.clone();
        return crate::core::blocking(move || store.start_conversation(account_id, &scope_owned)).await;
    };
    if !aged_out {
        return Ok(id);
    }

    let excerpt = {
        let store = core.deps.store.clone();
        let history = crate::core::blocking(move || load_history(&store, id, HISTORY_CAP)).await?;
        last_messages_text(&history, 6)
    };
    if excerpt.trim().is_empty() {
        let store = core.deps.store.clone();
        return crate::core::blocking(move || store.start_conversation(account_id, &scope_owned)).await;
    }
    match crate::agent::continues_previous(&core.deps.llm, &excerpt, text).await {
        Ok(true) => {
            tracing::info!(account_id, id, "session expired but topic continues; keeping context");
            let store = core.deps.store.clone();
            if !crate::core::blocking(move || store.touch_conversation(id)).await? {
                // The continuation check took long enough that the thread's
                // own TTL ran out and it was deleted underneath it — the
                // window `expire_threads`'s ordering closes on the web path
                // stays open here, since this check and the bump are two
                // separate round trips with an LLM call between them. `id`
                // no longer names anything; a fresh thread is the only
                // honest answer left.
                tracing::info!(account_id, id, "continuation confirmed but the thread was gone by the time it was bumped; starting fresh");
                let store = core.deps.store.clone();
                return crate::core::blocking(move || store.start_conversation(account_id, &scope_owned)).await;
            }
            Ok(id)
        }
        Ok(false) => {
            tracing::info!(account_id, "session expired; starting fresh");
            let store = core.deps.store.clone();
            crate::core::blocking(move || store.start_conversation(account_id, &scope_owned)).await
        }
        Err(e) => {
            tracing::warn!(error = %e, account_id, "continuation check failed; starting fresh");
            let store = core.deps.store.clone();
            crate::core::blocking(move || store.start_conversation(account_id, &scope_owned)).await
        }
    }
}

/// `Some(reply)` when this person has spent today's allowance.
///
/// Founders are exempt: they are the people paying for the bot.
///
/// A failed count lets the message through. The cap is a cost guard, not an
/// access control — the gate above it already decided this person is
/// allowed here — and a database blip should not silence everyone at once.
pub async fn over_daily_cap(core: &Core, account_id: i64) -> Option<String> {
    match core.founder_account(account_id).await {
        Ok(true) => return None,
        Ok(false) => {}
        // Same reasoning as a failed count below: the cap is a cost guard,
        // not access control, and a database blip must not silence everyone.
        Err(e) => {
            tracing::warn!(error = %e, account_id, "founder check failed; letting it through");
            return None;
        }
    }
    let cap = core.cfg.invite_daily_requests;
    let store = core.deps.store.clone();
    let used = match crate::core::blocking(move || store.requests_today(account_id)).await {
        Ok(used) => used,
        Err(e) => {
            tracing::warn!(error = %e, account_id, "daily cap check failed; letting it through");
            return None;
        }
    };
    (used >= cap).then(|| {
        tracing::info!(account_id, used, cap, "daily cap reached");
        format!("You've used today's {cap} requests. It resets at midnight UTC.")
    })
}

/// How many stored rows a transcript is built from.
///
/// Twenty times the model's window, because the two caps answer different
/// questions: the model's is a context budget, and this one only has to be
/// larger than any thread can become before the 48-hour sweep takes it. A
/// long afternoon of price research runs to a few hundred rows, so in
/// practice a page shows the thread from its first word — which is the
/// point. It is a ceiling rather than a promise, so that one pathological
/// conversation cannot make a page render megabytes.
///
/// Rows, not exchanges: at the ~28 rows a tool-heavy run stores — the
/// question, a dozen-odd tool call/result pairs, the answer — this is
/// roughly fourteen exchanges. `turns_of` keeps only the text turns, so
/// what the page shows for a very long thread is its last dozen-or-so
/// exchanges rather than four hundred bubbles.
pub(crate) const TRANSCRIPT_CAP: usize = 400;

/// Rewrites a conversation's stored messages to match `history`.
///
/// For a caller that means "this conversation is exactly these messages".
/// A run does not: it appends, see `append_history`.
pub(crate) fn save_history(store: &crate::store::Store, conversation_id: i64, history: &[LlmMessage]) -> anyhow::Result<()> {
    store.replace_messages(conversation_id, &bodies_of(history)?)
}

/// Adds messages to the end of a conversation's log.
///
/// How a run writes what it produced. The stored log is not the model's
/// window and does not have to agree with it: the window is cut on the way
/// out, by `load_history`.
pub(crate) fn append_history(store: &crate::store::Store, conversation_id: i64, messages: &[LlmMessage]) -> anyhow::Result<()> {
    store.append_messages(conversation_id, &bodies_of(messages)?)
}

fn bodies_of(messages: &[LlmMessage]) -> anyhow::Result<Vec<String>> {
    Ok(messages
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?)
}

/// The last `cap` stored messages, oldest first, exactly as they were
/// written. A row that no longer deserializes — because rig changed shape
/// under us — is dropped rather than fatal: losing some context is
/// survivable, refusing to answer at all is not.
///
/// What a *reader* wants. Nothing here is shaped for a provider, so a
/// window cut at `cap` may well open mid-tool-call — which is fine for a
/// transcript, where tool traffic is not rendered at all, and fatal for the
/// model. `load_history` is the one that makes it safe to send.
pub(crate) fn load_history_raw(store: &crate::store::Store, conversation_id: i64, cap: usize) -> anyhow::Result<Vec<LlmMessage>> {
    let bodies = store.conversation_messages(conversation_id, cap)?;
    let mut out = Vec::with_capacity(bodies.len());
    for body in bodies {
        match serde_json::from_str::<LlmMessage>(&body) {
            Ok(m) => out.push(m),
            Err(e) => tracing::warn!(error = %e, "dropping an unreadable stored message"),
        }
    }
    Ok(out)
}

/// The last `cap` messages, shaped so a provider will accept them.
///
/// The trim lives here rather than at the call site because this *is* the
/// question "what does the model get": a window cut by row count can land
/// between a tool call and its result, and a provider rejects that outright.
/// It used to run just before saving instead, which is what made the stored
/// thread no longer than the model's window — the trim drops from the
/// front, and what it dropped was gone.
///
/// The read is wide and the trim is narrow, and that order is the whole
/// point. `trim_history` takes the newest `cap` and, when they are tool
/// traffic all the way up with no plain-user head to open on, falls back to
/// keeping the prose — which is only a good answer if it can see prose. A
/// run stores the question, a dozen-odd tool call/result pairs and the
/// answer, so a read of `cap + 1` rows on a thread like that hands the
/// fallback nothing but tool traffic and one reply: the model was given its
/// own last answer and never the question that prompted it. Reading
/// `TRANSCRIPT_CAP` puts the earlier turns back within the fallback's
/// reach. It also closes a smaller hole — a row that no longer
/// deserializes used to make the raw read return exactly `cap`, and the
/// trim skips a history no longer than the cap.
pub(crate) fn load_history(store: &crate::store::Store, conversation_id: i64, cap: usize) -> anyhow::Result<Vec<LlmMessage>> {
    let mut history = load_history_raw(store, conversation_id, TRANSCRIPT_CAP)?;
    crate::run::trim_history(&mut history, cap);
    Ok(history)
}

/// The sayable text of a user turn, or `None` when it holds only tool
/// results — the rule shared by `last_messages_text` and `transcript_of` so
/// what counts as "something a person said" cannot drift between the two.
fn text_of_user(content: &rig::OneOrMany<rig::message::UserContent>) -> Option<String> {
    content
        .iter()
        .filter_map(|c| match c {
            rig::message::UserContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .reduce(|a, b| format!("{a}\n{b}"))
}

/// The sayable text of an assistant turn, or `None` when it holds only tool
/// calls — see `text_of_user`.
fn text_of_assistant(content: &rig::OneOrMany<rig::message::AssistantContent>) -> Option<String> {
    content
        .iter()
        .filter_map(|c| match c {
            rig::message::AssistantContent::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .reduce(|a, b| format!("{a}\n{b}"))
}

/// The last `n` plain-text messages of a history, rendered as
/// "user:/assistant:" lines for the continuation classifier.
pub(crate) fn last_messages_text(history: &[LlmMessage], n: usize) -> String {
    let mut lines: Vec<String> = history
        .iter()
        .rev()
        .filter_map(|m| match m {
            LlmMessage::User { content } => text_of_user(content).map(|t| format!("user: {t}")),
            LlmMessage::Assistant { content, .. } => text_of_assistant(content).map(|t| format!("assistant: {t}")),
            _ => None,
        })
        .take(n)
        .collect();
    lines.reverse();
    lines.join("\n")
}

/// The conversation a page should show, without starting one.
///
/// `resolve_conversation` is wrong here twice over: it needs the message
/// text, because after a long gap it asks the model whether the new message
/// continues the old thread, and it creates a conversation when there is
/// none. Opening a page must create nothing.
pub(crate) fn latest_direct(store: &crate::store::Store, account_id: i64) -> anyhow::Result<Option<i64>> {
    let ttl = SESSION_TTL.as_secs() as i64;
    Ok(store.latest_conversation(account_id, "direct", ttl)?.map(|(id, _aged)| id))
}

/// What was said, oldest first, with the tool traffic left out.
///
/// The same line `last_messages_text` draws, kept structured rather than
/// flattened: that one exists to feed a classifier a blob, this one exists
/// to be rendered.
///
/// Reads far more than the model's window, and reads it raw. A page is
/// showing a person their own conversation, so the only reason to leave
/// anything out is size; the agent's trim exists to keep a provider happy
/// and has nothing to say here. A transcript cut at `TRANSCRIPT_CAP` may
/// therefore open on an answer rather than a question, which reads as a
/// conversation joined late — correct, and far better than one that starts
/// at the last exchange because everything above it was deleted.
pub(crate) fn transcript_of(store: &crate::store::Store, conversation_id: i64) -> anyhow::Result<Vec<scout_api::Turn>> {
    Ok(turns_of(&load_history_raw(store, conversation_id, TRANSCRIPT_CAP)?))
}

/// Whether an assistant message is an answer rather than a step of one.
///
/// A message that carries a tool call is the model working. Whatever text
/// sits beside the call is what it says on its way to an answer — "let me
/// check the next shop" — and the finished reply is the turn with no call
/// left to make, because had there been one the agent would have kept
/// going. So the presence of a call, not the absence of text, is the test.
fn is_answer(content: &rig::OneOrMany<rig::message::AssistantContent>) -> bool {
    !content
        .iter()
        .any(|c| matches!(c, rig::message::AssistantContent::ToolCall(_)))
}

/// Whether the answer at `i` was replaced by the one after it.
///
/// The repair paths hand the model a correction and keep its second attempt,
/// but `rig` appends rather than replaces — so the history holds both. Left
/// in, a transcript shows the superseded answer *and* its replacement: the
/// dead links the repair existed to remove, or the raw `<tool_call>` markup
/// the model wrote as prose, sitting above the real answer forever.
///
/// A thumbs-up follow-up also opens with a system note and must NOT trigger
/// this — the answer above that one still stands. `CORRECTION_NOTE` is what
/// separates the two.
fn superseded(history: &[LlmMessage], i: usize) -> bool {
    let Some(LlmMessage::User { content }) = history.get(i + 1) else {
        return false;
    };
    text_of_user(content).is_some_and(|t| t.starts_with(crate::text::CORRECTION_NOTE))
}

/// The turns a history should be rendered as, oldest first.
///
/// Split out of `transcript_of` because the store half needs a database and
/// this half is the part that can be wrong: which messages count as things
/// that were said, and what is stripped from them before they are shown.
fn turns_of(history: &[LlmMessage]) -> Vec<scout_api::Turn> {
    history
        .iter()
        .enumerate()
        .filter_map(|(i, m)| match m {
            // Cut, because `rig`'s `Chat::chat` appends the prompt it is
            // given to the history — so the dead-link repair's own
            // instruction was saved as a user message and rendered back to
            // the person as something they had said.
            LlmMessage::User { content } => text_of_user(content).map(|text| scout_api::Turn {
                role: scout_api::Role::You,
                text: crate::text::said_by_person(&text).to_string(),
            }),
            // Stripped, because what is *stored* is the model's raw message:
            // `run_agent` saves `res.messages()`, tags and all, and only the
            // reply it hands the channel goes through `strip_thinking`.
            // Telegram never renders history so this never showed; the web
            // page does, and rendering it raw put a whole chain of thought
            // on screen — permanently, on every load, rather than for the
            // moment a stream does.
            LlmMessage::Assistant { content, .. }
                if is_answer(content) && !superseded(history, i) =>
            {
                text_of_assistant(content).map(|text| scout_api::Turn {
                    role: scout_api::Role::Scout,
                    text: crate::text::strip_thinking(&text),
                })
            }
            _ => None,
        })
        .filter(|t| !t.text.trim().is_empty())
        .collect()
}

/// The current thread's transcript, or empty when there is no thread yet.
pub async fn transcript(core: &Core, account_id: i64) -> anyhow::Result<Vec<scout_api::Turn>> {
    let store = core.store();
    crate::core::blocking(move || {
        let Some(id) = latest_direct(&store, account_id)? else {
            return Ok(Vec::new());
        };
        transcript_of(&store, id)
    })
    .await
}

/// The current thread's id and transcript, or `None` when there is no
/// thread yet.
///
/// `transcript` answers the page, which only needs the turns. Mirroring
/// needs the id as well, because a turn's key is scoped to its conversation
/// — the same question asked again in a new thread is a new turn.
pub async fn current_thread(
    core: &Core,
    account_id: i64,
) -> anyhow::Result<Option<(i64, Vec<scout_api::Turn>)>> {
    let store = core.store();
    crate::core::blocking(move || {
        let Some(id) = latest_direct(&store, account_id)? else {
            return Ok(None);
        };
        Ok(Some((id, transcript_of(&store, id)?)))
    })
    .await
}

/// Which thread is current, without reading what is in it.
///
/// `current_thread` answers the same question, but it loads and shapes the
/// whole transcript to do it — so a caller that only wants to compare one
/// id against another pays for every turn of a long conversation to throw
/// them all away. The switch note is exactly that caller.
pub async fn current_thread_id(core: &Core, account_id: i64) -> anyhow::Result<Option<i64>> {
    let store = core.store();
    crate::core::blocking(move || latest_direct(&store, account_id)).await
}

/// The longest a person may make a title.
const RENAME_CHARS: usize = 80;

/// The account's threads for the sidebar: pinned first, then by last use,
/// with `current` on the one `latest_direct` returns.
///
/// The two reads are separate statements, not one transaction, so a thread
/// can vanish between them — a caller must tolerate a list with no `current`
/// row rather than assume exactly one is always marked.
pub async fn threads(core: &Core, account_id: i64) -> anyhow::Result<Vec<scout_api::Thread>> {
    let store = core.store();
    crate::core::blocking(move || {
        let current = latest_direct(&store, account_id)?;
        Ok(store
            .threads_of(account_id)?
            .into_iter()
            .map(|row| scout_api::Thread {
                current: Some(row.id) == current,
                id: row.id,
                title: row.title,
                pinned: row.pinned,
                updated_at: row.updated_at,
            })
            .collect())
    })
    .await
}

/// One thread's name, for a caller that wants the name and not the list.
pub async fn thread_title(
    core: &Core,
    account_id: i64,
    conversation_id: i64,
) -> anyhow::Result<Option<String>> {
    let store = core.store();
    crate::core::blocking(move || store.thread_title(account_id, conversation_id)).await
}

/// Switches to a thread: bumps it to current and returns its transcript.
/// `None` when the account has no such thread.
pub async fn open_thread(
    core: &Core,
    account_id: i64,
    conversation_id: i64,
) -> anyhow::Result<Option<Vec<scout_api::Turn>>> {
    let store = core.store();
    crate::core::blocking(move || {
        if !store.open_conversation(account_id, conversation_id)? {
            return Ok(None);
        }
        Ok(Some(transcript_of(&store, conversation_id)?))
    })
    .await
}

/// What a rename did. A blank name is refused as an outcome rather than an
/// error, so the caller can tell "not a name" from "the database failed"
/// without parsing a message — and so the rule lives here, once, rather
/// than in every route that calls this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renamed {
    Done,
    NotFound,
    Blank,
}

/// A name the person chose. Trimmed, stripped of layout characters, cut at
/// `RENAME_CHARS`, and the cut itself is silent — no ellipsis, because a
/// name a person typed should not grow punctuation it never had. A caller
/// that shows the name back to the person after a cut must re-read it
/// rather than echo what it sent.
///
/// The strip happens before the cut so the budget is spent on characters
/// that are actually shown, and it happens at all because a name that
/// arrives over HTTP is no more trustworthy than one a model wrote — the
/// same bidi override that would flip a suggested title flips a typed one.
pub async fn rename(core: &Core, account_id: i64, conversation_id: i64, title: &str) -> anyhow::Result<Renamed> {
    let title: String = strip_layout_chars(title.trim())
        .chars()
        .take(RENAME_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string();
    if title.is_empty() {
        return Ok(Renamed::Blank);
    }
    let store = core.store();
    let done = crate::core::blocking(move || store.set_thread_title(account_id, conversation_id, &title)).await?;
    Ok(if done { Renamed::Done } else { Renamed::NotFound })
}

/// Permanent: exempt from expiry. Nothing else.
pub async fn set_pinned(core: &Core, account_id: i64, conversation_id: i64, pinned: bool) -> anyhow::Result<bool> {
    let store = core.store();
    crate::core::blocking(move || store.set_thread_pinned(account_id, conversation_id, pinned)).await
}

/// See `Store::delete_conversation` for what this deliberately leaves
/// behind: a mirror row already queued in the outbox is not swept.
pub async fn delete_thread(core: &Core, account_id: i64, conversation_id: i64) -> anyhow::Result<bool> {
    let store = core.store();
    crate::core::blocking(move || store.delete_conversation(account_id, conversation_id)).await
}

/// Drops a leading list marker (`1. `, `1) `, `- `, `* `, `• `), which is
/// how a model that was asked for one title answers with a list of one.
fn strip_list_marker(line: &str) -> &str {
    for bullet in ["- ", "* ", "• "] {
        if let Some(rest) = line.strip_prefix(bullet) {
            return rest.trim_start();
        }
    }
    // At most two digits: a list a model writes never reaches a hundred
    // items, and a longer run is the title itself — "2024. Year in review"
    // is a name, not the two-thousand-and-twenty-fourth bullet.
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=2).contains(&digits) {
        let rest = &line[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return rest.trim_start();
        }
    }
    line
}

/// Drops a leading `Title:`, in any case and with markdown bold around it,
/// which the preamble asks for but a model volunteers anyway.
fn strip_title_label(line: &str) -> &str {
    let rest = line.strip_prefix("**").unwrap_or(line);
    let Some(after) = rest.to_ascii_lowercase().strip_prefix("title:").map(str::len) else {
        return line;
    };
    let rest = &rest[rest.len() - after..];
    rest.strip_prefix("**").unwrap_or(rest).trim()
}

/// Drops the markdown a model dresses a title in when it forgets it was
/// asked for plain text: a leading `#` run from a heading, and a symmetric
/// emphasis wrap. Symmetric on purpose — `*` alone at the front is a bullet
/// `strip_list_marker` already took, and an unmatched one is punctuation
/// inside the name, not decoration around it.
fn strip_markdown(line: &str) -> &str {
    let line = line.trim_start_matches('#').trim_start();
    for wrap in ["**", "__", "*", "_"] {
        if let Some(inner) = line.strip_prefix(wrap).and_then(|l| l.strip_suffix(wrap)) {
            return inner.trim();
        }
    }
    line
}

/// What the model said, made fit for a sidebar: the answer line, a leading
/// list marker, a leading "Title:", markdown decoration and wrapping quotes
/// dropped, trailing punctuation dropped, layout characters filtered out,
/// cut at `RENAME_CHARS` with no trailing space left by the cut. `None`
/// when nothing is left.
///
/// The answer line is the first non-empty one, unless — once its marker and
/// label are off — it is empty or ends in a colon and another line follows.
/// Then it was the preamble to the answer, not the answer.
///
/// Quotes are stripped on both sides of the punctuation pass, because the
/// two orders each leave one real answer intact: `'A title'.` keeps its
/// closing quote if quotes go first, and `"A title."` keeps its full stop
/// if punctuation does.
pub fn clean_title(raw: &str) -> Option<String> {
    let quote = |c: char| matches!(c, '"' | '\'' | '“' | '”' | '‘' | '’');
    let text = crate::text::strip_thinking(raw);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    // "Here is a title for your conversation:" is the announcement, not the
    // answer — a line that ends in a colon with another line behind it is
    // the model clearing its throat. The markers come off first, because a
    // line that is nothing but `**Title:**` only looks like an answer until
    // the label is gone.
    let first = strip_title_label(strip_list_marker(lines.next()?));
    let line = if first.is_empty() || first.ends_with(':') {
        lines
            .next()
            .map(|l| strip_title_label(strip_list_marker(l)))
            .unwrap_or(first)
    } else {
        first
    };
    let line = strip_markdown(line);
    let line = line.trim_matches(quote);
    let line = line.trim_end_matches(['.', '!', '?', ':']);
    let line = line.trim_matches(quote);
    let cleaned = strip_layout_chars(line.trim())
        .chars()
        .take(RENAME_CHARS)
        .collect::<String>()
        .trim_end()
        .to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// A character a sidebar row must never carry: a control character, a line
/// or paragraph separator, one of the bidi overrides and isolates, which
/// can make a title render as something other than what it says — and,
/// unclosed, drag the row after it along too — or one of the invisibles
/// (zero-width space and joiners, the RTL/LTR marks, the byte-order mark,
/// the soft hyphen) that leave a name looking like one thing and matching
/// another.
fn is_layout_char(c: char) -> bool {
    c.is_control()
        || matches!(c,
            '\u{00AD}'
            | '\u{200B}'..='\u{200F}'
            | '\u{2028}' | '\u{2029}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}')
}

/// The one rule about what a title may contain, for all three ways a thread
/// gets named: the model's suggestion, the name a person types, and the cut
/// of a first message.
///
/// A layout character that is also whitespace — a newline, a tab, a line
/// separator — becomes a space rather than nothing. Dropping it would weld
/// the words on either side together, which is a worse title than the one
/// the character was hiding in.
fn strip_layout_chars(s: &str) -> String {
    s.chars()
        .filter_map(|c| match (is_layout_char(c), c.is_whitespace()) {
            (false, _) => Some(c),
            (true, true) => Some(' '),
            (true, false) => None,
        })
        .collect()
}

/// Asks the model for a name and stores it. `None` when the thread is not
/// the account's, has nothing in it to name, or went away while the model
/// was thinking. An unusable answer is an error the caller reports; the old
/// title stays.
///
/// The thread is read, not opened: naming an old sidebar row must not make
/// it the thread Telegram continues, which is what `open_thread` would do.
pub async fn suggest_title(core: &Core, account_id: i64, conversation_id: i64) -> anyhow::Result<Option<String>> {
    let store = core.store();
    let Some(turns) = crate::core::blocking(move || {
        if !store.owns_thread(account_id, conversation_id)? {
            return Ok(None);
        }
        Ok(Some(transcript_of(&store, conversation_id)?))
    })
    .await?
    else {
        return Ok(None);
    };
    if turns.is_empty() {
        return Ok(None);
    }
    let text: String = turns
        .iter()
        .take(6)
        .map(|t| {
            let who = match t.role {
                scout_api::Role::You => "user",
                scout_api::Role::Scout => "scout",
            };
            format!("{who}: {}", t.text.chars().take(400).collect::<String>())
        })
        .collect::<Vec<_>>()
        .join("\n");
    let raw = crate::agent::title_for(&core.deps.llm, &text).await?;
    let Some(title) = clean_title(&raw) else {
        anyhow::bail!("the model gave no usable title");
    };
    let store = core.store();
    let written = title.clone();
    // The thread can be deleted while the model is thinking; then nothing
    // was named, and saying otherwise would put a title on a gone row.
    let stored = crate::core::blocking(move || store.set_thread_title(account_id, conversation_id, &written)).await?;
    Ok(stored.then_some(title))
}

/// Starts a fresh conversation, discarding whatever thread was live.
///
/// History lives in the store now, so clearing an in-memory slot would clear
/// nothing — /reset has to mean a new thread or it means nothing.
pub async fn reset(core: &Core, account_id: i64, scope: &str) -> anyhow::Result<i64> {
    let store = core.store();
    let scope = scope.to_string();
    crate::core::blocking(move || store.start_conversation(account_id, &scope)).await
}

/// Records an exchange as though it had already happened, without spending
/// a model call.
///
/// `Store` and `save_history` stay private to this crate — that boundary is
/// deliberate, see the module doc at the crate root — so this is the one
/// door a caller outside the crate has to them. It exists for tests that
/// need `transcript` to return something real: the only other public writer
/// of history is `run::run_agent`, which means talking to a live model.
///
/// Named for what it is so that nobody reaches for it in earnest: a channel
/// writing an exchange nobody had is a channel inventing history.
#[doc(hidden)]
pub async fn seed_exchange_for_tests(
    core: &Core,
    account_id: i64,
    scope: &str,
    you_said: &str,
    scout_said: &str,
) -> anyhow::Result<i64> {
    let store = core.store();
    let scope = scope.to_string();
    let (you_said, scout_said) = (you_said.to_string(), scout_said.to_string());
    crate::core::blocking(move || {
        let id = store.start_conversation(account_id, &scope)?;
        save_history(&store, id, &[LlmMessage::user(you_said), LlmMessage::assistant(scout_said)])?;
        Ok(id)
    })
    .await
}

/// How many characters of a first message become the thread's name.
const TITLE_CHARS: usize = 40;

/// The automatic name: the first message, whitespace collapsed to single
/// spaces, cut at `TITLE_CHARS` chars, not grapheme clusters, with an
/// ellipsis when cut. A flag emoji built from two code points can be split
/// at the cut; the cut is on a char boundary, so the result is still valid
/// text, and a title is a label, not a rendering.
///
/// Layout characters go first, before the collapse, so the space a stripped
/// newline leaves behind is collapsed with the rest rather than surviving
/// as a double space.
pub fn first_message_title(text: &str) -> String {
    let text = strip_layout_chars(text);
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = one_line.chars();
    let head: String = chars.by_ref().take(TITLE_CHARS).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// Names a thread after its first answer, unless it already has a name.
///
/// Called by `run_agent` after a successful save, only when the run carries
/// the person's own words (`RunContext::title_source`). The prompt is never
/// the source: on Telegram the prompt carries a system note the person
/// never wrote, and a title cut from it would read as one. Failure is
/// logged, not returned: a missing title is not worth failing an answer
/// that is already written.
pub async fn title_if_missing(core: &Core, conversation_id: i64, source: &str) {
    let title = first_message_title(source);
    if title.is_empty() {
        return;
    }
    let store = core.store();
    if let Err(e) = crate::core::blocking(move || store.set_thread_title_if_missing(conversation_id, &title)).await {
        tracing::warn!(error = %e, conversation_id, "could not name the thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn narrating_tool_call(text: &str, tool: &str) -> LlmMessage {
        // What a multi-turn run actually stores for a step: the model says
        // what it is about to do, and calls the tool in the same message.
        LlmMessage::Assistant {
            id: None,
            content: rig::OneOrMany::many([
                rig::message::AssistantContent::text(text),
                rig::message::AssistantContent::ToolCall(rig::message::ToolCall::new(
                    "call-1".to_string(),
                    rig::message::ToolFunction {
                        name: tool.to_string(),
                        arguments: serde_json::json!({}),
                    },
                )),
            ])
            .unwrap(),
        }
    }

    #[test]
    fn an_instruction_scout_gave_itself_is_not_shown_as_the_persons_words() {
        // `rig`'s `Chat::chat` appends the prompt to the history, so the
        // dead-link repair — which runs on exactly the price answers this
        // bot exists to write — left its own instruction in the transcript
        // wearing the reader's name.
        let history = vec![
            LlmMessage::user("find me cheapest gillette\n\n[system note] This is a cheapest-price request."),
            LlmMessage::user(crate::links::repair_prompt(&["https://bol.com/404".to_string()])),
            LlmMessage::assistant("EUR 40.44 delivered"),
        ];
        let turns = turns_of(&history);
        assert_eq!(turns.len(), 2, "got: {:?}", turns.iter().map(|t| &t.text).collect::<Vec<_>>());
        assert_eq!(turns[0].text, "find me cheapest gillette");
        assert_eq!(turns[1].text, "EUR 40.44 delivered");
    }

    #[test]
    fn the_narration_a_run_writes_between_tool_calls_is_not_a_turn() {
        // Measured on a real transcript: a price comparison rendered as a
        // page of "Let me check Kruidvat, ShaveSavings and bol.com", then
        // "The shavesavings 8-pack is out of stock", then the answer — one
        // bubble per step of the run, permanently, on every reload.
        //
        // A message carrying a tool call is the model working. The text
        // beside the call is what it says on its way to an answer, not the
        // answer: the finished reply is the turn with no call left to make,
        // because had there been one the agent would have kept going.
        let history = vec![
            LlmMessage::user("find me cheapest gillette cartridges"),
            narrating_tool_call("Let me check Kruidvat, ShaveSavings and bol.com.", "search"),
            narrating_tool_call("The 8-pack is out of stock. Let me check delivery.", "fetch_page"),
            LlmMessage::assistant("EUR 40.44 delivered — 16-pack, shavesavings.com"),
        ];
        let turns = turns_of(&history);
        assert_eq!(turns.len(), 2, "got: {:?}", turns.iter().map(|t| &t.text).collect::<Vec<_>>());
        assert_eq!(turns[0].text, "find me cheapest gillette cartridges");
        assert_eq!(turns[1].text, "EUR 40.44 delivered — 16-pack, shavesavings.com");
    }

    #[test]
    fn an_answer_that_was_corrected_is_not_shown_beside_its_replacement() {
        // `rig` appends the repair exchange rather than replacing it, so the
        // history holds the answer with the dead links *and* the corrected
        // one. Rendering both leaves the links this bot spent a request
        // removing sitting above the answer, permanently — and mirroring
        // both sends them to the reader's phone.
        let history = vec![
            LlmMessage::user("cheapest beans"),
            LlmMessage::assistant("EUR 3 at https://dead.example/404"),
            LlmMessage::user(crate::links::repair_prompt(&["https://dead.example/404".to_string()])),
            LlmMessage::assistant("EUR 3 at bol.com"),
        ];
        let turns = turns_of(&history);
        assert_eq!(turns.len(), 2, "got: {:?}", turns.iter().map(|t| &t.text).collect::<Vec<_>>());
        assert_eq!(turns[1].text, "EUR 3 at bol.com");
    }

    #[test]
    fn a_thumbs_up_does_not_supersede_the_answer_it_praised() {
        // The reaction follow-up also opens with a system note, but the
        // answer above it stands — dropping it would delete a real reply
        // from the reader's transcript for having been liked.
        let history = vec![
            LlmMessage::assistant("EUR 3 at bol.com"),
            LlmMessage::user("[system note] The user reacted with a thumbs-up to this earlier reply."),
            LlmMessage::assistant("Want me to save that one?"),
        ];
        let turns = turns_of(&history);
        assert_eq!(turns.len(), 2, "got: {:?}", turns.iter().map(|t| &t.text).collect::<Vec<_>>());
        assert_eq!(turns[0].text, "EUR 3 at bol.com");
    }

    #[test]
    fn a_rendered_turn_still_has_its_reasoning_stripped() {
        // The other half of the same guarantee: what is stored is the
        // model's raw message, tags and all, so the answer turn is stripped
        // at render time — including the namespaced spelling.
        let history = vec![LlmMessage::assistant("my reasoning</mm:think>The answer")];
        assert_eq!(turns_of(&history)[0].text, "The answer");
    }

    #[test]
    fn last_messages_text_renders_roles_and_takes_tail() {
                let history = vec![
            LlmMessage::user("oldest question"),
            LlmMessage::assistant("oldest answer"),
            LlmMessage::user("find me a bike"),
            LlmMessage::assistant("here are 3 bikes"),
        ];
        let excerpt = last_messages_text(&history, 2);
        assert_eq!(excerpt, "user: find me a bike\nassistant: here are 3 bikes");
    }

    #[test]
    fn history_survives_being_dropped_and_reloaded() {
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        let original = vec![LlmMessage::user("cheapest beans"), LlmMessage::assistant("here")];
        save_history(&s, c, &original).unwrap();
        let loaded = load_history(&s, c, HISTORY_CAP).unwrap();

        // Not compared struct-for-struct: rig leaves `additional_params`
        // as None when a message is built in code but deserializes it to
        // Some({}), so a round trip is never byte-identical. What has to
        // hold is that the words and their roles survive, and that a second
        // trip changes nothing further — otherwise history would drift a
        // little every time it was reloaded.
        assert_eq!(loaded.len(), original.len());
        assert_eq!(last_messages_text(&loaded, 2), last_messages_text(&original, 2));

        save_history(&s, c, &loaded).unwrap();
        let again = load_history(&s, c, HISTORY_CAP).unwrap();
        assert_eq!(again, loaded, "reloading must reach a fixed point");
    }

    #[test]
    fn saving_replaces_rather_than_appends() {
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        save_history(&s, c, &[LlmMessage::user("one"), LlmMessage::assistant("two")]).unwrap();
        // `save_history` is the seeding door, not the run's writer: it says
        // "this conversation is exactly these messages". A run appends
        // instead — see `append_history` — so nothing here can shorten a
        // thread by accident.
        save_history(&s, c, &[LlmMessage::assistant("two")]).unwrap();

        let loaded = load_history(&s, c, HISTORY_CAP).unwrap();
        assert_eq!(loaded.len(), 1, "a replace must not leave the old messages behind");
    }

    /// A conversation long enough that the model's window cannot hold it:
    /// 30 exchanges, 60 messages, written the way a run writes them.
    fn a_long_thread(s: &crate::store::Store, account_id: i64) -> i64 {
        let c = s.start_conversation(account_id, "direct").unwrap();
        for i in 0..30 {
            append_history(
                s,
                c,
                &[LlmMessage::user(format!("question {i}")), LlmMessage::assistant(format!("answer {i}"))],
            )
            .unwrap();
        }
        c
    }

    #[test]
    fn the_page_gets_the_whole_thread_and_the_model_gets_its_window() {
        // The bug: what was stored *was* the model's window, so a thread
        // longer than `HISTORY_CAP` opened in the browser showing only its
        // last exchange — and the earlier turns were not merely hidden,
        // they had been deleted. The store is the whole log now, and the
        // cap applies to the load, not to what is kept.
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = a_long_thread(&s, a);

        let turns = transcript_of(&s, c).unwrap();
        assert_eq!(turns.len(), 60, "the page lost turns it should have shown");
        assert_eq!(turns[0].text, "question 0", "the page does not start at the beginning");
        assert_eq!(turns[59].text, "answer 29");

        let window = load_history(&s, c, HISTORY_CAP).unwrap();
        assert!(window.len() <= HISTORY_CAP, "the model was sent {} messages", window.len());
        assert_eq!(
            last_messages_text(&window, 2),
            "user: question 29\nassistant: answer 29",
            "the window is not the newest end of the thread"
        );
    }

    #[test]
    fn the_model_never_opens_on_a_dangling_tool_result() {
        // The window is cut by row count, and a cut can land between a tool
        // call and its result — which providers reject outright. The trim
        // that used to run before saving now runs on the load, so the rule
        // is enforced where the messages are actually handed to the model.
        use rig::completion::message::{AssistantContent, ToolResult, ToolResultContent, UserContent};
        use rig::message::{ToolCall, ToolFunction};
        use rig::one_or_many::OneOrMany;
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        let mut written = vec![LlmMessage::user("cheapest beans")];
        for i in 0..12 {
            written.push(LlmMessage::Assistant {
                id: None,
                content: OneOrMany::one(AssistantContent::ToolCall(ToolCall::new(
                    format!("call-{i}"),
                    ToolFunction { name: "web_search".to_string(), arguments: serde_json::json!({}) },
                ))),
            });
            written.push(LlmMessage::User {
                content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                    id: format!("call-{i}"),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::text("three shops")),
                })),
            });
        }
        written.push(LlmMessage::assistant("here are three"));
        append_history(&s, c, &written).unwrap();

        let window = load_history(&s, c, HISTORY_CAP).unwrap();

        assert!(!window.is_empty(), "the window must not be empty");
        assert!(
            !matches!(
                window.first(),
                Some(LlmMessage::User { content })
                    if content.iter().any(|p| matches!(p, UserContent::ToolResult(_)))
            ),
            "the window opens on an orphaned tool result: {window:?}"
        );
        // Nothing was thrown away to achieve that: the full log still holds
        // the question the trim could not fit.
        let turns = transcript_of(&s, c).unwrap();
        assert_eq!(turns[0].text, "cheapest beans");
    }

    fn a_tool_call(id: &str) -> LlmMessage {
        LlmMessage::Assistant {
            id: None,
            content: rig::OneOrMany::one(rig::message::AssistantContent::ToolCall(
                rig::message::ToolCall::new(
                    id.to_string(),
                    rig::message::ToolFunction {
                        name: "web_search".to_string(),
                        arguments: serde_json::json!({}),
                    },
                ),
            )),
        }
    }

    fn a_tool_result(id: &str) -> LlmMessage {
        LlmMessage::User {
            content: rig::OneOrMany::one(rig::message::UserContent::ToolResult(
                rig::message::ToolResult {
                    id: id.to_string(),
                    call_id: None,
                    content: rig::OneOrMany::one(rig::message::ToolResultContent::text("three shops")),
                },
            )),
        }
    }

    #[test]
    fn the_model_still_sees_the_question_after_a_thread_of_tool_heavy_runs() {
        // The window collapsed to a single message on exactly the threads
        // this bot has: a run stores the question, a dozen-odd tool
        // call/result pairs, and the answer — about twenty-eight rows. Read
        // `cap + 1` of those and every row in hand is tool traffic bar the
        // answer, so `trim_history` finds no plain-user head and its
        // text-only fallback has only those twenty-one rows to filter: one
        // assistant message, and the model is handed its own reply with the
        // question that prompted it nowhere in sight.
        //
        // The fix is to read wide and trim narrow. The fallback is a good
        // answer when it can see the whole log — it keeps the prose, which
        // is what was asked and what was said — and a useless one when the
        // log it can see is tool traffic by construction.
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        for exchange in 0..3 {
            let mut written = vec![LlmMessage::user(format!("question {exchange}"))];
            for step in 0..13 {
                let id = format!("call-{exchange}-{step}");
                written.push(a_tool_call(&id));
                written.push(a_tool_result(&id));
            }
            written.push(LlmMessage::assistant(format!("answer {exchange}")));
            append_history(&s, c, &written).unwrap();
        }

        let window = load_history(&s, c, HISTORY_CAP).unwrap();

        assert!(window.len() <= HISTORY_CAP, "the model was sent {} messages", window.len());
        let excerpt = last_messages_text(&window, 10);
        assert!(
            excerpt.contains("question 2"),
            "the model never sees the question it is answering (window_len={}): {excerpt:?}",
            window.len()
        );
        assert!(excerpt.contains("answer 2"), "the model lost its own last answer: {excerpt:?}");
        assert!(
            matches!(
                window.first(),
                Some(LlmMessage::User { content })
                    if content.iter().all(|p| matches!(p, rig::message::UserContent::Text(_)))
            ),
            "the window does not open on something a person said: {window:?}"
        );
    }

    #[test]
    fn a_transcript_is_the_exchange_without_the_tool_traffic() {
        // History holds tool calls and their results as well as the
        // conversation. A page shows what was said, not how it was found —
        // and `last_messages_text` already draws that line for the
        // continuation classifier, so this draws the same one.
        use scout_api::{Role, Turn};
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();

        // A real exchange has the tool call and its result sitting between
        // the question and the answer. Without them in this fixture the
        // test's own name would be a claim it never checks.
        use rig::completion::message::{AssistantContent, ToolResult, ToolResultContent, UserContent};
        use rig::message::{ToolCall, ToolFunction};
        use rig::one_or_many::OneOrMany;
        save_history(
            &s,
            c,
            &[
                LlmMessage::user("cheapest beans"),
                LlmMessage::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::ToolCall(ToolCall::new(
                        "call-1".to_string(),
                        ToolFunction {
                            name: "web_search".to_string(),
                            arguments: serde_json::json!({}),
                        },
                    ))),
                },
                LlmMessage::User {
                    content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                        id: "call-1".to_string(),
                        call_id: None,
                        content: OneOrMany::one(ToolResultContent::text("three shops")),
                    })),
                },
                LlmMessage::assistant("here are three"),
            ],
        )
        .unwrap();

        assert_eq!(
            transcript_of(&s, c).unwrap(),
            vec![
                Turn { role: Role::You, text: "cheapest beans".into() },
                Turn { role: Role::Scout, text: "here are three".into() },
            ]
        );
    }

    #[test]
    fn a_transcript_does_not_show_the_reasoning_stored_beside_the_answer() {
        // What is stored is the model's raw message. The live stream strips
        // it; a page reading history has to strip it too, or a chain of
        // thought sits on screen every time the page is opened.
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let c = s.start_conversation(a, "direct").unwrap();
        save_history(
            &s,
            c,
            &[
                LlmMessage::user("test"),
                LlmMessage::assistant(
                    "<think>\nThe user just sent \"test\". I should respond briefly.\n</think>\n\nHi! Scout here.",
                ),
            ],
        )
        .unwrap();

        let turns = transcript_of(&s, c).unwrap();
        assert_eq!(turns[1].text, "Hi! Scout here.");
        assert!(
            !turns.iter().any(|t| t.text.contains("<think>")),
            "reasoning reached the page: {turns:?}"
        );
    }

    #[test]
    fn a_title_is_the_first_message_on_one_line_cut_at_forty_characters() {
        assert_eq!(first_message_title("cheapest OneBlade cartridges"), "cheapest OneBlade cartridges");
        assert_eq!(
            first_message_title("  find me   the\ncheapest\n\nPhilips OneBlade replacement cartridges please "),
            "find me the cheapest Philips OneBlade re…"
        );
        // Cut on a character boundary, never inside one.
        assert_eq!(first_message_title(&"ë".repeat(50)), format!("{}…", "ë".repeat(40)));
        assert_eq!(first_message_title("   "), "");
        // The boundary itself: exactly forty is not cut, forty-one is.
        assert_eq!(first_message_title(&"a".repeat(40)), "a".repeat(40));
        assert_eq!(first_message_title(&"a".repeat(41)), format!("{}…", "a".repeat(40)));
        // The automatic name obeys the same rule as the other two: a title
        // cut from a message carries no character that would make the row
        // read as something the message did not say.
        assert_eq!(first_message_title("Be\u{202E}ans"), "Beans");
    }

    #[tokio::test]
    async fn the_first_answer_names_the_thread_and_the_second_does_not_rename_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("titles.duckdb");
        let core = Core::start(crate::config::Config::for_test(path.to_str().unwrap()), None).unwrap();
        let store = core.store();
        let a = store.account_for_telegram(11).unwrap();
        let id = store.start_conversation(a, "direct").unwrap();

        title_if_missing(&core, id, "wasmiddel per kilo, bol.com").await;
        title_if_missing(&core, id, "only under 20 euro").await;

        assert_eq!(store.thread_title(a, id).unwrap().as_deref(), Some("wasmiddel per kilo, bol.com"));
    }

    async fn threads_core() -> (Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads.duckdb");
        let core = Core::start(crate::config::Config::for_test(path.to_str().unwrap()), None).unwrap();
        (core, dir)
    }

    #[tokio::test]
    async fn the_list_marks_exactly_the_thread_telegram_would_continue() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let older = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();
        let newer = seed_exchange_for_tests(&core, a, "direct", "hubs", "two").await.unwrap();
        core.store().set_thread_pinned(a, older, true).unwrap();
        core.store().open_conversation(a, newer).unwrap(); // used last, not merely inserted last

        let list = threads(&core, a).await.unwrap();

        // Pinned first in the list, but current is by last use — so the
        // pinned row is first and the newer row is current.
        assert_eq!(list.iter().map(|t| t.id).collect::<Vec<_>>(), vec![older, newer]);
        assert_eq!(list.iter().filter(|t| t.current).count(), 1);
        assert!(list[1].current);
        assert_eq!(list[0].title.as_deref(), None, "seeding does not run the agent, so no title");
    }

    #[tokio::test]
    async fn opening_a_thread_makes_it_the_one_a_message_continues_and_returns_it() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let older = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();
        let _newer = seed_exchange_for_tests(&core, a, "direct", "hubs", "two").await.unwrap();

        let turns = open_thread(&core, a, older).await.unwrap().expect("my own thread");

        assert_eq!(turns[0].text, "beans");
        assert_eq!(latest_direct(&core.store(), a).unwrap(), Some(older));
        assert_eq!(open_thread(&core, a, 424242).await.unwrap(), None, "opened nothing");
    }

    #[tokio::test]
    async fn opening_a_thread_that_has_no_messages_yet_returns_an_empty_transcript() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let id = reset(&core, a, "direct").await.unwrap();

        assert_eq!(open_thread(&core, a, id).await.unwrap(), Some(vec![]));
        assert_eq!(suggest_title(&core, a, id).await.unwrap(), None, "an empty thread must not reach the model");
    }

    #[tokio::test]
    async fn rename_pin_and_delete_answer_not_found_for_someone_elses_thread() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let b = core.store().account_for_telegram(22).unwrap();
        let mine = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();

        assert_eq!(rename(&core, b, mine, "theirs").await.unwrap(), Renamed::NotFound);
        assert!(!set_pinned(&core, b, mine, true).await.unwrap());
        assert!(!delete_thread(&core, b, mine).await.unwrap());

        assert_eq!(rename(&core, a, mine, "  beans, cheapest  ").await.unwrap(), Renamed::Done);
        assert_eq!(core.store().thread_title(a, mine).unwrap().as_deref(), Some("beans, cheapest"));
        assert!(set_pinned(&core, a, mine, true).await.unwrap());
        assert!(threads(&core, a).await.unwrap()[0].pinned);
        assert!(set_pinned(&core, a, mine, false).await.unwrap());
        assert!(!threads(&core, a).await.unwrap()[0].pinned, "an unpin that fails makes a thread immortal");
        assert!(delete_thread(&core, a, mine).await.unwrap());
        assert!(threads(&core, a).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rename_refuses_an_empty_name_and_cuts_a_long_one() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let mine = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();

        assert_eq!(rename(&core, a, mine, "   ").await.unwrap(), Renamed::Blank, "an empty name is not a name");
        rename(&core, a, mine, &"x".repeat(100)).await.unwrap();
        assert_eq!(core.store().thread_title(a, mine).unwrap().unwrap().chars().count(), 80);

        // A name typed into a browser gets the same rule as one a model
        // wrote: a bidi override in a sidebar row is no less a lie for
        // having arrived over HTTP.
        rename(&core, a, mine, "Be\u{202E}ans").await.unwrap();
        assert_eq!(core.store().thread_title(a, mine).unwrap().as_deref(), Some("Beans"));
    }

    #[tokio::test]
    async fn touching_a_thread_thats_already_gone_reports_it() {
        // The continuation check and the bump that follows it are two
        // separate round trips with an LLM call between them — long enough
        // for the thread's own TTL to run out and the row to be deleted
        // before the bump lands. `touch_conversation` has to say so rather
        // than silently updating nothing.
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let id = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();

        assert!(core.store().delete_conversation(a, id).unwrap());
        assert!(!core.store().touch_conversation(id).unwrap(), "a deleted thread has nothing to bump");
    }

    #[test]
    fn a_thread_gone_by_the_time_the_continuation_bumps_it_starts_fresh() {
        // `resolve_conversation` needs a live model to reach this branch —
        // `continues_previous` only comes back `Ok(true)` after a real
        // call — so this is asserted from the source instead: the
        // continuation branch must check `touch_conversation`'s result and
        // fall back to `start_conversation` when it comes back false,
        // rather than handing back an id for a row that is already gone.
        let src = include_str!("session.rs");
        let start = src.find("Ok(true) => {").expect("the continuation branch must exist");
        let end = src[start..].find("Ok(false) => {").expect("the next arm must exist") + start;
        let body = &src[start..end];
        assert!(body.contains("if !"), "the branch does not check whether the bump found a row");
        let touch_at = body.find("touch_conversation").expect("the branch must bump the thread");
        let start_at = body.find("start_conversation").expect("a thread gone underneath it must fall back to a fresh one");
        assert!(start_at > touch_at, "the fallback must run after the bump, not before it");
    }

    #[test]
    fn a_suggested_title_is_one_line_without_quotes_and_never_empty() {
        assert_eq!(clean_title("\"Cheapest OneBlade cartridges\"\n"), Some("Cheapest OneBlade cartridges".to_string()));
        assert_eq!(clean_title("Title: 'AMS to LIS in October'."), Some("AMS to LIS in October".to_string()));
        assert_eq!(clean_title("<think>hmm</think>Wasmiddel per kilo"), Some("Wasmiddel per kilo".to_string()));
        assert_eq!(clean_title("   "), None);
        // A chatty model announces the title on the line before it.
        assert_eq!(
            clean_title("Sure! Here is a title for your conversation:\nBeans, cheapest"),
            Some("Beans, cheapest".to_string())
        );
        assert_eq!(clean_title("**Title:** Beans"), Some("Beans".to_string()));
        // The label can be the whole line, and then the answer is the next
        // one — which it only looks like once the label is off.
        assert_eq!(clean_title("**Title:**\nBeans"), Some("Beans".to_string()));
        assert_eq!(
            clean_title("1. Cheapest OneBlade cartridges"),
            Some("Cheapest OneBlade cartridges".to_string())
        );
        // Two digits is a list; four is a year the title starts with.
        assert_eq!(clean_title("2024. Year in review"), Some("2024. Year in review".to_string()));
        // Markdown is decoration, not part of the name.
        assert_eq!(clean_title("**Beans, cheapest**"), Some("Beans, cheapest".to_string()));
        assert_eq!(clean_title("Here is your title:\n\n**Beans**"), Some("Beans".to_string()));
        assert_eq!(clean_title("### Beans"), Some("Beans".to_string()));
        // A title is one line of plain text: no control characters, no
        // bidi override to make a sidebar row read backwards, and no
        // invisible that leaves a name looking like another one.
        assert_eq!(clean_title("Beans\u{202E}cheapest\r"), Some("Beanscheapest".to_string()));
        assert_eq!(clean_title("Bea\u{200B}ns"), Some("Beans".to_string()));
        // The cut is at `RENAME_CHARS` and never leaves a trailing space:
        // "word " is five characters, so the cut lands on the space after
        // the sixteenth word and the trim takes it back off.
        let long = clean_title(&"word ".repeat(30)).unwrap();
        assert_eq!(long.chars().count(), 79);
    }

    #[tokio::test]
    async fn suggesting_a_title_for_someone_elses_thread_asks_nobody_and_finds_nothing() {
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let b = core.store().account_for_telegram(22).unwrap();
        let mine = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();
        // No model is reachable in tests; reaching one would be an error,
        // not `None`, so `None` proves the check ran first.
        assert_eq!(suggest_title(&core, b, mine).await.unwrap(), None);
    }

    #[tokio::test]
    async fn suggesting_a_title_does_not_make_the_thread_current() {
        // Pressing "name this" on an old sidebar row names it. It must not
        // also make it the thread Telegram continues — the person did not
        // switch threads, they asked for a label.
        let (core, _dir) = threads_core().await;
        let a = core.store().account_for_telegram(11).unwrap();
        let older = seed_exchange_for_tests(&core, a, "direct", "beans", "three").await.unwrap();
        let newer = seed_exchange_for_tests(&core, a, "direct", "hubs", "two").await.unwrap();
        core.store().open_conversation(a, newer).unwrap();
        assert_eq!(latest_direct(&core.store(), a).unwrap(), Some(newer));

        // No model is reachable in tests, so this ends in an error — after
        // the read that used to bump the thread. Asserting the error is
        // what keeps the test honest: an ownership check that silently
        // returned `None` would never reach the read, and the assertion
        // below would pass for the wrong reason.
        assert!(
            suggest_title(&core, a, older).await.is_err(),
            "the call must have reached the model"
        );

        assert_eq!(latest_direct(&core.store(), a).unwrap(), Some(newer), "naming a thread switched to it");
    }

    #[test]
    fn a_conversation_that_was_never_started_is_not_started_by_asking() {
        // Opening a page must not write rows. `resolve_conversation` would
        // create one, which is why the page does not use it.
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        assert_eq!(latest_direct(&s, a).unwrap(), None);
        assert_eq!(latest_direct(&s, a).unwrap(), None, "asking twice created one");
    }
}
