//! The small pages the signed-in half renders.
//!
//! Strings and `replace`, like `page.rs`, and for the same reason: a
//! template engine to interpolate three values is a dependency to keep
//! patched forever.

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_page_is_whole() {
        // `replace` on a token that is not there does nothing and says
        // nothing, so a shell that lost `__BODY__` would ship as an empty
        // page with no error anywhere.
        let page = sign_in("c");
        assert!(!page.contains("__BODY__"), "a page rendered with no body");
        assert!(!page.contains("__TITLE__"), "a page rendered with no title");
        assert!(page.contains("</html>"));
    }
}
