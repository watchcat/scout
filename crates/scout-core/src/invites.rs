use crate::core::Core;
use crate::store::Store;

/// What claiming a seat did. Defined next to the table it is read from,
/// re-exported here because this is where a channel meets it.
pub use crate::store::Claim;

/// Capacity when `/invite new` is given a name and no number.
pub(crate) const DEFAULT_CAPACITY: i64 = 100;
pub(crate) const INVITE_USAGE: &str = "usage:\n\
/invite new <name> [capacity] - open a round (capacity defaults to 100)\n\
/invite status - rounds, seats used, and who is waiting\n\
/invite open <name> | /invite close <name>\n\
/invite announce <name> - tell the waitlist a round is open";

/// A Telegram start parameter is 1-64 characters long.
pub(crate) const MAX_ROUND_NAME: usize = 64;

/// How someone already in a chat with Scout joins a round.
///
/// The link is not that route. Telegram only shows the START button — and
/// only START delivers the payload — in a chat with no history, so anyone
/// who has ever messaged the bot (which is everyone on the waitlist, since
/// being turned away is itself a message) opens a link to nothing. Sending
/// the command by hand delivers the same payload and always works.
pub(crate) fn join_instruction(code: &str) -> String {
    format!("/start {code}")
}

/// The invite link itself, for posting somewhere public where the people
/// reading it have never messaged Scout — which is where it does work.
pub(crate) fn join_link(username: &str, code: &str) -> String {
    format!("https://t.me/{username}?start={code}")
}

pub const NOT_ADMIN: &str = "That command is for the bot's admin.";

#[derive(Debug, PartialEq, Eq)]
pub enum InviteCmd {
    New { code: String, capacity: i64 },
    Status,
    SetOpen { code: String, open: bool },
    Announce(String),
}

pub fn parse_invite(arg: &str) -> Result<InviteCmd, String> {
    let mut words = arg.split_whitespace();
    let Some(verb) = words.next() else {
        return Err(INVITE_USAGE.to_string());
    };
    let cmd = match verb.to_ascii_lowercase().as_str() {
        "status" => InviteCmd::Status,
        "new" => {
            let code = check_round_name(words.next())?;
            let capacity = match words.next() {
                Some(raw) => check_capacity(raw)?,
                None => DEFAULT_CAPACITY,
            };
            InviteCmd::New { code, capacity }
        }
        "open" => InviteCmd::SetOpen { code: check_round_name(words.next())?, open: true },
        "close" => InviteCmd::SetOpen { code: check_round_name(words.next())?, open: false },
        "announce" => InviteCmd::Announce(check_round_name(words.next())?),
        other => return Err(format!("I don't know \"{other}\".\n\n{INVITE_USAGE}")),
    };
    // A trailing word is a typo, not something to ignore: silently dropping
    // it is how "/invite new autumn 100 seats" opens a round nobody meant.
    if let Some(extra) = words.next() {
        return Err(format!("I didn't expect \"{extra}\".\n\n{INVITE_USAGE}"));
    }
    Ok(cmd)
}

/// A round name is also a Telegram start parameter, so it is checked
/// against exactly what one may contain — 1-64 characters of `A-Za-z0-9_-`.
/// Refusing at the point the admin types it beats discovering it when
/// nobody can join.
pub(crate) fn check_round_name(name: Option<&str>) -> Result<String, String> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Err(format!("that needs a round name.\n\n{INVITE_USAGE}"));
    };
    if name.chars().count() > MAX_ROUND_NAME {
        return Err(format!(
            "\"{name}\" is {} characters; an invite link carries at most {MAX_ROUND_NAME}.",
            name.chars().count()
        ));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(format!(
            "\"{name}\" has characters an invite link can't carry. \
             Use letters, digits, - and _ only."
        ));
    }
    Ok(name.to_string())
}

pub(crate) fn check_capacity(raw: &str) -> Result<i64, String> {
    let n: i64 = raw
        .parse()
        .map_err(|_| format!("\"{raw}\" is not a number of seats."))?;
    if n < 1 {
        return Err("a round needs at least one seat.".to_string());
    }
    Ok(n)
}

pub(crate) fn parse_user_id(arg: &str) -> Result<i64, String> {
    let arg = arg.trim();
    if arg.is_empty() {
        return Err("that needs a numeric user id — /stat lists them.".to_string());
    }
    match arg.parse::<i64>() {
        Ok(id) if id > 0 => Ok(id),
        _ => Err(format!("\"{arg}\" is not a Telegram user id. /stat lists them.")),
    }
}

/// What the waitlist is sent when a round opens. HTML so the command sits
/// in a `<code>` span, which Telegram makes tap-to-copy — the code is
/// validated to `A-Za-z0-9_-`, so there is nothing in it to escape.
pub(crate) fn announce_message(code: &str) -> String {
    format!(
        "A new round of Scout invites just opened — you asked to be told.\n\n\
         Send me this to claim a seat:\n<code>{}</code>\n\n\
         (Tap it to copy.) Seats are first come, first served.",
        join_instruction(code)
    )
}

pub(crate) fn new_round_reply(code: &str, capacity: i64, username: Option<&str>) -> String {
    let mut reply = format!("Round \"{code}\" is open for {capacity}.\n\n");
    match username {
        Some(username) => reply.push_str(&format!("{}\n\n", join_link(username, code))),
        None => reply.push_str(
            "I couldn't read my own username just now, so I can't build the \
             link — it is https://t.me/<botname>?start= followed by the name.\n\n",
        ),
    }
    reply.push_str(&format!(
        "The link only works for people who have never messaged Scout: \
         Telegram shows the START button only in an empty chat, and only \
         START carries the code. Anyone who has already talked to Scout \
         joins by sending\n{}\n\n\
         /invite announce {code} tells the waitlist, using that same command.",
        join_instruction(code)
    ));
    reply
}

pub(crate) fn status_report(rounds: &[crate::store::RoundStatus], waiting: i64) -> String {
    if rounds.is_empty() {
        return "No rounds yet. /invite new <name> [capacity] opens one.".to_string();
    }
    let mut out = String::new();
    for r in rounds {
        out.push_str(&format!(
            "{} — {}/{} seats, {}\n",
            r.code,
            r.used,
            r.capacity,
            if r.open { "open" } else { "closed" }
        ));
    }
    out.push_str(&format!("\n{waiting} waiting."));
    out
}

/// What an announcement would do, decided without sending anything.
pub enum Announcement {
    /// Nobody should be told, and why.
    Refused(String),
    /// Who to reach, as (account, address), and what to say.
    Ready { targets: Vec<(i64, i64)>, text: String },
}

/// Whether a recipient was reached.
///
/// Three states rather than a bool because "did not arrive" and "will never
/// arrive" call for opposite responses: retry the first, stop chasing the
/// second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reached {
    Yes,
    No,
    Gone,
}

/// Chooses who hears that a round is open, oldest first.
///
/// Decides and does not send, so the same decision serves any channel. A
/// closed, unknown or full round is refused here rather than discovered
/// halfway through a broadcast: announcing a round nobody can join spends
/// the whole waitlist's one notification on a dead end, and stamps them as
/// told about something real.
pub async fn plan_announcement(core: &Core, code: &str) -> anyhow::Result<Announcement> {
    let store = core.store();
    let code = code.to_string();
    crate::core::blocking(move || plan_announcement_blocking(&store, &code)).await
}

fn plan_announcement_blocking(store: &Store, code: &str) -> anyhow::Result<Announcement> {
    let rounds = store.rounds()?;
    let refusal = match rounds.iter().find(|r| r.code == code) {
        None => Some(format!("There's no round called \"{code}\".")),
        Some(r) if !r.open => Some(format!(
            "Round \"{code}\" is closed, so nobody could join through it. \
             /invite open {code} first."
        )),
        Some(r) if r.used >= r.capacity => Some(format!(
            "Round \"{code}\" is full ({}/{}), so nobody could join through it. \
             Open a bigger round instead.",
            r.used, r.capacity
        )),
        Some(_) => None,
    };
    if let Some(refusal) = refusal {
        return Ok(Announcement::Refused(refusal));
    }
    let targets = store.waitlist_to_invite()?;
    if targets.is_empty() {
        return Ok(Announcement::Refused("Nobody is waiting to be told.".to_string()));
    }
    Ok(Announcement::Ready { targets, text: announce_message(code) })
}

/// Records what actually happened, one entry per recipient.
///
/// Stamped per success, so a re-run reaches only who was missed. A recipient
/// who has blocked Scout is dropped from the waitlist entirely — chasing them
/// forever would make every later announcement slower for everyone else.
pub async fn record_announcement(core: &Core, outcomes: Vec<(i64, Reached)>) -> anyhow::Result<()> {
    let store = core.store();
    crate::core::blocking(move || record_announcement_blocking(&store, &outcomes)).await
}

fn record_announcement_blocking(store: &Store, outcomes: &[(i64, Reached)]) -> anyhow::Result<()> {
    for (account_id, reached) in outcomes {
        match reached {
            Reached::Yes => store.mark_invited(*account_id)?,
            Reached::No => {}
            Reached::Gone => store.forget_waitlist(*account_id)?,
        }
    }
    Ok(())
}

/// Everyone an announcement could reach on Telegram, as (account, chat).
///
/// A thin wrapper today, but it is the door `/advert` goes through, and in
/// 2b-2 it becomes an endpoint rather than a store call. Someone with an
/// account but no recorded address is absent, because there is nowhere to
/// send — silently reaching nobody would read as a delivered announcement.
pub async fn advert_targets(core: &Core) -> anyhow::Result<Vec<(i64, i64)>> {
    let store = core.store();
    crate::core::blocking(move || store.broadcast_targets()).await
}


/// Runs an invite command and returns what to say about it.
///
/// Returns text rather than sending it: core has no channel, and the web app
/// will want this same answer rendered differently.
///
/// `bot_username` is supplied by the caller because it is a fact about a
/// channel, not about Scout. Only `New` uses it, and a reply without a link
/// is better than losing a round because a username lookup blipped.
///
/// The admin check is here as well as in the adapter. The adapter's is a
/// courtesy that saves a round trip; this one is the actual gate, because in
/// 2b-2 the caller is across a network and may be the web app.
pub async fn invite(
    core: &Core,
    admin_telegram_id: i64,
    cmd: InviteCmd,
    bot_username: Option<&str>,
) -> String {
    if !core.is_admin(admin_telegram_id) {
        return NOT_ADMIN.to_string();
    }
    let store = core.store();
    match cmd {
        InviteCmd::New { code, capacity } => {
            let created = {
                let (store, code) = (store.clone(), code.clone());
                crate::core::blocking(move || store.create_round(&code, capacity)).await
            };
            match created {
                Err(e) => {
                    tracing::error!(error = %e, code, "could not open a round");
                    "Sorry, I couldn't open that round.".to_string()
                }
                Ok(false) => format!(
                    "There is already a round called \"{code}\". \
                     Pick another name — reusing one would pool two rounds' \
                     seats under a single capacity."
                ),
                Ok(true) => new_round_reply(&code, capacity, bot_username),
            }
        }
        InviteCmd::Status => {
            let status =
                crate::core::blocking(move || Ok((store.rounds()?, store.waiting_count()?))).await;
            match status {
                Ok((rounds, waiting)) => status_report(&rounds, waiting),
                Err(e) => {
                    tracing::error!(error = %e, "could not read invite status");
                    "Sorry, I couldn't read the rounds.".to_string()
                }
            }
        }
        InviteCmd::SetOpen { code, open } => {
            let changed = {
                let code = code.clone();
                crate::core::blocking(move || store.set_round_open(&code, open)).await
            };
            match changed {
                Err(e) => {
                    tracing::error!(error = %e, code, "could not change a round");
                    "Sorry, I couldn't change that round.".to_string()
                }
                Ok(false) => format!("There's no round called \"{code}\"."),
                Ok(true) if open => format!("Round \"{code}\" is admitting again."),
                Ok(true) => format!(
                    "Round \"{code}\" is closed. Its seats stay spent; \
                     /invite open {code} resumes it."
                ),
            }
        }
        // Announcing needs a channel to send on, so the adapter handles it
        // through plan_announcement instead. Reaching here means a caller
        // routed it wrongly.
        InviteCmd::Announce(_) => {
            "Announcing needs a channel to send on.".to_string()
        }
    }
}

/// What a `/kick` or `/unkick` did.
///
/// `membership` is what the caller's cache should now say about this
/// Telegram id: `Some(true)` in, `Some(false)` out, `None` unchanged. Core
/// keeps no cache — the table is the record — but the adapter's gate does,
/// and it has to follow.
pub struct KickOutcome {
    pub reply: String,
    pub membership: Option<bool>,
}

/// Revokes (`kicking`) or restores a member named by Telegram id.
pub async fn kick(
    core: &Core,
    admin_telegram_id: i64,
    arg: &str,
    kicking: bool,
) -> KickOutcome {
    let no_change = |reply: String| KickOutcome { reply, membership: None };
    if !core.is_admin(admin_telegram_id) {
        return no_change(NOT_ADMIN.to_string());
    }
    let target = match parse_user_id(arg) {
        Ok(t) => t,
        Err(problem) => return no_change(problem),
    };
    let store = core.store();
    let changed = crate::core::blocking(move || match kicking {
        true => store.revoke(store.account_for_telegram(target)?),
        false => store.restore(store.account_for_telegram(target)?),
    })
    .await;
    match changed {
        Err(e) => {
            tracing::error!(error = %e, target, kicking, "membership change failed");
            no_change("Sorry, I couldn't write that down, so nothing changed.".to_string())
        }
        Ok(true) if kicking => KickOutcome {
            reply: format!(
                "{target} is out. Their seat stays spent, so the round \
                 does not quietly reopen."
            ),
            membership: Some(false),
        },
        Ok(true) => KickOutcome {
            reply: format!("{target} is back in. That consumed no seat."),
            membership: Some(true),
        },
        Ok(false) if kicking => no_change(format!(
            "{target} isn't a member, so there's nothing to remove. \
             Founders listed in ALLOWED_TELEGRAM_USER_IDS aren't \
             members — remove those from .env instead."
        )),
        Ok(false) => no_change(format!(
            "{target} isn't a revoked member, so there's nothing to undo."
        )),
    }
}


/// Claims a seat on a round for whoever pressed START.
///
/// Resolves the account first: admission is recorded against a person, not
/// against a Telegram id, so the same person arriving later by another route
/// is already in.
pub async fn claim(
    core: &Core,
    telegram_id: i64,
    chat_id: i64,
    code: &str,
) -> anyhow::Result<crate::store::Claim> {
    let store = core.store();
    let code = code.to_string();
    crate::core::blocking(move || {
        let account_id = store.account_for_telegram(telegram_id)?;
        let outcome = store.claim_seat(account_id, &code)?;
        // Where to reach them is the channel's business, not the seat's.
        // Recorded whatever the outcome was: the START they just pressed is
        // the permission, and an admitted person who has not spoken yet
        // still needs to be reachable by an announcement.
        store.note_delivery(account_id, "telegram", &chat_id.to_string())?;
        Ok(outcome)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_advert_reaches_everyone_with_a_known_address() {
        let (s, _d) = crate::store::tests::test_store();
        let a = s.account_for_telegram(11).unwrap();
        let b = s.account_for_telegram(22).unwrap();
        s.note_delivery(a, "telegram", "555").unwrap();
        s.note_delivery(b, "telegram", "666").unwrap();
        // Has an account but has never spoken, so there is nowhere to send.
        s.account_for_telegram(33).unwrap();

        assert_eq!(s.broadcast_targets().unwrap(), vec![(a, 555), (b, 666)]);
    }

    #[tokio::test]
    async fn an_admitted_telegram_user_is_reachable_without_speaking_first() {
        // The gap this closes: the delivery row used to be written only on
        // the waitlist branch, so someone admitted straight away had no
        // recorded address until their first message, and an announcement
        // could not reach them.
        let dir = tempfile::tempdir().unwrap();
        let core = crate::core::Core::start(
            crate::config::Config::for_test(dir.path().join("claim.duckdb").to_str().unwrap()),
            None,
        )
        .unwrap();
        core.store().create_round("autumn", 5).unwrap();

        assert_eq!(claim(&core, 42, 4242, "autumn").await.unwrap(), Claim::Admitted);

        let account = core.store().account_for_telegram(42).unwrap();
        assert_eq!(
            core.store().broadcast_targets().unwrap(),
            vec![(account, 4242)],
            "admitted straight away, but still reachable — no message required first"
        );
    }

    #[test]
    fn an_announcement_refuses_a_round_that_cannot_take_anyone() {
        let (s, _d) = crate::store::tests::test_store();
        s.create_round("autumn", 1).unwrap();
        let a = s.account_for_telegram(11).unwrap();
        s.claim_seat(a, "autumn").unwrap();

        // Full. Announcing it would invite people to a door that is shut.
        match plan_announcement_blocking(&s, "autumn").unwrap() {
            Announcement::Refused(reason) => assert!(reason.contains("full"), "{reason}"),
            Announcement::Ready { .. } => panic!("a full round must not be announced"),
        }
        match plan_announcement_blocking(&s, "no-such-round").unwrap() {
            Announcement::Refused(reason) => assert!(reason.contains("no round"), "{reason}"),
            Announcement::Ready { .. } => panic!("an unknown round must not be announced"),
        }
    }

    #[test]
    fn an_announcement_reaches_the_longest_waiting_first_and_records_only_what_landed() {
        let (s, _d) = crate::store::tests::test_store();
        s.create_round("autumn", 1).unwrap();
        let first = s.account_for_telegram(11).unwrap();
        s.claim_seat(first, "autumn").unwrap();
        // Both turned away, in order, so both land on the waitlist.
        let second = s.account_for_telegram(22).unwrap();
        s.claim_seat(second, "autumn").unwrap();
        // claim_seat no longer records where to reach them — the channel
        // does — so give the waitlist the addresses it needs to find them.
        s.note_delivery(second, "telegram", "22").unwrap();
        let third = s.account_for_telegram(33).unwrap();
        s.claim_seat(third, "autumn").unwrap();
        s.note_delivery(third, "telegram", "33").unwrap();

        s.create_round("winter", 5).unwrap();
        let Announcement::Ready { targets, text } = plan_announcement_blocking(&s, "winter").unwrap() else {
            panic!("an open round with room should be announceable");
        };
        assert_eq!(
            targets.iter().map(|t| t.0).collect::<Vec<_>>(),
            vec![second, third],
            "oldest first, so a round smaller than the queue reaches those who waited longest"
        );
        assert!(text.contains("/start winter"), "the command, because a link cannot reach them");
        assert!(!text.contains("t.me/"), "a link only works on an empty chat");

        // Only the one that landed is stamped; the other must be retried.
        record_announcement_blocking(&s, &[(second, Reached::Yes), (third, Reached::No)]).unwrap();
        let Announcement::Ready { targets, .. } = plan_announcement_blocking(&s, "winter").unwrap() else {
            panic!("still announceable");
        };
        assert_eq!(
            targets.iter().map(|t| t.0).collect::<Vec<_>>(),
            vec![third],
            "a delivery that failed is tried again"
        );

        // Someone gone for good is dropped rather than chased forever.
        record_announcement_blocking(&s, &[(third, Reached::Gone)]).unwrap();
        match plan_announcement_blocking(&s, "winter").unwrap() {
            Announcement::Refused(r) => assert!(r.contains("Nobody is waiting"), "{r}"),
            Announcement::Ready { targets, .. } => panic!("still targeting {targets:?}"),
        }
    }

    #[test]
    fn round_names_are_held_to_what_an_invite_link_can_carry() {
        // Telegram start parameters are 1-64 of A-Za-z0-9_-. Refused where
        // it is typed, rather than discovered when nobody can join.
        assert_eq!(check_round_name(Some("autumn-drop_2")).unwrap(), "autumn-drop_2");
        assert_eq!(check_round_name(Some(&"a".repeat(64))).unwrap().len(), 64);

        assert!(check_round_name(None).is_err());
        assert!(check_round_name(Some("")).is_err());
        assert!(check_round_name(Some("   ")).is_err());
        assert!(check_round_name(Some(&"a".repeat(65))).is_err());
        for bad in ["autumn drop", "autumn.drop", "осень", "autumn/drop", "autumn?x=1"] {
            assert!(check_round_name(Some(bad)).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn capacity_defaults_to_a_hundred_and_refuses_nonsense() {
        assert_eq!(
            parse_invite("new autumn").unwrap(),
            InviteCmd::New { code: "autumn".to_string(), capacity: 100 }
        );
        assert_eq!(
            parse_invite("new autumn 250").unwrap(),
            InviteCmd::New { code: "autumn".to_string(), capacity: 250 }
        );
        assert!(parse_invite("new autumn lots").is_err());
        assert!(parse_invite("new autumn 0").is_err(), "a round with no seats admits nobody");
        assert!(parse_invite("new autumn -5").is_err());
    }

    #[test]
    fn invite_subcommands_parse_and_a_stray_word_is_refused() {
        assert_eq!(parse_invite("status").unwrap(), InviteCmd::Status);
        assert_eq!(parse_invite("  STATUS ").unwrap(), InviteCmd::Status);
        assert_eq!(
            parse_invite("open autumn").unwrap(),
            InviteCmd::SetOpen { code: "autumn".to_string(), open: true }
        );
        assert_eq!(
            parse_invite("close autumn").unwrap(),
            InviteCmd::SetOpen { code: "autumn".to_string(), open: false }
        );
        assert_eq!(
            parse_invite("announce autumn").unwrap(),
            InviteCmd::Announce("autumn".to_string())
        );

        // Nothing at all, an unknown verb, and a missing name all say how
        // to use it rather than doing something surprising.
        for bad in ["", "   ", "delete autumn", "open", "announce", "new"] {
            assert!(parse_invite(bad).is_err(), "accepted: {bad:?}");
        }
        // A trailing word is a typo. Ignoring it is how "/invite new autumn
        // 100 seats" opens a round nobody meant.
        assert!(parse_invite("new autumn 100 seats").is_err());
        assert!(parse_invite("status now").is_err());
    }

    #[test]
    fn a_kick_needs_a_real_user_id() {
        assert_eq!(parse_user_id(" 123456 ").unwrap(), 123456);
        for bad in ["", "  ", "@watchcat", "0", "-1", "12.5"] {
            assert!(parse_user_id(bad).is_err(), "accepted: {bad:?}");
        }
    }

    #[test]
    fn the_announce_asks_for_the_command_because_a_link_would_not_work() {
        // Everyone on the waitlist has chat history with Scout — being
        // turned away is itself a message — and Telegram only delivers a
        // start payload through the START button, which only appears in an
        // empty chat. A link here would open a chat and carry nothing.
        let out = announce_message("autumn-drop");
        assert!(out.contains("/start autumn-drop"), "got: {out}");
        assert!(
            !out.contains("t.me/"),
            "a link is the one route this audience cannot use: {out}"
        );
        // Tap-to-copy, so nobody has to retype a code by hand.
        assert!(out.contains("<code>/start autumn-drop</code>"), "got: {out}");
    }

    #[test]
    fn opening_a_round_gives_the_link_and_the_command_it_falls_back_to() {
        let reply = new_round_reply("autumn", 100, Some("scout_bot"));
        assert!(reply.contains("https://t.me/scout_bot?start=autumn"), "got: {reply}");
        assert!(reply.contains("/start autumn"), "got: {reply}");
        assert!(reply.contains("100"));

        // get_me can blip. Losing the round over it would be worse than a
        // reply without a link, so the round is still open and the code is
        // still in the message.
        let reply = new_round_reply("autumn", 100, None);
        assert!(reply.contains("autumn"), "got: {reply}");
        assert!(reply.contains("/start autumn"), "got: {reply}");
    }

    #[test]
    fn status_shows_seats_capacity_and_the_queue() {
        use crate::store::RoundStatus;
        let rounds = vec![
            RoundStatus { code: "autumn".into(), capacity: 100, used: 100, open: true },
            RoundStatus { code: "winter".into(), capacity: 50, used: 3, open: false },
        ];
        let out = status_report(&rounds, 12);
        assert!(out.contains("autumn — 100/100 seats, open"), "got: {out}");
        assert!(out.contains("winter — 3/50 seats, closed"), "got: {out}");
        assert!(out.contains("12 waiting"), "got: {out}");

        let empty = status_report(&[], 0);
        assert!(empty.contains("/invite new"), "got: {empty}");
    }
}