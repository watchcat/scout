//! Pages that say no in ordinary HTML.
//!
//! A bot check announces itself in its title and a shell is a few hundred
//! bytes; `browser.rs` knows both. A third kind is neither: the shop's own
//! notice that it will not serve this reader, laid out like any page of
//! the shop, answered with a 200 or shown to the browser that was sent
//! after a 403. The one bol.com showed a browser on 2026-11-18 is 9 KB of
//! HTML. Read as a page it is a product page with nothing on it, and that
//! is how it was handed to the model.
//!
//! What gives such a page away is that it is short and says so. The rule
//! and its phrases are in `refusal_marks.txt`, each phrase with where it
//! was read; the trace script reads the same file, so that what the script
//! counts is what the tool refuses.

use std::sync::OnceLock;

struct Rule {
    /// A page with this many characters of text or more is taken as a page.
    max_chars: usize,
    /// Lower case, as the text is compared.
    marks: Vec<&'static str>,
}

const SOURCE: &str = include_str!("refusal_marks.txt");

/// What is trimmed off a line, spelt out because the script's reader
/// trims the same and each language's own idea of white space differs.
const BLANK: [char; 3] = [' ', '\t', '\r'];

fn parse(source: &'static str) -> Result<Rule, String> {
    let mut max_chars = None;
    let mut marks = Vec::new();
    for line in source.split('\n').map(|line| line.trim_matches(BLANK)) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.strip_prefix("max_chars ") {
            // Digits and nothing else, so that the script's reader and this
            // one cannot disagree about a sign or a stray character.
            Some(n) if max_chars.is_none() => {
                let n = n.trim_matches(BLANK);
                if n.is_empty() || !n.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(format!("max_chars: {n:?} is not a number"));
                }
                max_chars = Some(n.parse().map_err(|e| format!("max_chars: {e}"))?);
            }
            Some(_) => return Err("max_chars is given twice".into()),
            None => marks.push(line),
        }
    }
    let max_chars = max_chars.ok_or("no max_chars line")?;
    if marks.is_empty() {
        return Err("no phrases".into());
    }
    Ok(Rule { max_chars, marks })
}

fn rule() -> &'static Rule {
    static RULE: OnceLock<Rule> = OnceLock::new();
    // The file is compiled in and a test parses it, so this cannot fail in a
    // build that passed its tests.
    RULE.get_or_init(|| parse(SOURCE).expect("refusal_marks.txt"))
}

/// Whether a page's text, as `fetch_page` extracts it, is a site refusing
/// its reader rather than the page: short, and saying so.
pub fn says_no(text: &str) -> bool {
    let rule = rule();
    // Counted no further than the limit: the text may be a whole page.
    if text.chars().take(rule.max_chars).count() >= rule.max_chars {
        return false;
    }
    let lower = text.to_lowercase();
    rule.marks.iter().any(|mark| lower.contains(mark))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What bol.com showed a browser at the node on 2026-11-18, title
    /// first as the extractor leaves it: the start of it.
    const BOL_BROWSER: &str = "bol Access blocked Your access to bol has been temporarily blocked \
        due to possible abuse. This can have several causes, including: Use of a VPN";
    /// And its answer to the plain request the same day: the start of its
    /// 1,097 characters of text.
    const BOL_PLAIN: &str = "bol IP address is blocked Your access to bol has been temporarily \
        blocked due to possible abuse from this IP address. This may be caused by several \
        factors, including: Use of a VPN: Please turn off your VPN and try accessing the site \
        again.";
    /// amazon.nl's whole answer to a plain request, HTTP 200.
    const AMAZON_NOTICE: &str = "Amazon.nl Klik op de knop hieronder om verder te winkelen \
        Doorgaan met winkelen Amazon.nl Gebruiks- en verkoopvoorwaarden Amazon.nl \
        Privacyverklaring © 1996-2025, Amazon.com, Inc. en dochterondernemingen";
    /// amazon.nl's whole answer to another plain request, HTTP 202.
    const AMAZON_CHECK: &str = "JavaScript is disabled In order to continue, we need to verify \
        that you're not a robot. This requires JavaScript. Enable JavaScript and then reload \
        the page.";

    fn a_page_of(chars: usize, saying: &str) -> String {
        let mut text = String::from(saying);
        while text.chars().count() < chars {
            text.push_str(" A widget for every purpose, in four colours");
        }
        text.chars().take(chars).collect()
    }

    #[test]
    fn the_notices_scout_has_met_are_refusals() {
        for (who, text) in [
            ("bol to a browser", BOL_BROWSER),
            ("bol to the plain request", BOL_PLAIN),
            ("bol with a Dutch cookie", "bol IP adres is geblokkeerd Je toegang tot bol is tijdelijk geblokkeerd vanwege mogelijk misbruik vanaf dit IP adres."),
            ("bol with a French one", "bol L'adresse IP est bloquée Votre accès à bol a été temporairement bloqué en raison d'un possible abus"),
            ("amazon's continue-shopping notice", AMAZON_NOTICE),
            ("amazon's robot check", AMAZON_CHECK),
        ] {
            assert!(says_no(text), "{who}: {text}");
        }
    }

    #[test]
    fn the_same_words_on_a_page_with_a_page_of_text_are_the_page() {
        // A review that tells of a blocked account, a forum thread about a
        // robot check: the words are there and the page is the page.
        let limit = rule().max_chars;
        for saying in [BOL_PLAIN, AMAZON_CHECK, "temporairement bloqué"] {
            assert!(says_no(&a_page_of(limit - 1, saying)), "one character under the limit is short");
            assert!(!says_no(&a_page_of(limit, saying)), "the limit itself is a page");
            assert!(!says_no(&a_page_of(6000, saying)));
        }
    }

    #[test]
    fn a_short_page_that_does_not_say_no_is_a_page() {
        for text in [
            "",
            "Widget 13,80 In stock Add to basket",
            // An empty basket says this on every shop there is.
            "Your basket is empty. Continue shopping",
            "Je winkelwagen is leeg. Verder winkelen",
            // The footer of every page behind reCAPTCHA.
            "Contact Name Email Message Send This site is protected by reCAPTCHA and the Google \
             Privacy Policy and Terms of Service apply.",
            "Robot vacuum cleaner 13,80",
            "De afvoer is geblokkeerd? Bel de loodgieter.",
            // Said to anyone not signed in; `browser.rs` knows it as a title.
            "Access denied. Please sign in to see your orders.",
        ] {
            assert!(!says_no(text), "{text}");
        }
    }

    /// The cases the trace script's tests read too: the two are separate
    /// implementations of one rule, and these are what holds them to it.
    #[derive(serde::Deserialize)]
    struct Case {
        why: String,
        text: String,
        /// Filled out with full stops to this many characters.
        pad_to: Option<usize>,
        refusal: bool,
    }

    #[test]
    fn the_cases_shared_with_the_trace_script_come_out_as_written() {
        let cases: Vec<Case> = serde_json::from_str(include_str!("refusal_cases.json")).expect("the cases parse");
        assert!(cases.len() >= 20, "the file was read");
        for case in cases {
            let mut text = case.text.clone();
            if let Some(chars) = case.pad_to {
                let have = text.chars().count();
                assert!(have <= chars, "{}: already longer than {chars}", case.why);
                text.push_str(&".".repeat(chars - have));
                assert_eq!(text.chars().count(), chars);
            }
            assert_eq!(says_no(&text), case.refusal, "{}", case.why);
        }
    }

    #[test]
    fn the_words_are_found_whatever_their_case() {
        assert!(says_no("ACCESS BLOCKED"));
        assert!(says_no("We need to verify that you're Not A Robot"));
    }

    #[test]
    fn the_shared_file_is_a_limit_and_phrases_ready_to_compare() {
        let rule = parse(SOURCE).expect("the file parses");
        assert_eq!(rule.max_chars, 1500);
        assert!(rule.marks.contains(&"temporarily blocked"));
        for mark in &rule.marks {
            assert_eq!(*mark, mark.trim());
            assert_eq!(*mark, mark.to_lowercase(), "compared in lower case, so written in it");
            assert!(mark.len() >= 10, "{mark:?} is too little to tell a refusal by");
        }
        let mut unique = rule.marks.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), rule.marks.len(), "a phrase twice");
    }

    #[test]
    fn a_file_without_a_limit_or_without_phrases_is_refused() {
        assert!(parse("# nothing\n").is_err());
        assert!(parse("max_chars 1500\n").is_err());
        assert!(parse("access blocked\n").is_err());
        assert!(parse("max_chars many\naccess blocked\n").is_err());
        assert!(parse("max_chars +1500\naccess blocked\n").is_err());
        // A line ends at a newline and is trimmed of spaces, tabs and a
        // carriage return: nothing the script's reader does otherwise.
        let rule = parse("max_chars 20\r\n\taccess\u{b}blocked\u{1f}\r\n").expect("parses");
        assert_eq!(rule.marks, vec!["access\u{b}blocked\u{1f}"]);
        assert!(parse("max_chars \naccess blocked\n").is_err());
        assert!(parse("max_chars 1500\nmax_chars 900\naccess blocked\n").is_err());
        let rule = parse("# a note\n\n  max_chars 20 \n  access blocked  \n").expect("parses");
        assert_eq!((rule.max_chars, rule.marks), (20, vec!["access blocked"]));
    }
}
