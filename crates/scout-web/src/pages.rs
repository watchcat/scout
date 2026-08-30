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
pub fn sign_in(csrf: &str) -> String {
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
  </div>"#
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
        for page in [sign_in("c"), check_your_inbox(), confirm("t", "c"), link_dead(),
                     link_already_used(), stale_form(), sorry()] {
            assert!(!page.contains("__BODY__"), "a page rendered with no body");
            assert!(!page.contains("__TITLE__"), "a page rendered with no title");
            assert!(page.contains("</html>"));
        }
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

}
