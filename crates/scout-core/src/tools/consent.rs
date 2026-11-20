//! The consent dialog a page opens over itself.
//!
//! `fetch_page` keeps the first 6,000 characters of a page's text and its
//! first thirty links, counted from the top of the document. A consent
//! dialog written in at the top of the body takes both. Of ten shops
//! rendered on 2026-11-18, two came back as the cookie notice
//! (`docs/2026-11-18-shop-reach.md`). Rendered again on 2026-11-20, with
//! the same flags and from a laptop, both carried Cookiebot's dialog as
//! the first element of the body: 11,435 characters of text on one and
//! 20,854 on the other, every cookie it knows of named and described
//! before a word of the shop's, and 6 and 28 of the thirty links kept
//! were the dialog's. amazon.nl's pages open the same way with a notice
//! of the shop's own, 2,209 characters and ten links. zooplus.nl and
//! coolblue.nl carry a dialog as well, at the end of the body, after
//! the page, where the cut did not reach it.
//!
//! So a dialog is read where those two put it: after the page. It is
//! moved and not taken out, because the rule that knows a dialog can be
//! wrong, and a block it mistakes then loses its place and not its
//! words; because a page that is nothing but its consent gate still
//! reads as one, and not as a page with nothing on it; and because the
//! refusal rule (`refusal.rs`) goes by how much text there is and what
//! it says, which a move leaves as it was.
//!
//! What it does not know: a dialog with neither of the marks below on
//! its own tag. Such a one is read where it stands, as before.

use super::fetch::{collapse_ws, decode_entities, strip_tags};
use std::borrow::Cow;

/// What a consent dialog's own tag said of it in the renders above:
/// `data-nosnippet`, which asks a search engine to leave the block out of
/// its summary of the page (Cookiebot's on two hosts, OneTrust's on
/// zooplus.nl), and the role of a dialog (amazon.nl's form). Neither is a
/// vendor's name for itself. Each marks other things as well — the menus
/// in amazon.nl's header and zooplus.nl's basket drawer are dialogs — so
/// a mark says only where a block begins, and what the block says
/// decides.
const MARKS: [&str; 2] = ["data-nosnippet", "role=\"dialog\""];

/// How soon a marked block has to say "cookie" to be the consent dialog.
/// The four seen say it within 75 characters of their text: amazon.nl's
/// at once, OneTrust's after a heading (45), Cookiebot's after the names
/// of its tabs (58 and 74). It is the same word in the Dutch and the
/// English they were written in. 200 is a choice, not a measurement:
/// room for a longer heading than those, and short of a block that is
/// about something else and comes to cookies in its small print.
const SAID_WITHIN_CHARS: usize = 200;

/// The document with every consent dialog moved to its end, so that the
/// page is read first and the dialog with whatever room is left. A
/// document with no such dialog is handed back as it is.
///
/// `html` is a document already rid of its scripts and styles: a tag
/// inside a script would be counted with the rest.
pub(super) fn read_last(html: &str) -> Cow<'_, str> {
    let lower = html.to_ascii_lowercase();
    let dialogs = dialogs(html, &lower);
    if dialogs.is_empty() {
        return Cow::Borrowed(html);
    }
    let mut page = String::with_capacity(html.len() + dialogs.len());
    let mut from = 0;
    for &(start, end) in &dialogs {
        page.push_str(&html[from..start]);
        // The words on either side were two words with a tag between
        // them, and stay two.
        page.push(' ');
        from = end;
    }
    page.push_str(&html[from..]);
    for &(start, end) in &dialogs {
        page.push_str(&html[start..end]);
    }
    Cow::Owned(page)
}

/// Where each consent dialog starts and ends, in the order they stand.
/// One inside another is the outer one's.
fn dialogs(html: &str, lower: &str) -> Vec<(usize, usize)> {
    let mut marks: Vec<(usize, usize)> = MARKS
        .iter()
        .flat_map(|mark| lower.match_indices(mark).map(|(at, _)| (at, at + mark.len())))
        .collect();
    marks.sort_unstable();
    let mut found: Vec<(usize, usize)> = Vec::new();
    for (at, after) in marks {
        if found.last().is_some_and(|&(_, end)| at < end) {
            continue;
        }
        let Some(start) = opening_tag(lower, at, after) else { continue };
        let Some(end) = element_end(lower, start) else { continue };
        if opens_on_cookies(&html[start..end]) {
            found.push((start, end));
        }
    }
    found
}

/// Where the opening tag begins that carries the mark at `at..after` as
/// an attribute of its own. Nothing when the mark is text between tags,
/// or a piece of a longer name: `data-role="dialog"` is another
/// attribute.
fn opening_tag(lower: &str, at: usize, after: usize) -> Option<usize> {
    let start = lower[..at].rfind('<')?;
    let tag = &lower[start + 1..at];
    let opens = tag.starts_with(|c: char| c.is_ascii_alphabetic()) && !tag.contains('>');
    let whole = tag.ends_with(|c: char| c.is_ascii_whitespace())
        && !lower[after..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    (opens && whole).then_some(start)
}

/// Where the element that opens at `start` ends: its own tag name,
/// counted open and closed until the two balance. A render is Chrome's
/// own writing of the DOM and always balances. Markup that does not —
/// an element with no closing tag, a page cut short — has no end, and
/// then nothing is moved.
fn element_end(lower: &str, start: usize) -> Option<usize> {
    let name = &lower[start + 1..];
    let name = &name[..name.find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))?];
    let mut depth = 0usize;
    let mut from = start;
    while let Some(at) = lower[from..].find('<').map(|i| i + from) {
        from = at + 1;
        let rest = &lower[from..];
        let (closing, rest) = match rest.strip_prefix('/') {
            Some(rest) => (true, rest),
            None => (false, rest),
        };
        // "<div" has to end the tag name: `<divider>` is another element.
        let Some(after) = rest.strip_prefix(name) else { continue };
        if !after.starts_with(|c: char| c == '>' || c == '/' || c.is_ascii_whitespace()) {
            continue;
        }
        if !closing {
            depth += 1;
            continue;
        }
        // Never below one here: the tag at `start` opened it, and the
        // close that brings it to nought is the end.
        depth -= 1;
        if depth == 0 {
            return lower[at..].find('>').map(|i| at + i + 1);
        }
    }
    None
}

/// Whether a block's text speaks of cookies from its start.
fn opens_on_cookies(block: &str) -> bool {
    let text = collapse_ws(&decode_entities(&strip_tags(block)));
    let opening: String = text.chars().take(SAID_WITHIN_CHARS).collect();
    opening.to_lowercase().contains("cookie")
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Renders in the shape of the ones above. The consent markup is the
    //! vendor's or amazon.nl's; the shops sell widgets.

    /// What Cookiebot's dialog says in one language.
    pub(crate) struct Wording {
        tabs: [&'static str; 3],
        heading: &'static str,
        lead: &'static str,
        provider_link: &'static str,
        buttons: [&'static str; 2],
    }

    /// As on the first of the two hosts.
    pub(crate) const DUTCH: Wording = Wording {
        tabs: ["Toestemming", "Details", "Over"],
        heading: "Deze website maakt gebruik van cookies",
        lead: "We gebruiken cookies om de winkel te laten werken en om bezoek te tellen.",
        provider_link: "Meer informatie over deze aanbieder",
        buttons: ["Weigeren", "Alles toestaan"],
    };

    /// As on the second.
    pub(crate) const ENGLISH: Wording = Wording {
        tabs: ["Consent", "Details", "About"],
        heading: "This website uses cookies",
        lead: "We use cookies to make the shop work and to count its visitors.",
        provider_link: "Learn more about this provider",
        buttons: ["Deny", "Allow all"],
    };

    /// Cookiebot's dialog as two hosts of ten carried it in a render on
    /// 2026-11-20, in shape: one element under the vendor's ids, a logo
    /// that is all attribute, the names of its tabs, a heading and a
    /// sentence, then every cookie it knows of under the provider that
    /// sets it, each provider with a link out, and the buttons last.
    /// `template` is the vendor's name for the layout: "popup" on one
    /// host, "overlay" on the other.
    pub(crate) fn cookiebot(template: &str, says: &Wording, providers: usize, cookies_each: usize) -> String {
        let cookies = |provider: usize| -> String {
            (0..cookies_each)
                .map(|cookie| {
                    format!(
                        "<div class=\"CybotCookiebotDialogDetailBodyContentCookieInfo\">\
                         <strong role=\"heading\" aria-level=\"4\">_widget_{provider}_{cookie}</strong>\
                         <span>Registers a unique ID that is used to generate statistical data on how \
                         the visitor uses the website.</span>\
                         <div class=\"CybotCookiebotDialogDetailBodyContentCookieInfoFooter\">\
                         <span><b>Maximum Storage Duration</b>: 1 year</span>\
                         <span><b>Type</b>: HTTP Cookie</span></div></div>"
                    )
                })
                .collect()
        };
        let groups: String = (0..providers)
            .map(|provider| {
                format!(
                    "<li class=\"CybotCookiebotDialogDetailBodyContentCookieGroup\">\
                     <a class=\"CybotCookiebotDialogDetailBodyContentCookieProvider\" role=\"button\" \
                     href=\"#\">Provider {provider}<div class=\"CybotCookiebotDialogDetailBulkConsentCount\">\
                     {cookies_each}</div></a>\
                     <a class=\"CybotCookiebotDialogDetailBodyContentCookieLink\" target=\"_blank\" \
                     href=\"https://provider-{provider}.example/privacy\">{}</a>\
                     <div class=\"CybotCookiebotDialogDetailBodyContentCookieInfoWrapper \
                     CybotCookiebotDialogHide\">{}</div></li>",
                    says.provider_link,
                    cookies(provider)
                )
            })
            .collect();
        let [consent, details, about] = says.tabs;
        let [deny, allow] = says.buttons;
        format!(
            "<div id=\"CybotCookiebotDialog\" name=\"CybotCookiebotDialog\" role=\"region\" \
             aria-labelledby=\"CybotCookiebotDialogBodyContentTitle\" tabindex=\"-1\" dir=\"ltr\" \
             data-nosnippet=\"\" ng-non-bindable=\"\" class=\"CybotCookiebotDialogActive\" \
             style=\"display: flex; font-size: 0.9375rem;\" data-template=\"{template}\">\
             <div class=\"CybotCookiebotDialogContentWrapper\"><div id=\"CybotCookiebotDialogHeader\">\
             <div id=\"CybotCookiebotDialogPoweredbyLink\"><img id=\"CybotCookiebotDialogPoweredbyImage\" \
             src=\"data:image/png;base64,iVBORw0KGgo=\" alt=\"logo\" width=\"46\" height=\"34\"></div>\
             <a href=\"https://consent-manager.example/powered-by\" target=\"_blank\" tabindex=\"-1\">\
             <svg viewBox=\"0 0 843 248\" aria-hidden=\"true\"><path d=\"M249 182\"></path></svg></a></div>\
             <div id=\"CybotCookiebotDialogNav\"><ul id=\"CybotCookiebotDialogNavList\" role=\"tablist\">\
             <li><a href=\"#\" role=\"tab\">{consent}</a></li><li><a href=\"#\" role=\"tab\">{details}</a></li>\
             <li class=\"CybotCookiebotDialogHide\"><a href=\"#\" role=\"tab\">[#IABV2SETTINGS#]</a></li>\
             <li><a href=\"#\" role=\"tab\">{about}</a></li></ul></div>\
             <div id=\"CybotCookiebotDialogTabContent\"><div id=\"CybotCookiebotDialogBody\" role=\"tabpanel\">\
             <div id=\"CybotCookiebotDialogBodyContentTitle\" role=\"heading\" aria-level=\"2\">{}</div>\
             <div id=\"CybotCookiebotDialogBodyContentText\">{}</div></div>\
             <div id=\"CybotCookiebotDialogTabContentDetails\" role=\"tabpanel\">\
             <ul id=\"CybotCookiebotDialogDetailBodyContentCookieContainerTypes\">{groups}</ul></div>\
             <div id=\"CybotCookiebotDialogDetailBodyContentTextAbout\" role=\"tabpanel\">\
             <a href=\"https://consent-manager.example/\" target=\"_blank\">Cookiebot</a></div></div>\
             <div id=\"CybotCookiebotDialogFooter\"><div id=\"CybotCookiebotDialogBodyButtonsWrapper\">\
             <button id=\"CybotCookiebotDialogBodyButtonDecline\">{deny}</button> \
             <button id=\"CybotCookiebotDialogBodyLevelButtonLevelOptinAllowAll\">{allow}</button>\
             </div></div></div></div>",
            says.heading, says.lead
        )
    }

    /// A product page as the first of the two hosts serves it, in shape: a
    /// bar and a menu, the product with its particulars and a price that
    /// is in its text and not in its data, then the other models. More
    /// than the 6,000 characters kept and more than thirty links.
    /// `opening` is what a render has first in its body.
    pub(crate) fn unpriced_page(opening: &str) -> String {
        let menu: String = ["widgets", "gadgets", "parts", "sale", "new", "about", "service", "contact", "returns", "account"]
            .iter()
            .map(|item| format!("<li><a href=\"/{item}/\">{item}</a></li>"))
            .collect();
        let others: String = (0..40)
            .map(|n| format!("<li><a href=\"/product/widget-{n}/\">Widget {n}</a> Blue <span>49,95</span></li>"))
            .collect();
        format!(
            "<!DOCTYPE html><html lang=\"nl\"><head><title>Widget W-100 Blue</title>\
             <style>.navbar{{top:0}}</style></head>\n\n<body class=\"single-product\">{opening}\n\n\
             <div class=\"site-top-bar\"><div class=\"container\">Free delivery on every widget. \
             <a href=\"/service/delivery/\">How it works</a></div></div>\n\
             <nav class=\"navbar\"><ul>{menu}</ul></nav>\n\
             <main><nav class=\"breadcrumb\"><a href=\"/\">Home</a> <a href=\"/product-category/widgets/\">\
             Widgets</a> W-100</nav><h1>Widget W-100</h1>\
             <dl><dt>Model number</dt><dd>W-100</dd><dt>Colour</dt><dd>Blue, Black</dd>\
             <dt>Available in sizes</dt><dd>S, M, L</dd><dt>Material</dt><dd>Steel</dd></dl>\
             <p class=\"price\">€ 49,95</p><p>{}</p><h2>Other models</h2><ul>{others}</ul></main>\n\
             <footer><a href=\"/privacy/\">Privacy</a> <a href=\"/service/terms/\">Terms</a></footer>\
             <script>window.dataLayer = [];</script></body></html>",
            "A widget for every purpose, in three sizes. ".repeat(130)
        )
    }

    /// A product page as the second serves it, in shape: priced by its own
    /// data, shorter than the 6,000 characters kept, and ending in the
    /// shop platform's own cookie bar and an empty picture viewer, which
    /// the plain answer has as the render does. The bar is a region, not
    /// a dialog, and nobody has marked it.
    pub(crate) fn priced_page(opening: &str) -> String {
        let menu: String = ["new", "small", "large", "spare", "sale"]
            .iter()
            .map(|item| format!("<a href=\"/{item}\">{item} widgets</a> "))
            .collect();
        format!(
            "<!DOCTYPE html><html lang=\"en\"><head><title>Blue widget W-200 | Shop</title>\
             <script type=\"application/ld+json\">{{\"@type\":\"Product\",\"name\":\"Blue widget\",\
             \"offers\":{{\"@type\":\"Offer\",\"price\":\"12.95\",\"priceCurrency\":\"EUR\",\
             \"availability\":\"https://schema.org/InStock\"}}}}</script></head>\n\
             <body class=\"is-ctl-product\" style=\"overflow: hidden;\">{opening}\n\n\
             <div id=\"page-top\" class=\"skip-to-content\"><a href=\"#content-main\">Skip to main content</a></div>\
             <header><a href=\"/delivery\">Free delivery from 40 €</a> <a href=\"/returns\">14 days to \
             return</a><nav>{menu}</nav></header>\
             <main id=\"content-main\"><h1>Blue widget, cordless</h1>\
             <p class=\"product-detail-price\">12,95 €</p>\
             <p class=\"delivery-information\">In stock, with you in 2 to 4 working days</p><p>{}</p></main>\
             <footer><a href=\"/contact\">Contact</a> <a href=\"/payment\">Payment</a> \
             <a href=\"/about\">About us</a></footer>\
             <div class=\"cookie-permission-container\" data-cookie-permission=\"true\" role=\"region\" \
             tabindex=\"-1\" aria-label=\"Cookie preferences\" style=\"display: block;\">\
             <div class=\"container\"><div class=\"row\"><div class=\"col cookie-permission-content\">This \
             website uses cookies to give you the best experience it can. <a href=\"/page/cookies\">More \
             information ...</a></div><div class=\"col-12\"><button type=\"submit\">Only technically \
             required</button> <button type=\"submit\">Configure</button></div></div></div></div>\
             <div class=\"js-pseudo-modal-template\"><div class=\"modal fade\" tabindex=\"-1\" \
             role=\"dialog\" aria-modal=\"true\" aria-hidden=\"true\" aria-labelledby=\"pseudo-modal-title\">\
             <div class=\"modal-dialog\"><div class=\"modal-content\"><button>Back</button></div></div>\
             </div></div></body></html>",
            "Runs for six hours on one charge. ".repeat(80)
        )
    }

    /// amazon.nl's own notice as its pages carried it in a render on
    /// 2026-11-20, in shape: a form that calls itself a dialog, inside a
    /// wrapper with its styles, a heading, a few paragraphs with a link
    /// in each, an alert and the buttons. Its heading is the shop's;
    /// the rest is not its wording.
    pub(crate) fn shops_own_notice() -> String {
        format!(
            "<div id=\"sp-cc-wrapper\"><style>#cos-banner{{position:fixed;z-index:1000}}</style>\n\
             <form method=\"post\" role=\"dialog\" aria-labelledby=\"sp-cc-header\" \
             action=\"/privacyprefs/banner\" id=\"cos-banner\"><input type=\"hidden\" name=\"locale\" \
             value=\"en-GB\">\n<div class=\"a-section sp-cc-text\"><h4 id=\"sp-cc-header\">Cookies and \
             Advertising Choices</h4>{}<div id=\"sp-cc-error\" class=\"aok-hidden\" role=\"alert\">\
             <div class=\"a-box-inner\"><div class=\"a-alert-content\">There was a problem saving your \
             cookie preferences.</div></div></div></div>\
             <div class=\"a-section sp-cc-buttons-container\"><div class=\"a-section sp-cc-buttons\">\
             <span class=\"a-button\"><input id=\"sp-cc-accept\" name=\"action\" type=\"submit\" \
             value=\"Accept\"><span aria-hidden=\"true\">Accept</span></span>\
             <span class=\"a-button\"><input id=\"sp-cc-rejectall-link\" name=\"action\" type=\"submit\" \
             value=\"Decline\"><span aria-hidden=\"true\">Decline</span></span>\
             <a id=\"sp-cc-customize\" href=\"/privacyprefs/customize\">Customise</a></div></div></form></div>",
            "<p>If you agree, we also use cookies to show you ads on this and on other services, as \
             our <a href=\"/help/cookie-notice\">Cookie notice</a> describes. You can change your \
             choice at any time, and what you choose here holds for this browser only.</p>"
                .repeat(9)
        )
    }

    /// A product page as amazon.nl's render carried it, in shape:
    /// `opening` first in the element that holds the page, then the skip
    /// links and a header whose menus are dialogs too, one of them with
    /// words in it, then the product. More than the 6,000 characters
    /// kept and more than thirty links.
    pub(crate) fn page_with_flyouts(opening: &str) -> String {
        let departments: String =
            (0..34).map(|n| format!("<a href=\"/b?node={n}\">Widgets {n}</a> ")).collect();
        format!(
            "<!DOCTYPE html><html lang=\"en-gb\"><head><title>Widget, 3rd generation : Shop</title></head>\n\
             <body class=\"a-m-nl\"><div id=\"a-page\"><script type=\"a-state\">{{}}</script>\n{opening}\
             <a id=\"nav-top\"></a><nav id=\"shortcut-menu\" role=\"navigation\"><h2>Skip to</h2>\
             <ul><li><a href=\"#skippedLink\">Main content</a></li></ul></nav>\
             <header><a href=\"/ref=nav_logo\">.nl</a> <a href=\"/gp/cart\">Basket</a>\
             <div id=\"nav-flyout-searchAjax\" class=\"nav-flyout\" aria-modal=\"false\" role=\"dialog\"></div>\
             <div id=\"nav-flyout-accountList\" class=\"nav-flyout\" aria-modal=\"false\" role=\"dialog\" \
             aria-label=\"Account settings\">Sign in New customer? <a href=\"/register\">Start here.</a> \
             Your Orders</div><div id=\"nav-flyout-prime\" class=\"nav-flyout\" aria-modal=\"false\" \
             role=\"dialog\"></div><div id=\"nav-main\">{departments}</div></header>\
             <div id=\"dp\"><h1 id=\"title\">Widget, 3rd generation</h1><span class=\"a-price\">€24.99</span>\
             <div id=\"availability\">In stock</div><button>Add to basket</button><p>{}</p></div>\
             </div></body></html>",
            "A widget with a clearer sound, in charcoal. ".repeat(140)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::read_last;
    use crate::tools::fetch::{self, PageContent, PageLink};
    use reqwest::Url;
    use std::borrow::Cow;

    fn url() -> Url {
        Url::parse("https://shop.example/product/widget-w-100/").unwrap()
    }

    fn read(html: &str) -> PageContent {
        fetch::extract_page(html, &url())
    }

    /// A page's text, whether it was cut, and its links, as `extract_page`
    /// made them before a dialog was moved: the document as it stands,
    /// top to bottom. Kept apart from the code so that "as before" is not
    /// whatever the code now does.
    fn read_as_before(html: &str) -> (String, bool, Vec<PageLink>) {
        let without_scripts = fetch::strip_tag_blocks(&fetch::strip_tag_blocks(html, "script"), "style");
        let links = fetch::extract_links(&without_scripts, &url());
        let full = fetch::collapse_ws(&fetch::decode_entities(&fetch::strip_tags(&without_scripts)));
        let cut = full.chars().count() > fetch::MAX_TEXT_CHARS;
        (full.chars().take(fetch::MAX_TEXT_CHARS).collect(), cut, links)
    }

    /// How many of the links kept lead out to a cookie's provider or to
    /// the consent manager.
    fn the_dialogs(links: &[PageLink]) -> usize {
        links.iter().filter(|link| link.url.contains("provider-") || link.url.contains("consent-manager")).count()
    }

    /// The start of a text, for a failure to show.
    fn start(text: &str) -> String {
        text.chars().take(160).collect()
    }

    #[test]
    fn the_fixtures_are_the_defect_as_it_was_seen() {
        // Or the tests below would pass for the wrong reason. Read as
        // before, each render is its dialog and nothing of the shop.
        let (text, cut, links) = read_as_before(&unpriced_page(&cookiebot("popup", &DUTCH, 5, 12)));
        assert!(
            text.starts_with("Widget W-100 Blue Toestemming Details [#IABV2SETTINGS#] Over Deze website maakt gebruik van cookies"),
            "got: {}",
            start(&text)
        );
        assert!(cut && !text.contains("Model number") && !text.contains("49,95"));
        assert_eq!(the_dialogs(&links), 6, "six of the thirty, as on that host");

        let (text, cut, links) = read_as_before(&priced_page(&cookiebot("overlay", &ENGLISH, 27, 4)));
        assert!(
            text.starts_with("Blue widget W-200 | Shop Consent Details [#IABV2SETTINGS#] About This website uses cookies"),
            "got: {}",
            start(&text)
        );
        assert!(cut && !text.contains("cordless") && !text.contains("12,95"));
        assert_eq!(the_dialogs(&links), 28, "twenty-eight of the thirty, as on that one");

        // amazon.nl's is shorter than the cut: a third of what is kept.
        let (text, _, links) = read_as_before(&page_with_flyouts(&shops_own_notice()));
        assert!(text.starts_with("Widget, 3rd generation : Shop Cookies and Advertising Choices"), "got: {}", start(&text));
        let page_begins = text.find("Skip to Main content").expect("the page is reached");
        assert!((1500..3000).contains(&page_begins), "the notice is {page_begins} characters");
        assert_eq!(links.iter().filter(|link| link.url.contains("cookie") || link.url.contains("privacyprefs")).count(), 10);
    }

    #[test]
    fn a_render_that_opens_with_the_dialog_reads_as_the_page_without_it() {
        // The first of the two hosts, in shape: Cookiebot's dialog in
        // Dutch, some 10,000 characters of it, written in as the first
        // element of the body of a page the plain request had read.
        let plain = read(&unpriced_page(""));
        let render = read(&unpriced_page(&cookiebot("popup", &DUTCH, 5, 12)));
        assert!(
            render.text.starts_with("Widget W-100 Blue Free delivery on every widget. How it works widgets gadgets"),
            "got: {}",
            start(&render.text)
        );
        assert!(render.text.contains("Model number W-100 Colour Blue, Black Available in sizes S, M, L"));
        assert!(render.text.contains("€ 49,95"));
        // The page fills the 6,000 by itself, so none of the dialog is left in them.
        assert!(render.truncated && !render.text.to_lowercase().contains("cookie"));
        assert_eq!(render.text, plain.text);
        assert_eq!(
            render.links[0],
            PageLink { url: "https://shop.example/service/delivery/".into(), text: "How it works".into() }
        );
        assert_eq!(render.links, plain.links);
    }

    #[test]
    fn a_page_shorter_than_the_cut_is_read_whole_and_the_dialog_after_it() {
        // The second, in shape: the same vendor's other layout, in
        // English, 20,000 characters of it with an empty underlay
        // beside it, on a page priced by its own data.
        let opening = cookiebot("overlay", &ENGLISH, 27, 4) + "<div id=\"CybotCookiebotDialogBodyUnderlay\"></div>";
        let plain = read(&priced_page(""));
        let render = read(&priced_page(&opening));
        assert_eq!(render.product.as_ref().and_then(|p| p.price.as_deref()), Some("12.95 EUR"));
        assert_eq!(render.product, plain.product);

        assert!(
            render.text.starts_with("Blue widget W-200 | Shop Skip to main content Free delivery from 40 €"),
            "got: {}",
            start(&render.text)
        );
        assert!(render.text.contains("Blue widget, cordless 12,95 € In stock, with you in 2 to 4 working days"));
        // All of the page as the plain request read it, its own cookie
        // bar where the shop put it, and only then the dialog.
        assert!(!plain.truncated);
        let after = render.text.strip_prefix(plain.text.as_str()).expect("the render begins with the whole page");
        assert!(after.starts_with(" Consent Details [#IABV2SETTINGS#] About This website uses cookies"), "got: {}", start(after));
        assert!(render.truncated);
        assert_eq!(render.text.chars().count(), fetch::MAX_TEXT_CHARS);

        // The page's links before any of the dialog's.
        assert!(plain.links.len() < 30 && the_dialogs(&plain.links) == 0);
        assert!(render.links.starts_with(&plain.links), "got: {:?}", render.links);
        assert_eq!(render.links[plain.links.len()].url, "https://provider-0.example/privacy");
    }

    #[test]
    fn a_shops_own_notice_that_calls_itself_a_dialog_is_read_after_the_page_as_well() {
        // amazon.nl: no consent manager's markup, a form with the role
        // of a dialog. The menus in its header are dialogs too and say
        // nothing of cookies: they stay where they are.
        let without = read(&page_with_flyouts(""));
        let render = read(&page_with_flyouts(&shops_own_notice()));
        assert!(
            render.text.starts_with("Widget, 3rd generation : Shop Skip to Main content .nl Basket Sign in New customer?"),
            "got: {}",
            start(&render.text)
        );
        assert!(render.text.contains("Widget, 3rd generation €24.99 In stock Add to basket"));
        assert!(!render.text.contains("Advertising Choices"));
        assert_eq!(render.text, without.text);
        assert_eq!(render.links, without.links);
    }

    #[test]
    fn a_dialog_the_document_puts_last_is_read_where_it_is() {
        // zooplus.nl, in shape: OneTrust's dialog as the last element of
        // the body, after the page. Nothing was wrong with that.
        let onetrust = "<div id=\"onetrust-consent-sdk\" data-nosnippet=\"true\"><div class=\"onetrust-pc-dark-filter\"></div>\
            <div id=\"onetrust-banner-sdk\" tabindex=\"0\"><div class=\"ot-sdk-container\" role=\"dialog\" aria-modal=\"true\">\
            <h2 id=\"onetrust-policy-title\">Help us make the shop better!</h2><div id=\"onetrust-policy-text\">We use \
            cookies and other technologies on our website. <a href=\"/legal/privacy\">Privacy statement</a></div>\
            <button id=\"onetrust-reject-all-handler\">Only necessary cookies</button> \
            <button id=\"onetrust-accept-btn-handler\">Agree and continue</button></div></div></div>";
        for page in [unpriced_page(""), priced_page("")] {
            let html = page.replace("</body>", &format!("{onetrust}</body>"));
            assert!(matches!(read_last(&html), Cow::Owned(_)), "it is known for a dialog");
            let (text, cut, links) = read_as_before(&html);
            let page = read(&html);
            assert_eq!((page.text, page.truncated, page.links), (text, cut, links));
        }
    }

    #[test]
    fn a_page_with_no_dialog_reads_as_it_did() {
        let basket_drawer = "<div class=\"z-modal\" role=\"dialog\" aria-modal=\"true\" aria-labelledby=\"h\">\
            <h2 id=\"h\">Added to your basket</h2>Add € 29,00 more for free delivery <a href=\"/basket\">To the basket</a></div>";
        let not_for_snippets = "<div data-nosnippet>Prices include VAT. <a href=\"/delivery\">Delivery</a> is extra.</div>";
        for (what, html) in [
            ("a product page", unpriced_page("")),
            ("one priced by its data, with a cookie bar nobody marked", priced_page("")),
            ("one whose menus are dialogs", page_with_flyouts("")),
            ("a dialog that is not about cookies, first in the body", unpriced_page(basket_drawer)),
            ("a block kept out of snippets that is the shop's own words", priced_page(not_for_snippets)),
            // A banner with no mark on it is not known for a dialog, and
            // is read where it stands, as it was.
            (
                "a cookie banner nobody marked",
                unpriced_page("<div id=\"cookie-banner\">We use cookies. <a href=\"/cookies\">Read more</a></div>"),
            ),
            (
                "a page about cookies",
                "<html><head><title>Cookies</title></head><body><h1>Cookies</h1><p>Our cookie policy.</p></body></html>".to_string(),
            ),
        ] {
            assert!(matches!(read_last(&html), Cow::Borrowed(_)), "{what}: the document itself is handed back");
            let (text, cut, links) = read_as_before(&html);
            let page = read(&html);
            assert_eq!(page.text, text, "{what}");
            assert_eq!(page.truncated, cut, "{what}");
            assert_eq!(page.links, links, "{what}");
        }
    }

    #[test]
    fn a_page_that_is_only_its_consent_gate_reads_as_it_did() {
        // Taken out, the dialog would leave a title and nothing: a page
        // with nothing on it, which a model takes for a page and opens
        // the next one like it. Moved, the gate is still what is read.
        let html = format!("<html><head><title>Shop</title></head><body>{}</body></html>", cookiebot("popup", &ENGLISH, 1, 2));
        let (text, cut, links) = read_as_before(&html);
        assert!(text.starts_with("Shop Consent Details [#IABV2SETTINGS#] About This website uses cookies"), "got: {}", start(&text));
        let page = read(&html);
        assert_eq!((page.text, page.truncated, page.links), (text, cut, links));
    }

    #[test]
    fn a_dialog_is_moved_whole_and_nothing_with_it() {
        // Elements of its own name inside it, another of that name after
        // it, and words hard against both its ends, which stay two words.
        let dialog = "<div role=\"dialog\"><div>Cookies<div>we use them</div></div><div>Accept</div></div>";
        let html = format!("<body>before{dialog}after<div>the page</div></body>");
        assert_eq!(read_last(&html), format!("<body>before after<div>the page</div></body>{dialog}"));

        // A tag whose name only begins with the dialog's is not one of
        // its own: counted as one, the line break here would be an
        // element opened and never closed.
        let dialog = "<b data-nosnippet>We use cookies<br>on this site.</b>";
        let html = format!("<body>{dialog}<p>The page.</p></body>");
        assert_eq!(read_last(&html), format!("<body> <p>The page.</p></body>{dialog}"));
    }

    #[test]
    fn a_dialog_inside_a_dialog_goes_with_it_and_two_keep_their_order() {
        // OneTrust's is kept out of snippets and has a dialog by role
        // inside it: one block, moved once.
        let outer = "<div id=\"onetrust-consent-sdk\" data-nosnippet=\"true\"><div role=\"dialog\" aria-modal=\"true\">\
            <h2>We use cookies</h2><button>Agree</button></div></div>";
        let form = "<form role=\"dialog\"><h4>Cookies and Advertising Choices</h4></form>";
        let html = format!("<body>{outer}<p>one</p>{form}<p>two</p></body>");
        assert_eq!(read_last(&html), format!("<body> <p>one</p> <p>two</p></body>{outer}{form}"));
    }

    #[test]
    fn a_mark_counts_only_as_an_attribute_of_an_opening_tag() {
        for dialog in [
            "<div data-nosnippet>Cookies</div>",
            "<div data-nosnippet=\"true\">Cookies</div>",
            "<div class=\"banner\"\n     data-nosnippet=\"\"\n>Cookies</div>",
            "<section id=\"consent\" role=\"dialog\">Cookies</section>",
            // A plain answer in capitals.
            "<FORM ROLE=\"dialog\">Cookies</FORM>",
        ] {
            let html = format!("<body>{dialog}<p>The page.</p></body>");
            assert_eq!(read_last(&html), format!("<body> <p>The page.</p></body>{dialog}"), "{dialog}");
        }
        for html in [
            // The words of a page that explains the marks.
            "<body><p>Cookies: give the banner role=\"dialog\" and data-nosnippet.</p><p>The page.</p></body>",
            // Other attributes that end as the marks do, or begin as they do.
            "<body><div data-role=\"dialog\">Cookies</div><p>The page.</p></body>",
            "<body><div data-nosnippet-off>Cookies</div><p>The page.</p></body>",
        ] {
            assert!(matches!(read_last(html), Cow::Borrowed(_)), "{html}");
        }
    }

    #[test]
    fn a_marked_block_is_the_dialog_only_if_it_opens_on_cookies() {
        // The word, whole, within the first 200 characters of its text.
        let block = |lead: usize| {
            format!("<body><div role=\"dialog\"><p>{}</p> cookies, in the small print</div><p>The page.</p></body>", "x".repeat(lead))
        };
        assert!(matches!(read_last(&block(193)), Cow::Owned(_)), "the word ends at the 200th character");
        assert!(matches!(read_last(&block(194)), Cow::Borrowed(_)), "and here one past it");
        // In whatever case, and as part of a longer word.
        for says in ["COOKIES", "Cookie-Einstellungen", "Deze website maakt gebruik van cookies"] {
            let html = format!("<body><div data-nosnippet><h2>{says}</h2></div><p>The page.</p></body>");
            assert!(matches!(read_last(&html), Cow::Owned(_)), "{says}");
        }
    }

    #[test]
    fn markup_that_does_not_balance_is_left_as_it_is() {
        for html in [
            // Never closed.
            "<body><div data-nosnippet><p>We use cookies.</p><p>The page.</p></body>",
            // An element that has no closing tag.
            "<body><img data-nosnippet alt=\"cookies\"><p>Cookies. The page.</p></body>",
            // A page cut short inside the tag itself.
            "<body><p>The page.</p><div role=\"dialog\"",
        ] {
            assert!(matches!(read_last(html), Cow::Borrowed(_)), "{html}");
        }
    }

    #[test]
    fn the_refusal_rule_sees_what_it_saw() {
        use crate::tools::refusal::says_no;
        let dialog = "<div data-nosnippet=\"\"><h2>This website uses cookies</h2><p>We use cookies to count our \
            visitors.</p><button>Deny</button> <button>Allow all</button></div>";
        let notice = |opening: &str| {
            format!(
                "<html><head><title>shop</title></head><body>{opening}<h1>Access blocked</h1>\
                 <p>Your access to the shop has been temporarily blocked.</p></body></html>"
            )
        };
        // A block notice with a dialog on top of it is a refusal, as it
        // was: the dialog does not hide it.
        assert!(fetch::is_refusal(&read(&notice(""))));
        assert!(fetch::is_refusal(&read(&notice(dialog))));
        assert!(read(&notice(dialog)).text.starts_with("shop Access blocked"));
        // A page that is only its consent gate is short and is no
        // refusal: nothing a dialog says is a no.
        let gate = format!("<html><head><title>Shop</title></head><body>{dialog}</body></html>");
        assert!(!fetch::is_refusal(&read(&gate)));
        // Under 10,000 characters of dialog the notice was not known for
        // one, by its length, and is not now: the dialog is moved, not
        // taken out, and the rule counts it as it did.
        let buried = notice(&cookiebot("popup", &DUTCH, 5, 12));
        assert!(!fetch::is_refusal(&read(&buried)));
        // So for every page here the rule answers as it did before.
        for html in [
            notice(""),
            notice(dialog),
            gate,
            buried,
            unpriced_page(&cookiebot("popup", &DUTCH, 5, 12)),
            priced_page(&cookiebot("overlay", &ENGLISH, 27, 4)),
            page_with_flyouts(&shops_own_notice()),
        ] {
            assert_eq!(says_no(&read(&html).text), says_no(&read_as_before(&html).0), "{}", start(&html));
        }
    }
}
