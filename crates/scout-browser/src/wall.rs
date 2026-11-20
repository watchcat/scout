//! A shop that says no, and when a session is over for it.
//!
//! From the reading (`docs/2026-11-18-shop-reach.md`): told to report a
//! refusal and stop, the model did so at once or after one more look in
//! three sessions of four. In the fourth it went on for eleven actions on a
//! shop that had blocked it, eight of them refused. So the session ends
//! itself, and whoever drives it is not asked.

/// What kind of no a page is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wall {
    /// The page's own HTTP status.
    Status(u16),
    /// A bot check, by a title `fetch_page` knows one by.
    BotCheck,
    /// The shop's own notice that it will not serve this reader, in
    /// ordinary HTML and often with a 200.
    Notice,
}

impl Wall {
    /// What whoever drives the session is told beside the page.
    pub fn note(self) -> String {
        match self {
            Wall::Status(status) => format!("This page is a refusal (HTTP {status}). Nothing is done to get past it."),
            Wall::BotCheck => "This page is a bot check. Nothing is done to get past it.".to_string(),
            Wall::Notice => "This page is the shop's notice that it will not serve this reader. Nothing is done to get past it.".to_string(),
        }
    }
}

/// Whether the page a session is on is a refusal: by the status its
/// document came with, or by what `fetch_page` itself would make of its
/// HTML. `status` is `None` where the browser reported none.
pub fn wall(status: Option<u16>, html: &str, url: &str) -> Option<Wall> {
    if let Some(status) = status.filter(|status| REFUSING.contains(status)) {
        return Some(Wall::Status(status));
    }
    // The extractor itself and not a copy of what it knows: the titles of
    // a bot check and the phrases of a block notice are scout-core's, each
    // with where it was read, and they are added to there.
    let read = scout_core::measure::page_as_read(html, url, None).ok()?;
    if read["challenge"] == true {
        Some(Wall::BotCheck)
    } else if read["refusal"] == true {
        Some(Wall::Notice)
    } else {
        None
    }
}

/// The statuses that are a shop saying no to this reader, as the prototype
/// had them. In the reading the two shops that refused a session answered
/// 403, and plain requests were answered 503 and 429 by two more. A 404 or
/// a 500 is a page that is missing or broken, which is another thing.
const REFUSING: [u16; 4] = [401, 403, 429, 503];

/// The pages a session has been shown, counted for refusals.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Walls {
    pages: u32,
    walls: u32,
    running: u32,
    first: bool,
}

impl Walls {
    /// Counts one more page shown, a refusal or not. A page read twice is
    /// two: looking again at a wall is persisting at it.
    pub fn saw(&mut self, wall: bool) {
        if wall {
            self.walls += 1;
            self.running += 1;
            self.first |= self.pages == 0;
        } else {
            self.running = 0;
        }
        self.pages += 1;
    }

    /// Why the session is over, or `None` while it is not. The prototype's
    /// rule, written after the session that would not stop: the page a
    /// session was opened on refused, or two pages running, or three in
    /// all. One side page that would not open is only that.
    pub fn turned_away(&self) -> Option<&'static str> {
        if self.first {
            Some("the page the session was opened on was refused")
        } else if self.running >= 2 {
            Some("the shop refused two pages running")
        } else if self.walls >= 3 {
            Some("the shop refused three pages")
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://www.shop.example/p/widget";

    fn padded(html: &str) -> String {
        format!("{html}<!-- {} -->", "padding ".repeat(400))
    }

    const PRODUCT: &str = r#"<html><head><title>Widget kopen</title>
        <script type="application/ld+json">
          {"@type":"Product","name":"Widget","offers":{"@type":"Offer","price":"12.99","priceCurrency":"EUR"}}
        </script></head><body><h1>Widget</h1><p>Nu voor 12,99</p></body></html>"#;

    #[test]
    fn a_wall_is_told_by_the_status_its_page_came_with() {
        for status in [401, 403, 429, 503] {
            assert_eq!(wall(Some(status), &padded(PRODUCT), URL), Some(Wall::Status(status)));
        }
        // A page that is not there, or a shop's own fault, is not a shop saying no.
        for status in [200, 204, 404, 410, 500] {
            assert_eq!(wall(Some(status), &padded(PRODUCT), URL), None, "{status}");
        }
        assert_eq!(wall(None, &padded(PRODUCT), URL), None);
    }

    #[test]
    fn a_bot_check_that_answers_200_is_a_wall_by_its_title() {
        let check = padded("<html><head><title>Just a moment...</title></head><body></body></html>");
        assert_eq!(wall(Some(200), &check, URL), Some(Wall::BotCheck));
        assert_eq!(wall(None, &check, URL), Some(Wall::BotCheck));
    }

    #[test]
    fn a_shops_own_block_notice_is_a_wall_though_it_is_an_ordinary_page() {
        // bol.com's notice in shape, as `scout_core::measure`'s own test has it.
        let notice = padded(
            "<html><head><title>bol</title></head><body><h1>IP address is blocked</h1><p>Your access \
             to bol has been temporarily blocked due to possible abuse from this IP address.</p></body></html>",
        );
        assert_eq!(wall(Some(200), &notice, URL), Some(Wall::Notice));
    }

    #[test]
    fn a_page_is_not_a_wall_for_a_word_in_it() {
        assert_eq!(wall(Some(200), &padded(PRODUCT), URL), None);
        let long = format!("<html><head><title>Widget</title></head><body>{} access blocked captcha</body></html>", "A long product page. ".repeat(200));
        assert_eq!(wall(Some(200), &long, URL), None);
    }

    fn after(pages: &[bool]) -> Option<&'static str> {
        let mut walls = Walls::default();
        for wall in pages {
            walls.saw(*wall);
        }
        walls.turned_away()
    }

    #[test]
    fn a_session_is_over_when_the_shop_has_turned_it_away() {
        // The page the session was opened on refused: there is nothing to drive.
        assert_eq!(after(&[true]), Some("the page the session was opened on was refused"));
        // A side page refused once is a page that would not open.
        assert_eq!(after(&[false, true]), None);
        assert_eq!(after(&[false, true, false]), None);
        // Twice running, or three times in all, is a shop saying no.
        assert_eq!(after(&[false, true, true]), Some("the shop refused two pages running"));
        assert_eq!(after(&[false, true, false, true]), None);
        assert_eq!(after(&[false, true, false, true, false, true]), Some("the shop refused three pages"));
        assert_eq!(after(&[false, false, false]), None);
        assert_eq!(after(&[]), None);
    }

    #[test]
    fn a_session_once_turned_away_stays_turned_away() {
        let mut walls = Walls::default();
        walls.saw(true);
        walls.saw(false);
        walls.saw(false);
        assert_eq!(walls.turned_away(), Some("the page the session was opened on was refused"));
    }

    #[test]
    fn each_kind_of_wall_is_named_to_whoever_drives() {
        assert!(Wall::Status(403).note().contains("HTTP 403"));
        assert!(Wall::BotCheck.note().contains("bot check"));
        assert!(Wall::Notice.note().contains("will not serve"));
    }
}
