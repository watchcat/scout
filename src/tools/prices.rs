use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;

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

fn round_to(x: f64, decimals: i32) -> f64 {
    let f = 10f64.powi(decimals);
    (x * f).round() / f
}

fn round2(x: f64) -> f64 {
    round_to(x, 2)
}

/// Per-unit prices are shown to the user, so they need enough precision to
/// mean something: cents are fine for a blade, but "0.00 per gram" is not a
/// price. Below 10 cents a unit, show four decimals.
fn round_per_unit(x: f64) -> f64 {
    if x >= 0.10 { round_to(x, 2) } else { round_to(x, 4) }
}

/// A `Row` plus the value the ranking is actually done on: the exact,
/// unrounded landed price per unit. Rounding before comparing collapses
/// everything cheap — a gram of detergent, a sheet of paper — onto the same
/// number and then breaks the tie by input order, which is how a dearer
/// offer used to win.
struct Ranked {
    row: Row,
    per_unit: f64,
    /// Position in the input, the only dependable identity for an offer: the
    /// preamble forbids inventing URLs, so pack variants of one product page
    /// legitimately arrive with the same url.
    idx: usize,
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

    let mut ranked: Vec<Ranked> = args
        .offers
        .iter()
        .enumerate()
        .map(|(idx, o)| {
            let landed = o.price + o.shipping.unwrap_or(0.0);
            let per_unit = landed / o.units as f64;
            Ranked {
                per_unit,
                idx,
                row: Row {
                    title: o.title.clone(),
                    url: o.url.clone(),
                    shop: o.shop.clone(),
                    units: o.units,
                    price: round2(o.price),
                    shipping: o.shipping.map(round2),
                    landed: round2(landed),
                    per_unit: round_per_unit(per_unit),
                    shipping_known: o.shipping.is_some(),
                    condition: o.condition.clone(),
                    note: o.note.clone(),
                },
            }
        })
        .collect();
    ranked.sort_by(|a, b| a.per_unit.total_cmp(&b.per_unit));

    // Offers that hide shipping until checkout must not win a headline pick
    // over one whose full cost is known — unless nothing is known.
    let known: Vec<&Ranked> = ranked.iter().filter(|r| r.row.shipping_known).collect();
    let candidates: Vec<&Ranked> = if known.is_empty() { ranked.iter().collect() } else { known };

    let smallest = candidates.iter().map(|r| r.row.units).min().unwrap_or(1);
    // Within one pack size, cheapest per unit is cheapest landed.
    let best_single = candidates
        .iter()
        .filter(|r| r.row.units == smallest)
        .min_by(|a, b| a.per_unit.total_cmp(&b.per_unit))
        .expect("candidate set is non-empty");
    let best_per_unit = candidates
        .iter()
        .min_by(|a, b| a.per_unit.total_cmp(&b.per_unit))
        .expect("candidate set is non-empty");

    let bulk_advantage =
        best_per_unit.idx != best_single.idx && best_per_unit.per_unit < best_single.per_unit;
    let saving_vs_single_pct = if bulk_advantage && best_single.per_unit > 0.0 {
        ((best_single.per_unit - best_per_unit.per_unit) / best_single.per_unit * 100.0).round()
            as i64
    } else {
        0
    };
    let best_single = best_single.row.clone();
    let best_per_unit = best_per_unit.row.clone();

    let rows: Vec<Row> = ranked.iter().map(|r| r.row.clone()).collect();

    let mut notes = Vec::new();
    let unknown = rows.len() - rows.iter().filter(|r| r.shipping_known).count();
    if unknown == rows.len() {
        // Holding them out of the headline was impossible — they are the
        // headline — so saying they were held out would be false.
        notes.push(
            "no offer states shipping; every figure here excludes delivery and the picks are on \
             item price alone — say so in your reply"
                .to_string(),
        );
    } else if unknown > 0 {
        notes.push(format!(
            "{unknown} offer(s) do not state shipping; they are ranked on item price alone and \
             cannot be a headline pick — say so when you mention them"
        ));
    }
    if !bulk_advantage {
        // best_single is the cheapest of the smallest pack size available,
        // which is not always one unit — do not let the reply call a 3-pack a
        // one-off.
        notes.push(if best_single.units == 1 {
            "no bulk option beats buying one — tell the user that instead of inventing a second \
             pick"
                .to_string()
        } else {
            let n = best_single.units;
            format!(
                "no bulk option beats the cheapest {n}-pack, and no single unit is on offer here \
                 — call the pick the cheapest {n}-pack, not a one-off, and do not invent a second \
                 pick"
            )
        });
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

/// Stateless: all the inputs come from the model, all the rules are in
/// `compare`.
pub struct ComparePricesTool;

impl Tool for ComparePricesTool {
    const NAME: &'static str = "compare_prices";
    type Error = PriceCompareError;
    type Args = CompareArgs;
    type Output = Comparison;

    fn description(&self) -> String {
        "Rank product offers by real cost: landed price (item + shipping) and \
         price per unit. Call it ONCE with every candidate offer when the user \
         asks for the cheapest option or the best price. Returns the cheapest \
         way to buy one (best_single), the best price per unit (best_per_unit, \
         usually a multipack), a ranked table and notes. Use its numbers \
         verbatim — do not recompute them. Omit shipping for an offer when the \
         page does not state it; never guess it."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "unit_name": {
                    "type": "string",
                    "description": "what one unit is: blade, gram, piece, litre..."
                },
                "offers": {
                    "type": "array",
                    "description": "all candidate offers, same currency, counted in the same unit",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"},
                            "url": {"type": "string", "description": "direct link to the offer"},
                            "shop": {"type": "string", "description": "e.g. bol.com"},
                            "price": {"type": "number", "description": "listing price, no shipping"},
                            "currency": {"type": "string", "description": "e.g. EUR"},
                            "units": {
                                "type": "integer",
                                "description": "how many units the listing contains (3 for a 3-pack); default 1"
                            },
                            "shipping": {
                                "type": "number",
                                "description": "shipping cost when stated; omit entirely if unknown"
                            },
                            "condition": {"type": "string", "description": "new, used..."},
                            "note": {"type": "string", "description": "short caveat, e.g. ships from UK"}
                        },
                        "required": ["title", "url", "price", "currency"]
                    }
                }
            },
            "required": ["unit_name", "offers"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        compare(&args)
    }
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

    fn args_in(unit_name: &str, offers: Vec<Offer>) -> CompareArgs {
        CompareArgs { unit_name: unit_name.to_string(), offers }
    }

    #[test]
    fn sub_cent_units_are_ranked_on_the_exact_price_not_the_rounded_one() {
        // Both bags round to 0.00 per gram; the cheaper one must still win.
        let out = compare(&args_in(
            "gram",
            vec![offer("bag-4eur", 4.00, 1000, Some(0.0)), offer("bag-3eur", 3.00, 1000, Some(0.0))],
        ))
        .unwrap();

        assert_eq!(out.best_per_unit.title, "bag-3eur");
        assert_eq!(out.rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), vec![
            "bag-3eur", "bag-4eur"
        ]);
    }

    #[test]
    fn sub_cent_bulk_advantage_is_detected() {
        // 0.009 vs 0.0072 per sheet: both round to 0.01, the box is 20% cheaper.
        let out = compare(&args_in(
            "sheet",
            vec![offer("A4 500", 4.50, 500, Some(0.0)), offer("A4 2500 box", 18.00, 2500, Some(0.0))],
        ))
        .unwrap();

        assert_eq!(out.best_single.title, "A4 500");
        assert_eq!(out.best_per_unit.title, "A4 2500 box");
        assert!(out.bulk_advantage);
        assert_eq!(out.saving_vs_single_pct, 20);
        assert!(!out.notes.iter().any(|n| n.contains("no bulk option")));
    }

    #[test]
    fn pack_variants_sharing_one_url_can_still_show_a_bulk_advantage() {
        // One product page, two pack options: the preamble forbids inventing
        // URLs, so this is what the model legitimately sends.
        let same_url = |title: &str, price: f64, units: u32| Offer {
            url: "https://shop.example/p".to_string(),
            ..offer(title, price, units, Some(0.0))
        };
        let out = compare(&args(vec![same_url("single", 10.0, 1), same_url("3-pack", 15.0, 3)])).unwrap();

        assert_eq!(out.best_single.title, "single");
        assert_eq!(out.best_per_unit.title, "3-pack");
        assert!(out.bulk_advantage, "different picks must not report no bulk advantage");
        assert_eq!(out.saving_vs_single_pct, 50);
        assert!(!out.notes.iter().any(|n| n.contains("no bulk option")));
    }

    #[test]
    fn saving_pct_uses_exact_unit_prices() {
        // 0.025 vs 0.020 per gram is a 20% saving, not the 33% the rounded
        // values (0.03 vs 0.02) suggest.
        let out = compare(&args_in(
            "gram",
            vec![offer("1kg", 25.00, 1000, Some(0.0)), offer("5kg", 100.00, 5000, Some(0.0))],
        ))
        .unwrap();

        assert!(out.bulk_advantage);
        assert_eq!(out.saving_vs_single_pct, 20);
    }

    #[test]
    fn per_unit_divides_the_unrounded_landed_sum() {
        // landed rounds to 10.01, but 10.005 / 2 is 5.0025 → 5.00, not 5.01.
        let out = compare(&args(vec![offer("odd", 10.005, 2, Some(0.0))])).unwrap();

        assert_eq!(out.rows[0].landed, 10.01);
        assert_eq!(out.rows[0].per_unit, 5.00);
    }

    #[test]
    fn per_unit_precision_adapts_to_the_size_of_the_unit_price() {
        // >= 0.10 per unit: cents are enough.
        let cents = compare(&args_in("gram", vec![
            offer("a", 12.345, 100, Some(0.0)),
            offer("boundary", 10.0, 100, Some(0.0)),
        ]))
        .unwrap();
        assert_eq!(cents.rows.iter().find(|r| r.title == "a").unwrap().per_unit, 0.12);
        assert_eq!(cents.rows.iter().find(|r| r.title == "boundary").unwrap().per_unit, 0.1);

        // < 0.10 per unit: cents would print "0.00 per gram".
        let sub = compare(&args_in("gram", vec![
            offer("just under", 9.9, 100, Some(0.0)),
            offer("tiny", 3.0, 1000, Some(0.0)),
        ]))
        .unwrap();
        assert_eq!(sub.rows.iter().find(|r| r.title == "just under").unwrap().per_unit, 0.099);
        assert_eq!(sub.rows.iter().find(|r| r.title == "tiny").unwrap().per_unit, 0.003);
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
    fn no_bulk_note_names_the_smallest_pack_when_no_single_is_available() {
        // The only offer with known shipping is a 3-pack, so best_single is a
        // 3-pack; "no bulk option beats buying one" describes a purchase that
        // was never on the table.
        let out = compare(&args(vec![
            offer("single", 5.0, 1, None),
            offer("3-pack", 20.0, 3, Some(2.0)),
        ]))
        .unwrap();

        assert_eq!(out.best_single.title, "3-pack");
        assert_eq!(out.best_single.units, 3);
        assert!(!out.bulk_advantage);
        assert!(!out.notes.iter().any(|n| n.contains("buying one")), "got: {:?}", out.notes);
        assert!(
            out.notes.iter().any(|n| n.contains("no bulk option") && n.contains("3-pack")),
            "got: {:?}",
            out.notes
        );
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
        // Here the wording is true: a fully-known offer took the headline.
        assert!(
            out.notes.iter().any(|n| n.contains("cannot be a headline pick")),
            "got: {:?}",
            out.notes
        );
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
        // The held-out-of-the-headline note would be a lie: these offers ARE
        // the headline picks, because nothing better exists.
        assert!(
            !out.notes.iter().any(|n| n.contains("cannot be a headline pick")),
            "got: {:?}",
            out.notes
        );
        assert!(
            out.notes.iter().any(|n| n.contains("no offer states shipping")),
            "got: {:?}",
            out.notes
        );
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

    #[tokio::test]
    async fn tool_accepts_the_model_facing_json() {
        let args: CompareArgs = serde_json::from_value(serde_json::json!({
            "unit_name": "blade",
            "offers": [
                {"title": "3 pack", "url": "https://e.com/3", "price": 15.17,
                 "currency": "EUR", "units": 3, "shipping": 16.98},
                {"title": "single", "url": "https://e.com/1", "price": 35.25,
                 "currency": "EUR", "shipping": 2.21}
            ]
        }))
        .unwrap();

        // units defaults to 1 when the model omits it
        assert_eq!(args.offers[1].units, 1);

        let out = ComparePricesTool.call(args).await.unwrap();
        assert_eq!(out.best_per_unit.title, "3 pack");
        assert_eq!(out.best_single.title, "single");
        assert_eq!(ComparePricesTool::NAME, "compare_prices");
    }
}
