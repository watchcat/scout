//! The small pages the signed-in half renders.
//!
//! Strings and `replace`, like `page.rs`, and for the same reason: a
//! template engine to interpolate three values is a dependency to keep
//! patched forever. The cost is that escaping is a thing this file has to
//! remember rather than a thing the engine does, so anything that came in
//! over HTTP goes through `escape` on its way out — see `confirm`.

const SHELL: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>__TITLE__ — Scout</title>
<link rel="icon" href="/icon.svg" type="image/svg+xml">
<style>
  :root{--base03:#002b36; --base02:#073642; --base01:#586e75; --base0:#839496;
    --base1:#93a1a1; --base2:#eee8d5; --blue:#268bd2}
  *{box-sizing:border-box}
  body{margin:0; background:var(--base03); color:var(--base0);
    font:16px/1.65 -apple-system,BlinkMacSystemFont,"Segoe UI",Inter,system-ui,sans-serif;
    -webkit-font-smoothing:antialiased}
  .wrap{max-width:520px; margin:0 auto; padding:70px 24px}
  a{color:var(--blue); text-decoration:none}
  a:hover{text-decoration:underline}
  .mark{display:flex; align-items:center; gap:12px; margin-bottom:30px; color:var(--cyan,#2aa198)}
  .mark .logo{width:44px; height:44px; flex:none}
  .mark .word{font-size:22px; font-weight:700; color:var(--base2)}
  h1{font-size:26px; line-height:1.2; margin:0 0 14px; color:var(--base2);
    font-weight:700; letter-spacing:-.02em}
  p{margin:0 0 18px}
  .card{background:var(--base02); border:1px solid #0d4a5a; border-radius:6px;
    padding:24px}
  label{display:block; font-size:12px; letter-spacing:.1em; text-transform:uppercase;
    color:var(--base01); margin:0 0 8px; font-weight:600}
  input[type=email]{width:100%; padding:11px 12px; border-radius:5px;
    border:1px solid var(--base01); background:var(--base03); color:var(--base2);
    font:inherit; margin:0 0 18px}
  .btn{background:var(--blue); color:var(--base03); font:inherit; font-weight:650;
    padding:11px 22px; border-radius:5px; border:0; cursor:pointer}
  .btn:hover{background:#3196d8}
  .btn.ghost{background:transparent; color:var(--base1); border:1px solid var(--base01)}
  .btn.ghost:hover{background:transparent; color:var(--base2)}
  .muted{font-size:14px; color:var(--base01); margin:18px 0 0}
</style>
</head>
<body>
<div class="wrap">
  <a class="mark" href="/">
    <svg class="logo" viewBox="0 0 72 72" aria-hidden="true">
      <g fill="none" stroke="currentColor">
        <circle cx="23.5" cy="36" r="11.5" stroke-width="3.2"/>
        <circle cx="48.5" cy="36" r="11.5" stroke-width="3.2"/>
        <circle cx="23.5" cy="36" r="4" stroke-width="2.6"/>
        <circle cx="48.5" cy="36" r="4" stroke-width="2.6"/>
      </g>
      <rect x="34" y="33.4" width="4" height="5.2" fill="currentColor"/>
    </svg>
    <span class="word">Scout</span>
  </a>
__BODY__
</div>
</body>
</html>
"#;

/// Wraps a page's markup in the site's shell.
///
/// `title` is a literal from this module, never anything a visitor sent, so
/// it is substituted unescaped. If that ever stops being true it has to go
/// through `escape` too.
fn shell(title: &str, body: &str) -> String {
    SHELL.replace("__TITLE__", title).replace("__BODY__", body)
}

/// The five characters that can turn text into markup.
///
/// Only ever applied to values that arrived over HTTP. The `t` parameter is
/// the one that matters: anyone can put anything in it and mail the link to
/// somebody else, and it is rendered back into an attribute.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Ask for an address, and say what will happen to it.
///
/// `csrf` is not escaped, here or in `confirm`: it is `session::mint`'s
/// output, which is decimal digits, base64url and dots, and none of those
/// can close an attribute. A value from the request would be a different
/// matter, which is why `t` is escaped and this is not.
pub fn sign_in(csrf: &str, widget: Option<&str>) -> String {
    // No widget when we could not read our own Telegram name at start-up.
    // The same choice `page.rs` makes about the join link: a login button
    // naming the wrong bot, or no bot, is worse than one way in.
    let telegram = match widget {
        Some(w) => format!(
            r#"
  <p class="muted">Or, if you already talk to Scout on Telegram:</p>
  <div class="card">{w}</div>"#
        ),
        None => String::new(),
    };
    shell(
        "Sign in",
        &format!(
            r#"  <h1>Sign in to Scout</h1>
  <p>We'll email you a link. There is no password to forget.</p>
  <div class="card">
    <form method="post" action="/sign-in/email">
      <input type="hidden" name="csrf" value="{csrf}">
      <label for="email">Email address</label>
      <input type="email" id="email" name="email" autocomplete="email"
             autofocus required placeholder="you@example.com">
      <button class="btn" type="submit">Email me a link</button>
    </form>
  </div>{telegram}"#
        ),
    )
}

/// Telegram's login button, as Telegram's own script draws it.
///
/// The one thing this site loads from anywhere but itself, which is why
/// `lib.rs` names `telegram.org` in the Content-Security-Policy and
/// nothing else. `data-auth-url` has to be an absolute URL on a domain
/// that has been given to BotFather; Telegram refuses the login otherwise,
/// and it refuses it in its own popup where our logs never see it.
pub fn telegram_widget(bot_username: &str, auth_url: &str) -> String {
    format!(
        r#"<script async src="https://telegram.org/js/telegram-widget.js?22"
        data-telegram-login="{bot}" data-size="large"
        data-auth-url="{auth}" data-radius="5"></script>"#,
        bot = escape(bot_username),
        auth = escape(auth_url),
    )
}

/// Everything the account page shows.
///
/// A struct rather than six positional arguments, because five of them are
/// `Option<&str>` and `bool` and a caller that swapped two would compile.
pub struct Account<'a> {
    /// In a round, as opposed to waiting for one.
    pub member: bool,
    /// `'email'`, `'telegram'` — the ways in that already exist.
    pub kinds: &'a [String],
    /// Scout's own Telegram chat, when the bot could name itself.
    pub chat_url: Option<&'a str>,
    /// Telegram's login button, for an account that has no Telegram
    /// identity yet. `None` also when we cannot name the bot.
    pub widget: Option<&'a str>,
    /// Bound to this account — see `session::csrf_for`.
    pub csrf: &'a str,
    /// How the last attempt to attach an identity went. One of this
    /// module's own sentences, chosen by the handler from a fixed set:
    /// nothing a visitor sent ever reaches this field, which is why it is
    /// not escaped.
    pub note: Option<&'a str>,
}

/// Where you stand, what proves it, and how to leave.
pub fn account(a: &Account) -> String {
    let note = match a.note {
        Some(n) => format!("  <p class=\"muted\">{n}</p>\n"),
        None => String::new(),
    };

    // Membership is the whole of the standing shown. No queue position:
    // `identity::Standing` cannot produce one — a revoked account has no
    // waitlist row either — and a number that is sometimes a lie is worse
    // than no number. See `SignIn::Queued`.
    let standing = if a.member {
        let chat = match a.chat_url {
            Some(url) => format!(
                "\n      <a class=\"btn quiet\" href=\"{url}\">Open Scout on Telegram</a>",
                url = escape(url)
            ),
            None => String::new(),
        };
        // This used to say Scout lives in Telegram, which stopped being
        // true the day `/chat` shipped. The link and the sentence are one
        // edit: an invitation to chat here, under a line saying the chat is
        // somewhere else, would read as a mistake.
        format!(
            r#"    <p>You're in. Ask Scout here or in Telegram — it is one conversation
       either way, so a question from a laptop carries on from a phone.</p>
    <p>
      <a class="btn" href="/chat">Open the chat</a>{chat}
    </p>"#
        )
    } else {
        r#"    <p>You're on the list. When a round opens with room in it, you're in —
       there is nothing else to do.</p>"#
            .to_string()
    };

    let has = |kind: &str| a.kinds.iter().any(|k| k == kind);
    let linked: String = a
        .kinds
        .iter()
        .map(|k| match k.as_str() {
            "email" => "email".to_string(),
            "telegram" => "Telegram".to_string(),
            // Never rendered today, and escaped anyway: the day a third
            // kind exists this must not be the line that has to be found.
            other => escape(other),
        })
        .collect::<Vec<_>>()
        .join(", ");

    // Only the missing one is offered. Offering both would invite somebody
    // to "link" the identity they are already signed in with, and be told
    // it is already theirs — a control whose only outcome is a shrug.
    let mut add = String::new();
    if !has("email") {
        add.push_str(&format!(
            r#"  <div class="card">
    <form method="post" action="/account/link/email">
      <input type="hidden" name="csrf" value="{csrf}">
      <label for="email">Add an email address</label>
      <input type="email" id="email" name="email" autocomplete="email"
             required placeholder="you@example.com">
      <button class="btn" type="submit">Email me a link</button>
    </form>
  </div>
"#,
            csrf = a.csrf
        ));
    }
    if !has("telegram") {
        if let Some(widget) = a.widget {
            add.push_str(&format!(
                r#"  <div class="card">
    <p>Add Telegram, and you can sign in either way.</p>
    {widget}
  </div>
"#
            ));
        }
    }

    shell(
        "Your account",
        &format!(
            r#"  <h1>Your account</h1>
{note}  <div class="card">
{standing}
    <p class="muted">Signed in with {linked}.</p>
  </div>
{add}  <form method="post" action="/sign-out">
    <input type="hidden" name="csrf" value="{csrf}">
    <button class="btn ghost" type="submit">Sign out</button>
  </form>"#,
            csrf = a.csrf
        ),
    )
}

/// The answer to every request for a link, whoever asked.
///
/// One page, one wording, no branch. Saying "we don't know that address"
/// would answer a question the form is not supposed to answer: whether
/// somebody has an account here.
pub fn check_your_inbox() -> String {
    shell(
        "Check your inbox",
        r#"  <h1>Check your inbox</h1>
  <div class="card">
    <p>If that address can sign in, a link is on its way. It works once and
       expires in 15 minutes.</p>
    <p class="muted">Nothing arrived? Check spam, then
       <a href="/sign-in">try again</a>.</p>
  </div>"#,
    )
}

/// The page the emailed link lands on.
///
/// A button rather than an automatic sign-in, because a `GET` here is not
/// necessarily a human: corporate mail scanners open every link in a
/// message before it is delivered. Signing in on `GET` would let the
/// scanner spend the token and leave the recipient reading "that link has
/// expired" — a failure that happens only to people at organisations that
/// scan, which is to say never on our own machines.
pub fn confirm(token: &str, csrf: &str) -> String {
    shell(
        "Confirm sign-in",
        &format!(
            r#"  <h1>Confirm sign-in</h1>
  <div class="card">
    <p>You asked to sign in to Scout. Confirm it was you.</p>
    <form method="post" action="/auth/email">
      <input type="hidden" name="csrf" value="{csrf}">
      <input type="hidden" name="t" value="{token}">
      <button class="btn" type="submit">Confirm sign-in</button>
    </form>
    <p class="muted">If you did not ask for this, close the page — nothing
       has happened to any account.</p>
  </div>"#,
            token = escape(token)
        ),
    )
}

/// Expired, unknown, or nonsense — all the same page.
///
/// Telling an unknown token apart from an expired one would confirm which
/// tokens have existed, which is a slow way of asking who has signed in.
pub fn link_dead() -> String {
    shell(
        "That link has expired",
        r#"  <h1>That link has expired</h1>
  <div class="card">
    <p>Sign-in links last 15 minutes and work once.</p>
    <p><a class="btn" href="/sign-in">Get a new link</a></p>
  </div>"#,
    )
}

/// Spent already — which usually means it worked.
pub fn link_already_used() -> String {
    shell(
        "That link has been used",
        r#"  <h1>That link has already been used</h1>
  <div class="card">
    <p>You may already be signed in.</p>
    <p><a class="btn" href="/account">Go to your account</a></p>
  </div>"#,
    )
}

/// A widget payload that did not verify.
///
/// A bad HMAC, a missing field and a replay of an hour-old sign-in all
/// render this. Naming which would tell somebody assembling a payload how
/// far they had got, and there is nothing an honest visitor could do with
/// the distinction anyway.
pub fn telegram_refused() -> String {
    shell(
        "That sign-in did not check out",
        r#"  <h1>That sign-in did not check out</h1>
  <div class="card">
    <p>Telegram sign-ins are only good for a minute. Try again.</p>
    <p><a class="btn" href="/sign-in">Back to sign in</a></p>
  </div>"#,
    )
}

/// A form whose token we did not mint, or minted too long ago.
///
/// One page for a forged `POST` and for a sign-in tab left open over
/// lunch, because they arrive identically and the honest instruction is
/// the same: start again.
pub fn stale_form() -> String {
    shell(
        "Start again",
        r#"  <h1>That form is out of date</h1>
  <div class="card">
    <p>Forms expire after 15 minutes.</p>
    <p><a class="btn" href="/sign-in">Start again</a></p>
  </div>"#,
    )
}

/// Something on our side broke. Says so, and says nothing else: what
/// failed is in the log, and the log is not a page.
pub fn sorry() -> String {
    shell(
        "Something went wrong",
        r#"  <h1>Something went wrong</h1>
  <div class="card">
    <p>That is on us, and it has been logged.</p>
    <p><a class="btn" href="/sign-in">Try again</a></p>
  </div>"#,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_that_is_markup_cannot_close_the_attribute_it_sits_in() {
        // The attack: mail somebody a link whose `t` is
        // `"><script>…</script>`, and the confirm page runs it.
        let page = confirm(r#""><script>alert(1)</script>"#, "c");
        assert!(!page.contains("<script>"), "a token escaped its attribute");
        assert!(page.contains("&quot;&gt;&lt;script&gt;"));
    }

    #[test]
    fn every_page_is_whole() {
        // `replace` on a token that is not there does nothing and says
        // nothing, so a shell that lost `__BODY__` would ship as an empty
        // page with no error anywhere.
        let kinds = ["email".to_string()];
        for page in [sign_in("c", None), sign_in("c", Some("<i>w</i>")), check_your_inbox(),
                     confirm("t", "c"), link_dead(), link_already_used(), stale_form(),
                     telegram_refused(), sorry(),
                     account(&Account { member: true, kinds: &kinds, chat_url: None,
                                        widget: None, csrf: "c", note: None })] {
            assert!(!page.contains("__BODY__"), "a page rendered with no body");
            assert!(!page.contains("__TITLE__"), "a page rendered with no title");
            assert!(page.contains("</html>"));
        }
    }

    const WIDGET: &str = r#"<script src="https://telegram.org/js/telegram-widget.js?22"></script>"#;

    fn kinds(of: &[&str]) -> Vec<String> {
        of.iter().map(|k| k.to_string()).collect()
    }

    /// A member who is in, with the bot's name known.
    fn member(kinds: &[String]) -> Account<'_> {
        Account {
            member: true,
            kinds,
            chat_url: Some("https://t.me/scoutbot"),
            widget: Some(WIDGET),
            csrf: "c",
            note: None,
        }
    }

    #[test]
    fn the_account_page_offers_only_the_way_in_that_is_missing() {
        // Offering both would invite somebody to attach the identity they
        // are signed in with and be told it is already theirs — a control
        // whose only outcome is a shrug.
        let email = kinds(&["email"]);
        let page = account(&member(&email));
        assert!(page.contains("telegram-widget.js"), "no way to add Telegram");
        assert!(
            !page.contains("/account/link/email"),
            "offered to add the address they signed in with"
        );

        let both = kinds(&["email", "telegram"]);
        let page = account(&member(&both));
        assert!(!page.contains("telegram-widget.js"));
        assert!(!page.contains("/account/link/email"));

        let telegram = kinds(&["telegram"]);
        let page = account(&member(&telegram));
        assert!(page.contains(r#"action="/account/link/email""#), "no way to add an address");
        assert!(!page.contains("telegram-widget.js"));

        // And no widget at all when the bot could not name itself, rather
        // than a button that signs people into nothing.
        let page = account(&Account { widget: None, ..member(&email) });
        assert!(!page.contains("telegram-widget.js"));
    }

    #[test]
    fn a_member_is_shown_the_chat_and_someone_queued_is_not() {
        // Telegram is where Scout is actually used until there is a web
        // chat, so a member who is not told that has been signed in to
        // nothing they can see.
        let email = kinds(&["email"]);
        let page = account(&member(&email));
        assert!(page.contains("https://t.me/scoutbot"), "a member was not shown the chat");

        // Queued: no link, because there is nothing behind it for them
        // yet. And no queue position, which `Standing` cannot produce.
        let waiting = account(&Account { member: false, ..member(&email) });
        assert!(!waiting.contains("t.me"), "someone still queued was pointed at the bot");
        for leak in ["position", "ahead of you", "seats"] {
            assert!(!waiting.contains(leak), "the page invented `{leak}`");
        }

        // And with no bot name — `getMe` failed at start-up — a member is
        // told where they stand and offered no link at all, rather than a
        // link to nowhere.
        let nameless = account(&Account { chat_url: None, ..member(&email) });
        assert!(!nameless.contains("t.me"));
        assert!(nameless.contains("You're in"));
    }

    #[test]
    fn both_forms_on_the_account_page_carry_the_session_bound_token() {
        // The token is minted by `session::csrf_for` for this account.
        // A form that shipped without one would be refused, which someone
        // would notice; a form that shipped with the *anonymous* token
        // would work, and that is the hole binding closes.
        let telegram = kinds(&["telegram"]);
        let page = account(&Account { csrf: "TOKEN", ..member(&telegram) });
        assert_eq!(
            page.matches(r#"name="csrf" value="TOKEN""#).count(),
            2,
            "sign out and add-an-address must both carry the form token"
        );
    }

    #[test]
    fn a_link_outcome_is_a_sentence_of_ours_and_the_bot_name_cannot_escape() {
        // `note` is chosen by the handler from a fixed set, never echoed
        // from the query string — but the widget's bot name comes from
        // `getMe`, which is a value someone else controls.
        let email = kinds(&["email"]);
        let page = account(&Account { note: Some("Telegram is linked."), ..member(&email) });
        assert!(page.contains("Telegram is linked."));

        let widget = telegram_widget(r#""><script>alert(1)</script>"#, "https://example.com/a");
        assert!(!widget.contains("<script>alert"), "a bot name escaped its attribute");
        assert!(widget.contains("&quot;&gt;&lt;script&gt;"));
    }

    #[test]
    fn the_dead_link_page_names_neither_expiry_nor_ignorance() {
        // Both an expired token and one that never existed render this, so
        // the wording must not commit to either. If someone "improves" it
        // to say "we have no record of that link", they have built the
        // oracle this page exists to avoid.
        let page = link_dead();
        assert!(!page.to_lowercase().contains("no record"));
        assert!(!page.to_lowercase().contains("unknown"));
    }

    #[test]
    fn a_member_is_offered_the_web_chat() {
        // Without a link the feature is unreachable — `/chat` is not
        // advertised anywhere else.
        let page = account(&member(&kinds(&["telegram"])));
        assert!(page.contains(r#"href="/chat""#), "no way in: {page}");
    }

}
