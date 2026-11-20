//! The addresses a session does not go to.
//!
//! Decided here, from the address alone, so that the same answer is given
//! wherever an address turns up: the page a session is opened on, a link
//! before it is pressed, a request the browser is about to make for the
//! main frame — a redirect's next hop included — and the address a page is
//! found at after a press.

use crate::drive::ADDRESS_CHARS;
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
    /// Longer than a caller may ask a session to open.
    TooLong,
}

/// The address a caller asks a session to open, as an address: on opening
/// a session, and for `open` within one. Where a page goes by itself is
/// not judged here but by [`address_refused`], at any length.
pub fn asked_address(text: &str) -> Result<Url, Refused> {
    let text = text.trim();
    // What follows a `?` is free text. A session holds nothing but a
    // brief, and an address is not where a page of it goes.
    if text.chars().count() > ADDRESS_CHARS {
        return Err(Refused::TooLong);
    }
    Url::parse(text).map_err(|_| Refused::NotWeb)
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
            Refused::TooLong => "that address is longer than the 2000 characters a session will open",
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
    // other fragment is a place on the page. A shop that keeps its route
    // in the query has it under one of a few names; anything else in a
    // query is what was searched for, or where a basket would go next, and
    // not where the page is.
    let fragment = url.fragment().map(|f| f.trim_start_matches('!')).filter(|f| f.starts_with('/')).unwrap_or_default().to_string();
    let routes: Vec<String> = url.query_pairs().filter(|(name, _)| ROUTE_NAMES.contains(&name.to_lowercase().as_str())).map(|(_, route)| route.into_owned()).collect();
    let steps: Vec<Step> = url
        .path()
        .split('/')
        .map(percent_decoded)
        .chain(routes.iter().flat_map(|route| route.split('/')).map(str::to_string))
        .chain(fragment.split('/').map(percent_decoded))
        .filter(|step| !step.is_empty())
        .map(|step| Step::of(&step))
        .collect();
    // The later step says what the page is: `/checkout/cart` is a basket,
    // and `/cart/checkout` and `/checkout/cart/payment` are not. A word for
    // paying under a step that says the page is information is neither.
    let mut is_checkout = false;
    let mut is_information = false;
    for step in &steps {
        is_information |= step.says_information();
        if step.says_checkout() || (step.says_paying() && !is_information) {
            is_checkout = true;
        } else if step.says_basket() {
            is_checkout = false;
        }
    }
    if is_checkout {
        return Some(Refused::Checkout);
    }
    if steps.iter().any(Step::says_sign_in) {
        return Some(Refused::SignIn);
    }
    None
}

// The lists below are a guess, in Dutch, German and English, at what shops
// call the two places: a first one, and then what the review of e4dd2f0
// found it let through. The one checkout that a session of the reading
// opened is not named in it, and no list of words is every shop's: what may
// be typed is refused on a checkout page as on any other.

/// A checkout wherever the word is written: `/onestepcheckout`,
/// `checkout_shipping.php`.
const CHECKOUT_WRITTEN: &[&str] = &["checkout", "afrekenen"];
/// A checkout only as a step of its own, these being also words for a
/// thing a shop sells or says of all it sells: a toy till, a page on
/// orders, "order it online".
const CHECKOUT_STEP: &[&str] = &["kasse", "kassa", "order", "bestelling", "bestellung", "bestellen"];
/// Paying, as a step of its own. Beside other words it is a page about
/// paying, which may be the shop's page about delivery too
/// (`/verzenden-en-betalen`), and that page a session is meant to read.
const PAYING_STEP: &[&str] = &["betalen", "betaling", "bezahlen", "zahlung", "payment", "pay"];
/// The little words a step of its own may begin with: `/zur-kasse`,
/// `/naar-de-kassa`.
const ON_THE_WAY_TO: &[&str] = &["zur", "zum", "zu", "naar", "de", "het", "to", "the", "go", "ga", "door", "verder", "weiter", "direct", "direkt", "jetzt", "nu"];
/// A step that says what is under it is something to read.
const INFORMATION: &[&str] = &["klantenservice", "kundenservice", "service", "help", "hulp", "hilfe", "info", "informatie", "information", "faq", "support"];
/// The basket, which a session may look at for what delivery costs.
const BASKET: &[&str] = &["cart", "basket", "winkelwagen", "winkelmand", "winkelmandje", "warenkorb"];
/// Signing in, where a word starts or ends with it: `loginPost`,
/// `customerlogin`, `einloggen`. Not anywhere in a word: `blogindex`.
const SIGN_IN_WRITTEN: &[&str] = &["login", "signin", "signup", "inloggen"];
/// Signing in or making an account, as a whole step of the path.
const SIGN_IN_STEP: &[&str] = &[
    "logon", "anmelden", "aanmelden", "anmeldung", "register", "registreren", "registrieren", "registratie", "registrierung",
    "registration", "auth", "oauth", "oauth2", "sso", "saml", "openid", "authenticate", "authentication", "authorize",
];
/// A person's own corner of a shop, as a step of its own: behind a sign-in
/// or the way to one, and nothing a session has business with.
const ACCOUNT_STEP: &[&str] = &["account", "konto", "kundenkonto"];
/// Whose it is said to be: `/mijn-account`, `/mein-konto`.
const ONES_OWN: &[&str] = &["mijn", "my", "mein", "meine", "uw", "your"];
/// A page about a password is a page for whoever has one.
const PASSWORD: &[&str] = &["password", "wachtwoord", "passwort", "kennwort"];
/// The names a shop keeps its route under when it keeps it in the query:
/// `?route=checkout/checkout`, `?controller=order`.
const ROUTE_NAMES: &[&str] = &["route", "controller", "action", "page", "view", "module", "cl", "rt", "dispatch"];

/// One step of a path, or one label of a host, as its words.
struct Step {
    /// The runs of letters and digits in it, lower case, without a file's
    /// extension.
    words: Vec<String>,
}

impl Step {
    /// `raw` is as a person would read it: a letter written as its number
    /// has been made the letter.
    fn of(raw: &str) -> Step {
        let lower = raw.to_lowercase();
        let named = match lower.rsplit_once('.') {
            Some((name, extension)) if !name.is_empty() && (2..=5).contains(&extension.len()) && extension.chars().all(|c| c.is_ascii_alphanumeric()) => name,
            _ => lower.as_str(),
        };
        Step { words: named.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect() }
    }

    /// Whether the step writes `word`, as one word or as two: `sign-in`,
    /// `sign_in`, `check-out`.
    fn in_two(&self, word: &str) -> bool {
        self.words.windows(2).any(|pair| pair[0].len() + pair[1].len() == word.len() && word.starts_with(pair[0].as_str()) && word.ends_with(pair[1].as_str()))
    }

    /// Whether the step is one of `these` and nothing else, but for the
    /// little words in `before` that may lead up to it.
    fn is_one_of(&self, these: &[&str], before: &[&str]) -> bool {
        match self.words.split_last() {
            Some((last, leading)) => these.contains(&last.as_str()) && leading.iter().all(|word| before.contains(&word.as_str())),
            None => false,
        }
    }

    fn says_checkout(&self) -> bool {
        CHECKOUT_WRITTEN.iter().any(|c| self.words.iter().any(|w| w.contains(c)) || self.in_two(c)) || self.is_one_of(CHECKOUT_STEP, ON_THE_WAY_TO)
    }

    fn says_paying(&self) -> bool {
        self.is_one_of(PAYING_STEP, ON_THE_WAY_TO)
    }

    fn says_information(&self) -> bool {
        self.words.iter().any(|w| INFORMATION.contains(&w.as_str()))
    }

    fn says_basket(&self) -> bool {
        self.words.iter().any(|w| BASKET.contains(&w.as_str()))
    }

    fn says_sign_in(&self) -> bool {
        SIGN_IN_WRITTEN.iter().any(|s| self.words.iter().any(|w| w.starts_with(s) || w.ends_with(s)) || self.in_two(s))
            || self.is_one_of(SIGN_IN_STEP, &[])
            || self.in_two("logon")
            || self.is_one_of(ACCOUNT_STEP, ONES_OWN)
            || self.words.iter().any(|w| PASSWORD.contains(&w.as_str()))
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
        // The same for a word for a password, which is taken wherever it is
        // a word of its own, so that `/wachtwoord-vergeten` is caught.
        assert_eq!(on_the_shop("/p/password-notebook-a5"), Some(Refused::SignIn));
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
    fn the_words_shops_were_found_to_use_that_the_first_lists_let_through() {
        // The review of e4dd2f0 tried these by hand; each was open.
        for path in [
            "/zur-kasse", "/naar-de-kassa", "/bestellen", "/winkelwagen/bestellen", "/betalen", "/bezahlen", "/zahlung", "/payment", "/pay",
            // A basket is a basket until a later step says paying.
            "/checkout/cart/payment",
            // A shop that keeps its route in the query.
            "/index.php?route=checkout/checkout", "/index.php?controller=order", "/?page=checkout",
        ] {
            assert_eq!(on_the_shop(path), Some(Refused::Checkout), "{path}");
        }
        for path in [
            "/customer/account/create", "/account/create", "/account", "/mijn-account", "/my-account", "/mein-konto", "/password/reset",
            "/wachtwoord-vergeten", "/authenticate", "/authentication", "/authorize", "/index.php?route=account/login", "/index.php?controller=authentication",
        ] {
            assert_eq!(on_the_shop(path), Some(Refused::SignIn), "{path}");
        }
    }

    #[test]
    fn a_page_that_says_how_to_pay_is_not_the_page_where_it_is_done() {
        // By rule: a word for paying is a checkout as a step of its own,
        // and not beside other words, nor under a step that says the page
        // is information.
        for path in [
            "/verzenden-en-betalen", "/versand-und-zahlung", "/shipping-and-payment", "/betalen-en-verzenden", "/betaalmethoden", "/payment-methods",
            "/klantenservice/betalen", "/klantenservice/bezorgen-en-betalen", "/service/payment", "/help/payment", "/info/zahlung", "/customer-service/pay",
            // To order something is also what a shop says of everything it sells.
            "/online-bestellen", "/kattenvoer-bestellen", "/p/widget-bestellen",
            // A basket by its route, a search, a page of results, a product by its number.
            "/index.php?route=checkout/cart", "/search?q=checkout&route=", "/cart?next=/checkout", "/p/1?page=2", "/index.php?route=product/product&product_id=1",
            // An account book is a book.
            "/p/account-book", "/accountancy-books", "/kontor",
        ] {
            assert_eq!(on_the_shop(path), None, "{path}");
        }
    }

    #[test]
    fn an_address_a_caller_asks_for_is_a_web_address_of_no_great_length() {
        assert_eq!(asked_address(" https://www.shop.example/p/widget ").map(|url| url.to_string()), Ok("https://www.shop.example/p/widget".to_string()));
        assert_eq!(asked_address("not an address"), Err(Refused::NotWeb));
        // What follows the question mark is free text, and an address is no place for a page of it.
        let long = format!("https://www.shop.example/search?q={}", "x".repeat(ADDRESS_CHARS));
        assert_eq!(asked_address(&long), Err(Refused::TooLong));
        let fits = format!("https://www.shop.example/search?q={}", "x".repeat(ADDRESS_CHARS - 34));
        assert_eq!((fits.chars().count(), asked_address(&fits).is_ok()), (ADDRESS_CHARS, true));
        assert!(Refused::TooLong.why().contains(&ADDRESS_CHARS.to_string()));
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
            "/catalogus", "/authentic-widgets", "/p/logo-widget", "/search?q=login", "/p/1#login",
        ] {
            assert_eq!(on_the_shop(path), None, "{path}");
        }
    }

    #[test]
    fn every_refusal_says_why_in_a_sentence_of_its_own() {
        let all = [Refused::NotWeb, Refused::LeavesShop, Refused::Checkout, Refused::SignIn, Refused::TooLong];
        for (i, a) in all.iter().enumerate() {
            assert!(!a.why().is_empty());
            for b in &all[i + 1..] {
                assert_ne!(a.why(), b.why());
            }
        }
    }
}
