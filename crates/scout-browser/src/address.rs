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
    /// Where an order is placed and paid for.
    Checkout,
    /// Where a person signs in or makes an account.
    SignIn,
}

impl Refused {
    /// What the caller is told. Final, as the guard's sentence is: another
    /// spelling of the same address meets the same answer.
    pub fn why(self) -> &'static str {
        match self {
            Refused::NotWeb => "that is not a web address",
            Refused::LeavesShop => "that address leaves this shop, and a session stays on the one it was opened on",
            Refused::Checkout => "that address is a checkout, and a session goes no further than the basket",
            Refused::SignIn => "that address is for signing in, and a session never signs in",
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
    // A name and a password written into the address are a sign-in the
    // browser would make on being sent there.
    if !url.username().is_empty() || url.password().is_some() {
        return Some(Refused::SignIn);
    }
    // What stands in front of the shop's own name: `checkout.shop.example`.
    // The name itself is not read, so a shop may be called what it likes.
    let host = url.host_str().unwrap_or_default().trim_end_matches('.');
    let front = host.strip_suffix(shop.site()).unwrap_or_default();
    for label in front.split('.').filter(|label| !label.is_empty()).map(Step::of) {
        if label.says_checkout() {
            return Some(Refused::Checkout);
        }
        if label.says_sign_in() {
            return Some(Refused::SignIn);
        }
    }
    // A page that keeps its route after the hash is at that route; any
    // other fragment is a place on the page.
    let route = url.fragment().map(|f| f.trim_start_matches('!')).filter(|f| f.starts_with('/'));
    let steps: Vec<Step> = url.path().split('/').chain(route.unwrap_or_default().split('/')).filter(|s| !s.is_empty()).map(Step::of).collect();
    // The later step says what the page is: `/checkout/cart` is a basket
    // and `/cart/checkout` is not.
    let last = steps.iter().rev().find_map(|step| {
        if step.says_checkout() {
            Some(true)
        } else if step.says_basket() {
            Some(false)
        } else {
            None
        }
    });
    if last == Some(true) {
        return Some(Refused::Checkout);
    }
    if steps.iter().any(Step::says_sign_in) {
        return Some(Refused::SignIn);
    }
    None
}

// The lists below are a first guess, in Dutch, German and English, at what
// shops call the two places. The one checkout that a session of the reading
// opened is not named in it, and no list of words is every shop's: what may
// be typed is refused on a checkout page as on any other.

/// A checkout wherever the word is written: `/onestepcheckout`,
/// `checkout_shipping.php`.
const CHECKOUT_WRITTEN: &[&str] = &["checkout", "afrekenen"];
/// A checkout only as a whole step of the path, these being also words for
/// a thing a shop sells or writes about: a toy till, a page on orders.
const CHECKOUT_STEP: &[&str] = &["kasse", "kassa", "order", "bestelling", "bestellung"];
/// The basket, which a session may look at for what delivery costs.
const BASKET: &[&str] = &["cart", "basket", "winkelwagen", "winkelmand", "winkelmandje", "warenkorb"];
/// Signing in, where a word starts or ends with it: `loginPost`,
/// `customerlogin`, `einloggen`. Not anywhere in a word: `blogindex`.
const SIGN_IN_WRITTEN: &[&str] = &["login", "signin", "signup", "inloggen"];
/// Signing in or making an account, as a whole step of the path.
const SIGN_IN_STEP: &[&str] = &[
    "logon", "anmelden", "aanmelden", "anmeldung", "register", "registreren", "registrieren",
    "registratie", "registrierung", "registration", "auth", "oauth", "oauth2", "sso", "saml", "openid",
];

/// One step of a path, or one label of a host, as its words.
struct Step {
    /// Lower case, without a file's extension.
    whole: String,
    /// The runs of letters and digits in it.
    words: Vec<String>,
}

impl Step {
    fn of(raw: &str) -> Step {
        let lower = percent_decoded(raw).to_lowercase();
        let whole = match lower.rsplit_once('.') {
            Some((name, extension)) if !name.is_empty() && (2..=5).contains(&extension.len()) && extension.chars().all(|c| c.is_ascii_alphanumeric()) => name.to_string(),
            _ => lower,
        };
        let words = whole.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect();
        Step { whole, words }
    }

    /// Whether the step writes `word`, as one word or as two: `sign-in`,
    /// `sign_in`, `check-out`.
    fn in_two(&self, word: &str) -> bool {
        self.words.windows(2).any(|pair| pair[0].len() + pair[1].len() == word.len() && word.starts_with(pair[0].as_str()) && word.ends_with(pair[1].as_str()))
    }

    fn says_checkout(&self) -> bool {
        CHECKOUT_WRITTEN.iter().any(|c| self.words.iter().any(|w| w.contains(c)) || self.in_two(c)) || CHECKOUT_STEP.contains(&self.whole.as_str())
    }

    fn says_basket(&self) -> bool {
        self.words.iter().any(|w| BASKET.contains(&w.as_str()))
    }

    fn says_sign_in(&self) -> bool {
        SIGN_IN_WRITTEN.iter().any(|s| self.words.iter().any(|w| w.starts_with(s) || w.ends_with(s)) || self.in_two(s))
            || SIGN_IN_STEP.contains(&self.whole.as_str())
            || self.in_two("logon")
    }
}

/// `%63heckout` as `checkout`: a letter written as its number is the same
/// letter, and `Url` leaves it as it was written.
fn percent_decoded(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(high), Some(low)) => {
                out.push((high * 16 + low) as u8);
                i += 3;
            }
            (byte, ..) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
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

    fn on_the_shop(path: &str) -> Option<Refused> {
        refused(&format!("https://www.shop.example{path}"))
    }

    #[test]
    fn a_checkout_address_is_refused() {
        for path in [
            "/checkout", "/checkout/", "/Checkout", "/nl/checkout/shipping", "/checkouts/cn/abc123", "/onestepcheckout/",
            "/checkout.html", "/checkout_shipping.php", "/check-out", "/afrekenen", "/nl/afrekenen/", "/kasse", "/kasse/zahlung",
            "/kassa", "/order", "/order/", "/bestelling", "/bestellung", "/cart/checkout", "/basket/checkout/address",
            // A letter written as its number is the same letter.
            "/%63heckout", "/check%6Fut/",
            // A page that keeps its route after the hash.
            "/#/checkout", "/shop#!/checkout/address",
        ] {
            assert_eq!(on_the_shop(path), Some(Refused::Checkout), "{path}");
        }
        assert_eq!(refused("https://checkout.shop.example/"), Some(Refused::Checkout));
        assert_eq!(refused("https://secure-checkout.shop.example/c/1"), Some(Refused::Checkout));
        // A host named for its checkout is one whatever the path says.
        assert_eq!(refused("https://checkout.shop.example/cart"), Some(Refused::Checkout));
    }

    #[test]
    fn a_basket_is_not_a_checkout_and_neither_is_a_page_about_paying() {
        for path in [
            "/", "/p/widget", "/cart", "/basket", "/winkelwagen", "/winkelmand", "/warenkorb",
            // A basket whose address begins with the word is a basket still: the later step says what the page is.
            "/checkout/cart", "/checkout/cart/", "/checkout/cart/add/product/12",
            // A page about delivery that is also about paying is one a session is meant to read.
            "/verzenden-en-betalen", "/versand-und-zahlung", "/shipping-and-payment", "/klantenservice/bezorgen-en-betalen",
            // The word inside a product's own name, or a page that is about orders and is not one.
            "/p/holz-kasse-spielzeug", "/pre-order-widgets", "/order-status", "/orders-and-returns", "/kassasystemen",
            // An anchor on a page is a place on it, and what was searched for is not where the page is.
            "/delivery#checkout", "/search?q=checkout", "/cart?next=/checkout",
        ] {
            assert_eq!(on_the_shop(path), None, "{path}");
        }
        // A shop may be named as it likes; only what stands in front of its name is read.
        assert_eq!(address_refused(&Shop::of(&Url::parse("https://checkout.example/").unwrap()).unwrap(), &Url::parse("https://www.checkout.example/p/1").unwrap()), None);
    }

    #[test]
    fn the_word_in_the_middle_of_another_products_name_is_refused_knowingly() {
        // "checkout" is taken wherever it is written, so a till for a shop
        // counter cannot be opened either. The other way round a checkout
        // named `/guestcheckout` would be let through.
        assert_eq!(on_the_shop("/p/self-checkout-kiosk"), Some(Refused::Checkout));
    }

    #[test]
    fn a_sign_in_address_is_refused() {
        for path in [
            "/login", "/login/", "/Login.aspx", "/login.php", "/account/login", "/customer/account/login/",
            "/customer/account/loginPost/", "/customerlogin", "/ap/signin", "/sign-in", "/users/sign_in", "/signup",
            "/sign-up", "/log-in", "/logon", "/inloggen", "/klant/inloggen", "/einloggen", "/anmelden", "/aanmelden",
            "/register", "/account/register", "/registrieren", "/registreren", "/oauth/authorize", "/oauth2/start",
            "/sso/start", "/auth", "/auth/callback", "/#/login", "/checkout/login",
        ] {
            assert!(matches!(on_the_shop(path), Some(Refused::SignIn | Refused::Checkout)), "{path}");
        }
        assert_eq!(on_the_shop("/account/login"), Some(Refused::SignIn));
        assert_eq!(refused("https://login.shop.example/"), Some(Refused::SignIn));
        assert_eq!(refused("https://auth.shop.example/start?client=shop"), Some(Refused::SignIn));
    }

    #[test]
    fn an_address_that_carries_a_name_and_a_password_is_a_sign_in() {
        // A browser sent there offers them to the shop as its own.
        assert_eq!(refused("https://klant:geheim@www.shop.example/p/widget"), Some(Refused::SignIn));
        assert_eq!(refused("https://klant@www.shop.example/"), Some(Refused::SignIn));
        assert_eq!(refused("https://www.shop.example/p/widget"), None);
    }

    #[test]
    fn a_page_is_not_a_sign_in_for_having_the_letters_in_it() {
        for path in [
            "/blog/index", "/blogindex", "/designing-widgets", "/p/cash-register-toy", "/author/jansen",
            "/catalogus", "/authentic-widgets", "/p/logo-widget", "/search?q=login", "/account", "/p/1#login",
        ] {
            assert_eq!(on_the_shop(path), None, "{path}");
        }
    }

    #[test]
    fn every_refusal_says_why_in_a_sentence_of_its_own() {
        let all = [Refused::NotWeb, Refused::LeavesShop, Refused::Checkout, Refused::SignIn];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.why().is_empty());
            for b in &all[i + 1..] {
                assert_ne!(a.why(), b.why());
            }
        }
    }
}
