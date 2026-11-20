//! What a session may type, decided by refusing the personal first.
//!
//! Ported from `typingRefused` in `scripts/shop-reach.mjs`, the prototype
//! the reading of 2026-11-18 was made with, and from its tests. Over that
//! reading's twenty sessions the only things typed were a quantity, a
//! postcode, search terms and list choices
//! (`docs/2026-11-18-shop-reach.md`), so what is left after the refusals is
//! those and a house number, which a Dutch shop asks for beside the
//! postcode.

/// The most a session types into one field. The spec's number: a search
/// term, a quantity and a postcode are all far shorter.
pub const MAX_TYPED: usize = 64;

/// What a page says of the thing a session wants to type into: its tag,
/// its attributes and its label, as the outline's script reads them off
/// the element.
#[derive(Debug, Default, Clone, PartialEq, serde::Deserialize)]
pub struct Field {
    /// `INPUT`, `TEXTAREA`, or whatever else was numbered.
    pub tag: String,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub inputmode: Option<String>,
    pub autocomplete: Option<String>,
    pub name: Option<String>,
    pub id: Option<String>,
    pub placeholder: Option<String>,
    pub title: Option<String>,
    /// `aria-label`, the text `aria-labelledby` points at, and the
    /// `<label>`, together.
    pub label: Option<String>,
}

const TOO_LONG: &str = "at most 64 characters may be typed";
const NOT_A_FIELD: &str = "that is not a field that takes a line of text";
const PERSONAL: &str = "that field is for something personal";

/// What a page calls a field when it is a person's: a name, an email, a
/// phone, a street or a place, a card or an account, a password, a date of
/// birth, a login. Dutch, German and English, as the prototype's list had
/// them after two reviews. Matched anywhere in what the page says of the
/// field, so `visitorNameSearch` is a name.
const PERSONAL_WORDS: &[&str] = &[
    "naam", "name", "mail", "telefo", "gsm", "rufnummer", "phone", "handy", "mobil", "straat",
    "street", "straße", "strasse", "adres", "address", "wohnort", "stadt", "city", "town",
    "plaats", "gemeente", "card", "kaart", "karte", "iban", "konto", "rekening", "account",
    "bsn", "cvv", "cvc", "prüfziffer", "sicherheitscode", "wachtwoord", "password", "passwort",
    "kennwort", "geboorte", "geboren", "geburt", "birth", "login", "benutzer", "user", "kunden",
    "klant", "customer", "firma", "company", "bedrijf",
];

/// The same, for words short enough to sit inside others: these count only
/// standing alone, so a `sort` field is not an `ort`.
const PERSONAL_ALONE: &[&str] = &["tel", "ort", "pin", "dob"];

/// A place, which a field that also says postcode is forgiven: where a
/// delivery checker takes a postcode it often offers a place as well.
const PLACES: &[&str] = &["adres", "address", "ort", "stadt", "city", "plaats"];

/// What a numeric keypad is for when it is not for a phone number.
const COUNTED: &[&str] = &[
    "aantal", "anzahl", "hoeveelheid", "quantity", "qty", "menge", "stück", "stuks", "huisnummer",
    "hausnummer",
];

/// The autofill names the HTML standard gives a person's own details.
/// `postal-code` and the two for a country are the ones left out.
const PERSONAL_AUTOFILL: &[&str] = &[
    "name", "given-name", "additional-name", "family-name", "nickname", "email", "impp", "tel",
    "street-address", "username", "new-password", "current-password", "one-time-code", "bday",
    "organization", "organization-title", "sex", "url", "photo",
];
const PERSONAL_AUTOFILL_STARTS: &[&str] =
    &["honorific-", "tel-", "address-line", "address-level", "bday-", "cc-"];

/// `a` then `b`, with at most one character between: "postcode",
/// "post code", "post-code".
fn says(said: &str, a: &str, b: &str) -> bool {
    said.match_indices(a).any(|(at, _)| {
        let rest = &said[at + a.len()..];
        rest.starts_with(b) || rest.chars().next().is_some_and(|c| rest[c.len_utf8()..].starts_with(b))
    })
}

/// `word` standing alone: not part of a longer run of letters and digits.
/// Narrower than the prototype's `\b`, to which an underscore was a letter
/// and `tel_nr` therefore no phone.
fn says_alone(said: &str, word: &str) -> bool {
    said.match_indices(word).any(|(at, _)| {
        let before = said[..at].chars().next_back();
        let after = said[at + word.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric) && !after.is_some_and(char::is_alphanumeric)
    })
}

fn says_postcode(said: &str) -> bool {
    says(said, "post", "code")
        || says(said, "postal", "code")
        || said.contains("plz")
        // "zip" at the start of a word: `zip`, `zipcode`, not `unzip`.
        || said.match_indices("zip").any(|(at, _)| !said[..at].chars().next_back().is_some_and(char::is_alphanumeric))
}

/// Why `text` may not be typed into `field`, or `None` when it may.
///
/// Anything a page marks as personal is refused before anything is
/// allowed. What is left is a search term, a quantity, a postcode and a
/// house number.
pub fn typing_refused(field: &Field, text: &str) -> Option<&'static str> {
    if text.chars().count() > MAX_TYPED {
        return Some(TOO_LONG);
    }
    let lower = |s: &Option<String>| s.as_deref().unwrap_or_default().trim().to_lowercase();
    let kind = match lower(&field.kind) {
        kind if kind.is_empty() => "text".to_string(),
        kind => kind,
    };
    let inputmode = lower(&field.inputmode);
    if field.tag.eq_ignore_ascii_case("textarea") {
        return Some("a text area is for a message, and a session writes none");
    }
    if !field.tag.eq_ignore_ascii_case("input") {
        return Some(NOT_A_FIELD);
    }
    match kind.as_str() {
        "password" => return Some("a password field is never typed into"),
        "email" => return Some("an email field is never typed into"),
        "text" | "search" | "number" | "tel" => {}
        // A date, an address of a site, a tick box, a file: none of them a
        // search term, a quantity or a postcode.
        _ => return Some(NOT_A_FIELD),
    }
    if inputmode == "email" {
        return Some("an email field is never typed into");
    }
    let autofill = lower(&field.autocomplete);
    if autofill.split_whitespace().any(|token| {
        PERSONAL_AUTOFILL.contains(&token) || PERSONAL_AUTOFILL_STARTS.iter().any(|start| token.starts_with(start))
    }) {
        return Some(PERSONAL);
    }
    // A search box is one whatever it suggests looking for.
    if kind == "search" {
        return None;
    }
    let said = [&field.name, &field.id, &field.placeholder, &field.label, &field.title]
        .map(lower)
        .join(" ");
    let postcode = says_postcode(&said);
    let personal = PERSONAL_WORDS.iter().filter(|word| said.contains(**word)).copied()
        .chain(PERSONAL_ALONE.iter().filter(|word| says_alone(&said, word)).copied())
        .chain(says(&said, "security", "code").then_some("security code"))
        // A postcode field inside an address form, or one that takes a
        // place as well, is still a postcode field.
        .any(|word| !(postcode && PLACES.contains(&word)));
    if personal {
        return Some(PERSONAL);
    }
    // A numeric keypad is how a shop asks for a postcode or a quantity; a
    // phone number it is not.
    let counted = COUNTED.iter().any(|word| said.contains(word)) || says(&said, "house", "number");
    if (kind == "tel" || inputmode == "tel") && !postcode && !counted {
        return Some("a phone keypad is typed into only for a postcode or a quantity");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: &str) -> Field {
        Field { tag: "INPUT".into(), kind: Some(kind.into()), ..Field::default() }
    }

    fn named(kind: &str, name: &str) -> Field {
        Field { name: Some(name.into()), ..input(kind) }
    }

    fn labelled(kind: &str, label: &str) -> Field {
        Field { label: Some(label.into()), ..input(kind) }
    }

    fn placeheld(kind: &str, placeholder: &str) -> Field {
        Field { placeholder: Some(placeholder.into()), ..input(kind) }
    }

    fn autocompleting(kind: &str, autocomplete: &str) -> Field {
        Field { autocomplete: Some(autocomplete.into()), ..input(kind) }
    }

    fn ok(field: Field, text: &str) {
        assert_eq!(typing_refused(&field, text), None, "{field:?}");
    }

    fn no(field: Field) {
        assert!(typing_refused(&field, "x").is_some(), "{field:?}");
    }

    #[test]
    fn a_search_term_a_quantity_a_postcode_and_a_house_number_may_be_typed() {
        ok(named("text", "postcode"), "1011 PN");
        ok(Field { autocomplete: Some("postal-code".into()), ..labelled("text", "Postcode") }, "1011 PN");
        ok(placeheld("search", "Zoeken"), "kattenvoer");
        ok(named("number", "quantity"), "2");
        ok(labelled("text", "Huisnummer"), "1");
        ok(autocompleting("text", "country-name"), "Nederland");
        // A field with no type at all is a text field.
        ok(Field { tag: "INPUT".into(), name: Some("q".into()), ..Field::default() }, "kattenvoer");
        // An address form's postcode says whose it is; it is a postcode still.
        ok(autocompleting("text", "shipping postal-code"), "1011 PN");
    }

    #[test]
    fn a_password_field_is_never_typed_into() {
        no(input("password"));
        no(input("PASSWORD"));
        no(autocompleting("text", "current-password"));
        no(autocompleting("text", "new-password"));
        no(autocompleting("text", "one-time-code"));
        for label in ["Wachtwoord", "Password", "Passwort", "Kennwort", "PIN", "Pin-code"] {
            no(labelled("text", label));
        }
    }

    #[test]
    fn a_payment_field_is_never_typed_into() {
        for autocomplete in ["cc-number", "cc-name", "cc-exp", "cc-csc", "billing cc-number"] {
            no(autocompleting("text", autocomplete));
        }
        no(named("text", "iban"));
        for label in ["Kartennummer", "Kaartnummer", "Card number", "Sicherheitscode", "Security code", "CVV", "CVC", "Rekeningnummer", "Kontonummer", "Prüfziffer"] {
            no(labelled("text", label));
        }
    }

    #[test]
    fn a_name_field_is_never_typed_into() {
        for autocomplete in ["name", "given-name", "additional-name", "family-name", "nickname", "honorific-prefix", "username", "organization", "organization-title"] {
            no(autocompleting("text", autocomplete));
        }
        no(labelled("text", "Voornaam"));
        no(Field { title: Some("Uw naam".into()), ..input("text") });
        no(Field { id: Some("visitorNameSearch".into()), ..input("text") });
        for label in ["Benutzername", "Firma", "Klantnummer", "Company", "Bedrijfsnaam"] {
            no(labelled("text", label));
        }
    }

    #[test]
    fn an_email_field_is_never_typed_into() {
        no(input("email"));
        no(autocompleting("text", "email"));
        no(named("text", "email_address"));
        no(labelled("text", "E-mailadres"));
        no(Field { inputmode: Some("email".into()), ..input("text") });
        // A messaging address is one too.
        no(autocompleting("text", "impp"));
    }

    #[test]
    fn a_phone_field_is_never_typed_into() {
        no(input("tel"));
        no(autocompleting("text", "tel"));
        no(autocompleting("text", "tel-national"));
        for label in ["Telefoonnummer", "Telefon", "Handynummer", "Mobile number", "Tel.", "GSM", "Rufnummer", "Phone"] {
            no(labelled("text", label));
        }
        // The prototype read "tel" as a word by JavaScript's idea of one, in
        // which an underscore joins two words into one.
        no(named("text", "tel_nr"));
        no(named("text", "customer-tel"));
        // A numeric keypad with nothing said of it is taken for a phone's.
        no(labelled("tel", "Nummer"));
        no(Field { inputmode: Some("tel".into()), ..labelled("text", "Nummer") });
    }

    #[test]
    fn a_street_address_field_is_never_typed_into() {
        for autocomplete in ["street-address", "address-line1", "address-level2", "shipping street-address"] {
            no(autocompleting("text", autocomplete));
        }
        no(placeheld("text", "Straatnaam"));
        for label in ["Ort", "Woonplaats", "Wohnort", "Plaats", "Postal address", "Straße", "Strasse", "Street", "Adres", "Stadt", "City", "Town", "Gemeente"] {
            no(labelled("text", label));
        }
    }

    #[test]
    fn whatever_else_a_page_marks_as_a_persons_is_refused_too() {
        for autocomplete in ["bday", "bday-year", "sex", "url", "photo"] {
            no(autocompleting("text", autocomplete));
        }
        for label in ["Geburtsdatum", "Geboortedatum", "Geboren am", "Date of birth", "DOB", "BSN", "Login"] {
            no(labelled("text", label));
        }
    }

    #[test]
    fn a_search_box_is_one_whatever_it_suggests_looking_for() {
        ok(placeheld("search", "Zoek op productnaam"), "kattenvoer");
        ok(placeheld("search", "Suche nach Marke, Artikel, Ort"), "lampe");
        // But not when the page itself marks it as personal.
        no(autocompleting("search", "email"));
        // The same suggestion on a plain text field is read as the field's subject.
        no(placeheld("text", "Zoek op productnaam"));
    }

    #[test]
    fn a_postcode_field_that_also_takes_a_place_is_a_postcode_field() {
        ok(labelled("text", "PLZ / Ort"), "1011 PN");
        ok(placeheld("text", "Postcode of plaats"), "1011 PN");
        ok(labelled("text", "Zip code or city"), "1011 PN");
        // The place alone is not, and a postcode beside a street is an address.
        no(labelled("text", "Plaats"));
        no(labelled("text", "Postcode en straat"));
    }

    #[test]
    fn a_numeric_keypad_is_for_a_postcode_or_a_count() {
        ok(named("tel", "postcode"), "1011");
        ok(labelled("tel", "Huisnummer"), "1");
        ok(labelled("tel", "Anzahl"), "2");
        ok(labelled("tel", "Hoeveelheid"), "2");
        ok(labelled("tel", "Aantal"), "2");
        ok(Field { inputmode: Some("tel".into()), ..labelled("text", "Aantal") }, "2");
    }

    #[test]
    fn at_most_64_characters_are_typed() {
        let field = named("text", "postcode");
        assert_eq!(typing_refused(&field, &"x".repeat(MAX_TYPED)), None);
        assert!(typing_refused(&field, &"x".repeat(MAX_TYPED + 1)).is_some());
        // Counted as characters, not bytes: 64 of these are 128 bytes.
        assert_eq!(typing_refused(&field, &"é".repeat(MAX_TYPED)), None);
        assert!(typing_refused(&field, &"é".repeat(MAX_TYPED + 1)).is_some());
        // Even into a search box.
        assert!(typing_refused(&input("search"), &"x".repeat(MAX_TYPED + 1)).is_some());
        // The sentence names the number the rule goes by.
        assert!(TOO_LONG.contains(&MAX_TYPED.to_string()));
    }

    #[test]
    fn only_a_field_that_takes_a_line_of_text_is_typed_into() {
        // A message to the shop is not a search term, a quantity or a postcode.
        no(Field { tag: "TEXTAREA".into(), name: Some("q".into()), ..Field::default() });
        no(Field { tag: "DIV".into(), ..Field::default() });
        for kind in ["checkbox", "radio", "file", "submit", "button", "hidden", "date", "url", "color", "range"] {
            no(named(kind, "q"));
        }
    }
}
