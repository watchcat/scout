//! One shop a session: what "the same shop" means for two addresses.
//!
//! A session is opened on one page and held to that page's registrable
//! domain, so `www.shop.example` may go to `basket.shop.example` and
//! nowhere else. The prototype (`scripts/shop-reach.mjs`) told a shop by
//! its last two labels and a list of eight suffixes that have three, which
//! its own header calls a hole: a shared suffix defeats it. Here the
//! registrable domain comes from the Public Suffix List.

use url::{Host, Url};

/// The shop a session is held to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shop {
    site: String,
}

impl Shop {
    /// The shop `url` is a page of, or `None` where it is no web address.
    pub fn of(url: &Url) -> Option<Shop> {
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        Some(Shop { site: site(&url.host()?) })
    }

    /// Whether `url` is an address of this shop.
    pub fn holds(&self, url: &Url) -> bool {
        Shop::of(url).is_some_and(|other| other == *self)
    }

    /// The registrable domain, or the address itself where the host is one.
    pub fn site(&self) -> &str {
        &self.site
    }
}

/// The part of a host that one party registered: `shop.example` of
/// `www.shop.example`, and three labels where the suffix has two, as under
/// `co.uk`. An IP address is its own site, and so is a name the list knows
/// no owner for.
pub fn site(host: &Host<&str>) -> String {
    match host {
        Host::Domain(name) => {
            // `Url` has already lowercased the name and written it in ASCII.
            let name = name.trim_end_matches('.');
            psl::domain_str(name).unwrap_or(name).to_string()
        }
        Host::Ipv4(ip) => ip.to_string(),
        Host::Ipv6(ip) => ip.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn site_of(s: &str) -> String {
        Shop::of(&url(s)).unwrap().site().to_string()
    }

    fn same(a: &str, b: &str) -> bool {
        Shop::of(&url(a)).unwrap().holds(&url(b))
    }

    #[test]
    fn a_shop_is_its_registrable_domain_whatever_stands_in_front_of_it() {
        assert_eq!(site_of("https://www.shop.example/p/1"), "shop.example");
        assert_eq!(site_of("https://checkout.shop.example/"), "shop.example");
        assert_eq!(site_of("https://shop.example/"), "shop.example");
        // Under a suffix of two labels a shop has three. The names are made up.
        assert_eq!(site_of("https://www.scout-fixture-a.co.uk/"), "scout-fixture-a.co.uk");
        assert_eq!(site_of("https://scout-fixture-a.com.hk/"), "scout-fixture-a.com.hk");
        assert!(same("https://www.shop.example/p/1", "https://basket.shop.example/"));
        assert!(same("https://www.shop.example/p/1", "http://shop.example:8080/x"));
        assert!(!same("https://www.shop.example/p/1", "https://pay.example/shop.example"));
        assert!(!same("https://www.shop.example/p/1", "https://shop.example.pay.example/"));
    }

    #[test]
    fn a_suffix_two_shops_share_does_not_make_them_one_shop() {
        // None of these suffixes was among the prototype's eight, so each
        // pair was "the same shop" by its last two labels. The names in
        // front of them are made up.
        assert_eq!(site_of("https://www.scout-fixture-a.co.za/"), "scout-fixture-a.co.za");
        assert!(!same("https://www.scout-fixture-a.co.za/", "https://scout-fixture-b.co.za/"));
        assert!(!same("https://www.scout-fixture-a.com.mx/", "https://scout-fixture-b.com.mx/"));
        // A suffix that belongs to a host of shops, or of sites, and not to a registry.
        assert!(!same("https://scout-fixture-a.myshopify.com/", "https://scout-fixture-b.myshopify.com/"));
        assert!(!same("https://scout-fixture-a.github.io/", "https://scout-fixture-b.github.io/"));
        assert!(same("https://scout-fixture-a.myshopify.com/", "https://scout-fixture-a.myshopify.com/cart"));
        // A suffix alone is nobody's shop, and no shop under it is the same.
        assert!(!same("https://co.uk/", "https://scout-fixture-a.co.uk/"));
    }

    #[test]
    fn spelling_does_not_move_an_address_to_another_shop() {
        assert!(same("https://www.shop.example/", "https://WWW.Shop.Example./x"));
        // What stands before the @ is a user name, not the host.
        assert!(!same("https://www.shop.example/", "https://www.shop.example@other.example/"));
        assert!(!same("https://www.shop.example/", "https://www.shop.example%2eother.example/"));
        // A name with letters outside ASCII is the same name however it is written.
        assert!(same("https://www.bücher.example/", "https://xn--bcher-kva.example/x"));
        assert!(!same("https://www.bücher.example/", "https://www.bucher.example/"));
    }

    #[test]
    fn an_address_by_number_is_a_site_of_its_own() {
        assert_eq!(site_of("http://127.0.0.1:8080/p"), "127.0.0.1");
        assert!(same("http://127.0.0.1:8080/p", "http://127.0.0.1:9090/q"));
        assert!(!same("http://127.0.0.1:8080/p", "http://127.0.0.2:8080/p"));
        assert!(!same("http://127.0.0.1:8080/p", "http://localhost:8080/p"));
        assert!(same("http://[::1]:8080/p", "http://[::1]/q"));
        // The last labels of a number are not a registrable domain: these
        // two, both from ranges nobody has on the internet, end alike.
        assert!(!same("http://10.0.0.1/", "http://192.168.0.1/"));
        assert_eq!(site_of("http://192.0.2.7/"), "192.0.2.7");
        assert_eq!(site_of("http://localhost:3000/"), "localhost");
    }

    #[test]
    fn only_a_web_address_is_a_page_of_a_shop() {
        for address in ["file:///etc/passwd", "data:text/html,hello", "about:blank", "mailto:?subject=widget", "javascript:void(0)", "ftp://shop.example/x"] {
            assert_eq!(Shop::of(&url(address)), None, "{address}");
            assert!(!Shop::of(&url("https://shop.example/")).unwrap().holds(&url(address)), "{address}");
        }
    }
}
