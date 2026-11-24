#!/bin/sh
# A stand-in for Chrome, for the test of what a render makes of Chrome's own
# error page (`tools/browser.rs`). It prints what `--dump-dom` prints when the
# page a render was sent on to could not be loaded: Chrome's error page, in
# the shape measured on 2026-11-24 with Chrome 158 for a page that sent
# itself on by script or by a meta refresh to a port nothing answered on.
# Then it ends.
cat <<'PAGE'
<html dir="ltr" lang="en"><head><title>shop.example</title></head><body class="neterror" id="t"><div id="main-frame-error" class="interstitial-wrapper"><div id="main-content"><div class="icon icon-generic"></div><div id="main-message"><h1><span>This site can’t be reached</span></h1><p><span><strong>shop.example</strong> refused to connect.</span></p><div class="error-code">ERR_CONNECTION_REFUSED</div></div></div></div></body></html>
PAGE
