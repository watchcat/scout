use serde::{Deserialize, Serialize};

/// One purchasable offer as the model extracted it. `units` is how many
/// countable things the listing contains (3 for a 3-pack); `shipping` absent
/// means "not stated", never "free".
#[derive(Debug, Deserialize)]
pub struct Offer {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub shop: Option<String>,
    pub price: f64,
    pub currency: String,
    #[serde(default = "one")]
    pub units: u32,
    #[serde(default)]
    pub shipping: Option<f64>,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Deserialize)]
pub struct CompareArgs {
    /// What one unit is: "blade", "gram", "piece". Free text, echoed back.
    pub unit_name: String,
    pub offers: Vec<Offer>,
}

/// One offer with the comparison arithmetic applied.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Row {
    pub title: String,
    pub url: String,
    pub shop: Option<String>,
    pub units: u32,
    pub price: f64,
    pub shipping: Option<f64>,
    /// price + shipping; equals price when shipping is unknown.
    pub landed: f64,
    pub per_unit: f64,
    pub shipping_known: bool,
    pub condition: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Comparison {
    pub unit_name: String,
    pub currency: String,
    /// Cheapest way to buy the smallest available quantity.
    pub best_single: Row,
    /// Lowest price per unit anywhere in the set.
    pub best_per_unit: Row,
    /// Percent saved per unit by taking `best_per_unit` over `best_single`;
    /// 0 when they are the same offer.
    pub saving_vs_single_pct: i64,
    /// True when buying the bigger pack genuinely costs less per unit.
    pub bulk_advantage: bool,
    /// Every offer, cheapest per unit first.
    pub rows: Vec<Row>,
    pub notes: Vec<String>,
}

/// Returned to the model as the tool's error text, so it says how to fix the
/// call rather than just what went wrong.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PriceCompareError(pub String);

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Everything that would make a comparison meaningless rather than merely
/// imprecise. The messages are instructions to the model, not diagnostics.
fn validate(args: &CompareArgs) -> Result<(), PriceCompareError> {
    let bad = |m: String| Err(PriceCompareError(m));
    if args.offers.is_empty() {
        return bad("give at least one offer to compare".to_string());
    }
    let currency = args.offers[0].currency.trim().to_uppercase();
    for o in &args.offers {
        if o.url.trim().is_empty() {
            return bad(format!("offer {:?} has no url — every offer needs its link", o.title));
        }
        if o.currency.trim().to_uppercase() != currency {
            return bad(format!(
                "all offers must share the same currency (got {currency} and {}); compare one \
                 currency at a time and mention the others separately",
                o.currency.trim().to_uppercase()
            ));
        }
        if o.units < 1 {
            return bad(format!(
                "offer {:?} has units {} — units is how many items the listing contains, at least 1",
                o.title, o.units
            ));
        }
        if !o.price.is_finite() || o.price < 0.0 {
            return bad(format!("offer {:?} has an unusable price {}", o.title, o.price));
        }
        if let Some(s) = o.shipping {
            if !s.is_finite() || s < 0.0 {
                return bad(format!(
                    "offer {:?} has an unusable shipping cost {s} — omit shipping when it is not \
                     stated instead of guessing",
                    o.title
                ));
            }
        }
    }
    Ok(())
}

/// Rank offers by landed cost per unit. Pure: no I/O, no model, no clock.
pub fn compare(args: &CompareArgs) -> Result<Comparison, PriceCompareError> {
    validate(args)?;
    let currency = args.offers[0].currency.trim().to_uppercase();

    let mut rows: Vec<Row> = args
        .offers
        .iter()
        .map(|o| {
            let landed = round2(o.price + o.shipping.unwrap_or(0.0));
            Row {
                title: o.title.clone(),
                url: o.url.clone(),
                shop: o.shop.clone(),
                units: o.units,
                price: round2(o.price),
                shipping: o.shipping.map(round2),
                landed,
                per_unit: round2(landed / o.units as f64),
                shipping_known: o.shipping.is_some(),
                condition: o.condition.clone(),
                note: o.note.clone(),
            }
        })
        .collect();
    rows.sort_by(|a, b| a.per_unit.total_cmp(&b.per_unit));

    // Offers that hide shipping until checkout must not win a headline pick
    // over one whose full cost is known — unless nothing is known.
    let known: Vec<&Row> = rows.iter().filter(|r| r.shipping_known).collect();
    let candidates: Vec<&Row> = if known.is_empty() { rows.iter().collect() } else { known };

    let smallest = candidates.iter().map(|r| r.units).min().unwrap_or(1);
    let best_single = candidates
        .iter()
        .filter(|r| r.units == smallest)
        .min_by(|a, b| a.landed.total_cmp(&b.landed))
        .map(|r| (*r).clone())
        .expect("candidate set is non-empty");
    let best_per_unit = candidates
        .iter()
        .min_by(|a, b| a.per_unit.total_cmp(&b.per_unit))
        .map(|r| (*r).clone())
        .expect("candidate set is non-empty");

    let bulk_advantage =
        best_per_unit.url != best_single.url && best_per_unit.per_unit < best_single.per_unit;
    let saving_vs_single_pct = if bulk_advantage && best_single.per_unit > 0.0 {
        ((best_single.per_unit - best_per_unit.per_unit) / best_single.per_unit * 100.0).round()
            as i64
    } else {
        0
    };

    let mut notes = Vec::new();
    let unknown = rows.len() - rows.iter().filter(|r| r.shipping_known).count();
    if unknown > 0 {
        notes.push(format!(
            "{unknown} offer(s) do not state shipping; they are ranked on item price alone and \
             cannot be a headline pick — say so when you mention them"
        ));
    }
    if !bulk_advantage {
        notes.push(
            "no bulk option beats buying one — tell the user that instead of inventing a second pick"
                .to_string(),
        );
    }

    Ok(Comparison {
        unit_name: args.unit_name.trim().to_string(),
        currency,
        best_single,
        best_per_unit,
        saving_vs_single_pct,
        bulk_advantage,
        rows,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(title: &str, price: f64, units: u32, shipping: Option<f64>) -> Offer {
        Offer {
            title: title.to_string(),
            url: format!("https://shop.example/{title}"),
            shop: Some("shop.example".to_string()),
            price,
            currency: "EUR".to_string(),
            units,
            shipping,
            condition: None,
            note: None,
        }
    }

    fn args(offers: Vec<Offer>) -> CompareArgs {
        CompareArgs { unit_name: "blade".to_string(), offers }
    }

    #[test]
    fn ranks_by_landed_price_per_unit() {
        // Real eBay numbers: the sticker-cheapest listing is not the
        // landed-cheapest, and the multipack wins only per unit.
        let out = compare(&args(vec![
            offer("single", 35.25, 1, Some(2.21)),
            offer("three-pack", 15.17, 3, Some(16.98)),
        ]))
        .unwrap();

        assert_eq!(out.best_single.title, "single");
        assert_eq!(out.best_single.landed, 37.46);
        assert_eq!(out.best_single.per_unit, 37.46);

        assert_eq!(out.best_per_unit.title, "three-pack");
        assert_eq!(out.best_per_unit.landed, 32.15);
        assert_eq!(out.best_per_unit.per_unit, 10.72);

        assert!(out.bulk_advantage);
        assert_eq!(out.saving_vs_single_pct, 71);
        // rows are ordered cheapest-per-unit first
        assert_eq!(
            out.rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(),
            vec!["three-pack", "single"]
        );
        assert!(out.rows.iter().all(|r| r.shipping_known));
    }

    #[test]
    fn no_bulk_advantage_is_reported_not_faked() {
        // The 2-pack is more expensive per unit than the single.
        let out = compare(&args(vec![
            offer("single", 10.0, 1, Some(0.0)),
            offer("two-pack", 25.0, 2, Some(0.0)),
        ]))
        .unwrap();

        assert!(!out.bulk_advantage);
        assert_eq!(out.saving_vs_single_pct, 0);
        assert_eq!(out.best_per_unit.title, "single");
        assert_eq!(out.best_single.title, "single");
        assert!(out.notes.iter().any(|n| n.contains("no bulk option")));

        // A tie is not an advantage either: the same price per unit means
        // there is nothing to gain from the bigger pack.
        let tie = compare(&args(vec![
            offer("single", 10.0, 1, Some(0.0)),
            offer("two-pack", 20.0, 2, Some(0.0)),
        ]))
        .unwrap();
        assert!(!tie.bulk_advantage);
        assert_eq!(tie.saving_vs_single_pct, 0);
    }

    #[test]
    fn unknown_shipping_cannot_take_a_headline_pick() {
        // Cheapest on sticker price, but shipping is unstated.
        let out = compare(&args(vec![
            offer("mystery-shipping", 5.0, 1, None),
            offer("known", 9.0, 1, Some(1.0)),
        ]))
        .unwrap();

        assert_eq!(out.best_single.title, "known");
        assert_eq!(out.best_per_unit.title, "known");
        // it is still listed, first even, because it is cheapest per unit
        assert_eq!(out.rows[0].title, "mystery-shipping");
        assert!(!out.rows[0].shipping_known);
        assert!(out.notes.iter().any(|n| n.contains("do not state shipping")));
    }

    #[test]
    fn all_shipping_unknown_still_produces_picks() {
        let out = compare(&args(vec![
            offer("a", 20.0, 1, None),
            offer("b", 30.0, 4, None),
        ]))
        .unwrap();

        assert_eq!(out.best_single.title, "a");
        assert_eq!(out.best_per_unit.title, "b"); // 7.50/unit
        assert!(out.bulk_advantage);
    }

    #[test]
    fn rejects_unusable_input() {
        let empty = compare(&args(vec![])).unwrap_err();
        assert!(empty.to_string().contains("at least one offer"), "got: {empty}");

        let mixed = compare(&args(vec![
            offer("eur", 10.0, 1, None),
            Offer { currency: "USD".to_string(), ..offer("usd", 10.0, 1, None) },
        ]))
        .unwrap_err();
        assert!(mixed.to_string().contains("same currency"), "got: {mixed}");

        let zero_units = compare(&args(vec![offer("bad", 10.0, 0, None)])).unwrap_err();
        assert!(zero_units.to_string().contains("units"), "got: {zero_units}");

        let negative = compare(&args(vec![offer("bad", -1.0, 1, None)])).unwrap_err();
        assert!(negative.to_string().contains("price"), "got: {negative}");

        let bad_shipping = compare(&args(vec![offer("bad", 1.0, 1, Some(f64::NAN))])).unwrap_err();
        assert!(bad_shipping.to_string().contains("shipping"), "got: {bad_shipping}");

        let no_url = compare(&args(vec![Offer { url: "  ".to_string(), ..offer("bad", 1.0, 1, None) }]))
            .unwrap_err();
        assert!(no_url.to_string().contains("url"), "got: {no_url}");
    }
}
