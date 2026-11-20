//! The addresses a session does not go to.
//!
//! Decided here, from the address alone, so that the same answer is given
//! wherever an address turns up: the page a session is opened on, a link
//! before it is pressed, a request the browser is about to make for the
//! main frame — a redirect's next hop included — and the address a page is
//! found at after a press.

use crate::site::Shop;
use url::Url;

/// Why a session does not go to an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Not `http` or `https`: a file, a mail address, a page of the browser's own.
    NotWeb,
    /// Another registrable domain than the one the session was opened on.
    LeavesShop,
}

impl Refused {
    /// What the caller is told. Final, as the guard's sentence is: another
    /// spelling of the same address meets the same answer.
    pub fn why(self) -> &'static str {
        match self {
            Refused::NotWeb => "that is not a web address",
            Refused::LeavesShop => "that address leaves this shop, and a session stays on the one it was opened on",
        }
    }
}

/// Why the main frame of a session on `shop` may not be at `url`, or
/// `None` when it may.
pub fn address_refused(shop: &Shop, url: &Url) -> Option<Refused> {
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return Some(Refused::NotWeb);
    }
    if !shop.holds(url) {
        return Some(Refused::LeavesShop);
    }
    None
}

/// The same for a link as the page wrote it, before it is pressed. A
/// `javascript:` link is a button by another name and goes nowhere by
/// itself; where its script then sends the page is judged when it does.
pub fn link_refused(shop: &Shop, href: &str) -> Option<Refused> {
    let href = href.trim();
    if href.is_empty() || href.get(..11).is_some_and(|scheme| scheme.eq_ignore_ascii_case("javascript:")) {
        return None;
    }
    match Url::parse(href) {
        Ok(url) => address_refused(shop, &url),
        Err(_) => Some(Refused::NotWeb),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shop() -> Shop {
        Shop::of(&Url::parse("https://www.shop.example/p/widget").unwrap()).unwrap()
    }

    fn refused(address: &str) -> Option<Refused> {
        address_refused(&shop(), &Url::parse(address).unwrap())
    }

    #[test]
    fn a_navigation_off_the_shops_registrable_domain_is_refused() {
        assert_eq!(refused("https://www.shop.example/p/other-widget"), None);
        assert_eq!(refused("https://basket.shop.example/cart"), None);
        assert_eq!(refused("http://shop.example/"), None);
        assert_eq!(refused("https://other.example/"), Some(Refused::LeavesShop));
        assert_eq!(refused("https://pay.example/shop.example"), Some(Refused::LeavesShop));
        assert_eq!(refused("https://www.shop.example.other.example/"), Some(Refused::LeavesShop));
        assert_eq!(refused("https://www.shop.example@other.example/"), Some(Refused::LeavesShop));
        assert_eq!(refused("http://127.0.0.1:9090/metrics"), Some(Refused::LeavesShop));
    }

    #[test]
    fn an_address_that_is_not_the_webs_is_refused() {
        for address in ["file:///etc/passwd", "data:text/html,<h1>x</h1>", "about:blank", "chrome://settings/", "blob:https://www.shop.example/1", "ftp://www.shop.example/x"] {
            assert_eq!(refused(address), Some(Refused::NotWeb), "{address}");
        }
    }

    #[test]
    fn a_link_is_judged_as_the_page_wrote_it_before_it_is_pressed() {
        let shop = shop();
        assert_eq!(link_refused(&shop, "https://www.shop.example/delivery"), None);
        assert_eq!(link_refused(&shop, "https://other.example/"), Some(Refused::LeavesShop));
        // Something that would start another program is not pressed.
        for href in ["mailto:?subject=widget", "tel:0000", "sms:0000", "intent://scan/#Intent;scheme=zxing;end", "file:///etc/passwd"] {
            assert_eq!(link_refused(&shop, href), Some(Refused::NotWeb), "{href}");
        }
        // A script link is a button; a link with no address is the page itself.
        assert_eq!(link_refused(&shop, "javascript:void(0)"), None);
        assert_eq!(link_refused(&shop, "JavaScript:addToBasket()"), None);
        assert_eq!(link_refused(&shop, ""), None);
        // Something that is no address at all goes nowhere.
        assert_eq!(link_refused(&shop, "https://"), Some(Refused::NotWeb));
    }

    #[test]
    fn every_refusal_says_why_in_a_sentence_of_its_own() {
        let all = [Refused::NotWeb, Refused::LeavesShop];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.why().is_empty());
            for b in &all[i + 1..] {
                assert_ne!(a.why(), b.why());
            }
        }
    }
}
