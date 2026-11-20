use super::delivery::{DeliveryDesk, Looked, Terms, NEW_SHOPS_PER_REQUEST};
use super::fetch;
use reqwest::Url;
use rig::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

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
    /// Whether `landed` includes a delivery cost, whoever's word it is.
    pub shipping_known: bool,
    /// Whose word `shipping` is: `offer` when the model handed it over as
    /// the listing's, `delivery_page` when it is the shop's kept delivery
    /// terms for an order of this value, `unknown` when nobody's.
    pub shipping_source: &'static str,
    /// With `delivery_page`: the page of the shop the cost was read from.
    pub delivery_page: Option<String>,
    /// With `delivery_page`: the order value the shop delivers free from,
    /// when this offer is under it.
    pub free_from: Option<f64>,
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
    /// The offer's shop, as the delivery desk keys one.
    host: Option<String>,
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
    if args.unit_name.trim().is_empty() {
        return bad(
            "unit_name is empty — pass what one unit is (blade, gram, litre, piece); it is \
             printed next to every price"
                .to_string(),
        );
    }
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
    compare_with(args, &Looked::default())
}

/// An offer's shop, as the delivery desk keys one.
fn shop_of(offer: &Offer) -> Option<String> {
    Url::parse(offer.url.trim()).ok().as_ref().and_then(fetch::host_key)
}

/// The addresses of the offers that state no shipping, cheapest first: by
/// item price per unit, which is all that is known of them yet. The desk
/// reads a few new shops a request, so the offers that decide the pick are
/// the ones asked about when it cannot read them all.
fn unstated_cheapest_first(args: &CompareArgs) -> Vec<Url> {
    let mut unstated: Vec<&Offer> = args.offers.iter().filter(|o| o.shipping.is_none()).collect();
    unstated.sort_by(|a, b| (a.price / a.units as f64).total_cmp(&(b.price / b.units as f64)));
    unstated.iter().filter_map(|o| Url::parse(o.url.trim()).ok()).collect()
}

/// What a shop's kept terms make of delivering one offer.
struct FromPage {
    cost: f64,
    page: Option<String>,
    free_from: Option<f64>,
}

/// The delivery cost of an offer that states none, by its shop's kept
/// terms: the band its order value falls in, where the terms are in the
/// comparison's currency. The order value is the offer's price, one of
/// that listing bought alone — `units` is how much the listing holds, not
/// how many of it are ordered.
fn from_delivery_page(offer: &Offer, host: Option<&String>, currency: &str, looked: &Looked) -> Option<FromPage> {
    let kept = looked.by_host.get(host?)?;
    let Terms::Costs { bands, currency: theirs } = &kept.terms else { return None };
    if theirs != currency {
        return None;
    }
    Some(FromPage {
        cost: bands.cost_for(offer.price)?,
        page: kept.source_url.clone(),
        free_from: bands.free_from().filter(|from| offer.price < *from),
    })
}

/// `compare`, with what the delivery desk found for the shops of the
/// offers that state no shipping. Still pure: the looking up is done.
///
/// A stated shipping is never replaced, whatever the shop's page says: the
/// listing is nearer the product than the shop's general terms are. An
/// offer that states none gets its shop's cost for an order of its value,
/// is ranked on it, and says in `shipping_source` and in a note that the
/// cost is the delivery page's word and not the listing's.
pub fn compare_with(args: &CompareArgs, looked: &Looked) -> Result<Comparison, PriceCompareError> {
    validate(args)?;
    let currency = args.offers[0].currency.trim().to_uppercase();

    let mut ranked: Vec<Ranked> = args
        .offers
        .iter()
        .enumerate()
        .map(|(idx, o)| {
            let host = shop_of(o);
            let from_page = match o.shipping {
                Some(_) => None,
                None => from_delivery_page(o, host.as_ref(), &currency, looked),
            };
            let (shipping, shipping_source) = match (o.shipping, &from_page) {
                (Some(stated), _) => (Some(stated), "offer"),
                (None, Some(found)) => (Some(found.cost), "delivery_page"),
                (None, None) => (None, "unknown"),
            };
            let landed = o.price + shipping.unwrap_or(0.0);
            let per_unit = landed / o.units as f64;
            Ranked {
                per_unit,
                idx,
                host,
                row: Row {
                    title: o.title.clone(),
                    url: o.url.clone(),
                    shop: o.shop.clone(),
                    units: o.units,
                    price: round2(o.price),
                    shipping: shipping.map(round2),
                    landed: round2(landed),
                    per_unit: round_per_unit(per_unit),
                    shipping_known: shipping.is_some(),
                    shipping_source,
                    free_from: from_page.as_ref().and_then(|found| found.free_from).map(round2),
                    delivery_page: from_page.and_then(|found| found.page),
                    condition: o.condition.clone(),
                    note: o.note.clone(),
                },
            }
        })
        .collect();
    ranked.sort_by(|a, b| a.per_unit.total_cmp(&b.per_unit));

    // Every offer competes, including those that state no shipping cost: an
    // unstated delivery fee is a reason to label a pick, not to bury it —
    // most shops never state one until checkout, and hiding them would hide
    // the cheapest option most of the time.
    let candidates: Vec<&Ranked> = ranked.iter().collect();

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

    let delivery_notes = delivery_notes(&ranked, looked);
    let rows: Vec<Row> = ranked.into_iter().map(|r| r.row).collect();

    let mut notes = Vec::new();
    // An offer priced from the shop's delivery page is not item-only, so
    // these two go by what nobody knows.
    let unknown = rows.iter().filter(|r| !r.shipping_known).count();
    if unknown == rows.len() {
        notes.push(
            "no offer states shipping; every figure here is item-only and excludes delivery — \
             say so in your reply"
                .to_string(),
        );
    } else if unknown > 0 {
        let named: Vec<&str> = rows
            .iter()
            .filter(|r| !r.shipping_known)
            .map(|r| r.title.as_str())
            .take(5)
            .collect();
        notes.push(format!(
            "item-only (shipping not stated): {} — present them normally but mark the price as \
             excluding delivery",
            named.join("; ")
        ));
    }
    notes.extend(delivery_notes);
    // The same page cannot be two different packs at two different prices;
    // one of them was pasted from another listing.
    for (i, row) in rows.iter().enumerate() {
        if let Some(twin) = rows[i + 1..]
            .iter()
            .find(|other| other.url == row.url && other.units != row.units)
        {
            notes.push(format!(
                "{:?} and {:?} share one url ({}) but claim different pack sizes ({} vs {}) — at \
                 most one is right; re-check the page before presenting either",
                row.title, twin.title, row.url, row.units, twin.units
            ));
            break;
        }
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

/// What the reply has to say of delivery that the desk looked up: whose
/// word an added cost is, which shop will not deliver, which charges by
/// something an offer does not say, and which nobody read. No date is
/// given for any of it: terms read for one person answer the next, and a
/// date would tell the second when somebody looked at that shop.
fn delivery_notes(ranked: &[Ranked], looked: &Looked) -> Vec<String> {
    let mut notes = Vec::new();
    let added: Vec<String> = ranked
        .iter()
        .filter(|r| r.row.shipping_source == "delivery_page")
        .take(5)
        .map(|r| {
            let cost = match r.row.shipping {
                Some(cost) if cost > 0.0 => format!("+{cost:.2}"),
                _ => "free".to_string(),
            };
            let host = r.host.as_deref().unwrap_or_default();
            match r.row.free_from {
                Some(from) => format!("{} {cost} ({host}; free from {from:.2})", r.row.title),
                None => format!("{} {cost} ({host})", r.row.title),
            }
        })
        .collect();
    if !added.is_empty() {
        notes.push(format!(
            "delivery added from the shop's own delivery page, not from the listing: {} — say \"delivery per the \
             shop's delivery page\" for these, never that the listing states it",
            added.join("; ")
        ));
    }
    // Once a shop, in the order its offers rank, and only for a shop whose
    // terms were asked for: one with an offer that states no shipping.
    let mut said: Vec<&str> = Vec::new();
    for r in ranked.iter().filter(|r| r.row.shipping_source == "unknown") {
        let Some(host) = r.host.as_deref().filter(|host| !said.contains(host)) else { continue };
        let Some(kept) = looked.by_host.get(host) else { continue };
        said.push(host);
        match &kept.terms {
            Terms::NotDelivered => {
                let country = looked.country.map_or("", |c| c.english);
                notes.push(format!(
                    "{host}: its delivery page says it does not deliver to the delivery country ({country}) — say so \
                     beside that option"
                ));
            }
            Terms::Depends => {
                let terms = kept.source_url.as_deref().unwrap_or(host);
                notes.push(format!("{host}: delivery is charged by weight or destination, so none was added — its terms: {terms}"));
            }
            _ => {}
        }
    }
    if !looked.not_looked_up.is_empty() {
        notes.push(format!(
            "delivery was not looked up at {}: {NEW_SHOPS_PER_REQUEST} new shops a request — those prices stay item-only",
            looked.not_looked_up.join(", ")
        ));
    }
    notes
}

/// All the inputs come from the model and all the rules are in
/// `compare_with`. The one thing the tool does besides is ask the delivery
/// desk about the shops of the offers that state no shipping — after the
/// call has been found well-formed, so that a call the model has to make
/// again costs no shop a request. Without a desk it is `compare`.
#[derive(Default)]
pub struct ComparePricesTool {
    pub delivery: Option<Arc<DeliveryDesk>>,
}

impl Tool for ComparePricesTool {
    const NAME: &'static str = "compare_prices";
    type Error = PriceCompareError;
    type Args = CompareArgs;
    type Output = Comparison;

    fn description(&self) -> String {
        "Rank product offers by real cost: landed price (item + shipping) and \
         price per unit. Call it ONCE with every candidate offer when the user \
         asks for the cheapest option or the best price. Returns the cheapest \
         way to buy the smallest pack in the set (best_single — that is not \
         necessarily one unit, check its units field before calling it a \
         one-off), the best price per unit (best_per_unit, usually a \
         multipack), a ranked table and notes. Use its numbers \
         verbatim — do not recompute them. Omit shipping for an offer when the \
         page does not state it; never guess it. An offer sent without \
         shipping may come back with a cost Scout read from the shop's own \
         delivery page: its row is marked shipping_source: delivery_page, \
         and the notes say how to word it."
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
                                "description": "how many unit_name this listing contains in total \
                                    — a 3-pack of blades is 3; with unit_name=gram a 500 g bag is \
                                    500; a box of 4 packs of 5 blades is 20. Every offer in the \
                                    call must count in the same unit. Default 1"
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
        validate(&args)?;
        let Some(desk) = &self.delivery else { return compare(&args) };
        let looked = desk.terms_for(&unstated_cheapest_first(&args)).await;
        let comparison = compare_with(&args, &looked)?;
        for source in ["offer", "delivery_page", "unknown"] {
            desk.count_compared(source, comparison.rows.iter().filter(|r| r.shipping_source == source).count());
        }
        Ok(comparison)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::delivery::fixtures::{World, DELIVERY, PRODUCT};
    use crate::tools::delivery::{self, Bands, Kept, Terms};
    use std::collections::HashMap;

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
            vec![
                offer("bag-4eur", 4.00, 1000, Some(0.0)),
                offer("bag-3eur", 3.00, 1000, Some(0.0)),
            ],
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
            vec![
                offer("A4 500", 4.50, 500, Some(0.0)),
                offer("A4 2500 box", 18.00, 2500, Some(0.0)),
            ],
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
        let out =
            compare(&args(vec![same_url("single", 10.0, 1), same_url("3-pack", 15.0, 3)])).unwrap();

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
        // Nobody sells this as a single: the smallest pack is 3, so
        // "no bulk option beats buying one" would describe a purchase that
        // was never on the table.
        let out = compare(&args(vec![
            offer("3-pack", 20.0, 3, Some(2.0)),
            offer("6-pack", 60.0, 6, Some(2.0)),
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
    fn unknown_shipping_still_wins_the_headline_but_is_named() {
        // Cheapest on item price, shipping unstated. Most shops never state
        // one before checkout, so demoting it would hide the cheapest option
        // as a matter of course.
        let out = compare(&args(vec![
            offer("mystery-shipping", 5.0, 1, None),
            offer("known", 9.0, 1, Some(1.0)),
        ]))
        .unwrap();

        assert_eq!(out.best_single.title, "mystery-shipping");
        assert_eq!(out.best_per_unit.title, "mystery-shipping");
        assert!(!out.rows[0].shipping_known);
        // ...but the reply has to say the price excludes delivery, and which
        // offer that applies to.
        assert!(
            out.notes
                .iter()
                .any(|n| n.contains("item-only") && n.contains("mystery-shipping")),
            "got: {:?}",
            out.notes
        );
    }

    #[test]
    fn one_url_claiming_two_pack_sizes_is_flagged() {
        // Exactly the production slip: a 9 kg pick carrying the URL of a
        // 2.7 kg listing, alongside that listing itself.
        let out = compare(&args(vec![
            Offer {
                url: "https://bol.com/p/vanish-2-7-kg/".to_string(),
                ..offer("6 x 1.5 kg", 44.99, 9000, None)
            },
            Offer {
                url: "https://bol.com/p/vanish-2-7-kg/".to_string(),
                ..offer("2.7 kg", 13.90, 2700, None)
            },
        ]))
        .unwrap();

        assert!(
            out.notes.iter().any(|n| n.contains("share one url")
                && n.contains("9000")
                && n.contains("2700")),
            "got: {:?}",
            out.notes
        );
        // Both still rank; the model is told to re-check, not to guess.
        assert_eq!(out.rows.len(), 2);
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

        // Otherwise the reply reads "€10.72 per ".
        let blank_unit =
            compare(&args_in("  ", vec![offer("ok", 1.0, 1, Some(0.0))])).unwrap_err();
        assert!(blank_unit.to_string().contains("unit_name"), "got: {blank_unit}");
    }

    #[test]
    fn model_facing_text_ties_units_to_unit_name_and_explains_best_single() {
        let params = ComparePricesTool::default().parameters();
        let units = params["properties"]["offers"]["items"]["properties"]["units"]["description"]
            .as_str()
            .expect("units has a description");
        // Without this the model sends units=1 for a 500 g bag and units=500
        // for the next one, and the tool cannot detect the mix.
        assert!(units.contains("unit_name"), "got: {units}");
        assert!(units.contains("500"), "got: {units}");
        assert!(units.contains("same unit"), "got: {units}");

        // best_single is the smallest pack available, not necessarily one.
        let description = ComparePricesTool::default().description();
        assert!(description.contains("smallest pack"), "got: {description}");
        // An offer sent without shipping can come back with one, and the
        // model has to know whose word it is before it reads the notes.
        assert!(description.contains("shipping_source"), "got: {description}");
        assert!(description.contains("the shop's own delivery page"), "got: {description}");
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

        let out = ComparePricesTool::default().call(args).await.unwrap();
        assert_eq!(out.best_per_unit.title, "3 pack");
        assert_eq!(out.best_single.title, "single");
        assert_eq!(ComparePricesTool::NAME, "compare_prices");
    }

    const DELIVERY_PAGE: &str = "https://shop.example/verzendkosten";

    /// What the desk hands back when `shop.example`'s terms are kept, for
    /// delivery to the Netherlands.
    fn looked(terms: Terms) -> Looked {
        Looked {
            by_host: [("shop.example".to_string(), Kept { terms, source_url: Some(DELIVERY_PAGE.to_string()) })].into(),
            not_looked_up: Vec::new(),
            country: delivery::country("NL"),
        }
    }

    /// 5.49 an order, free from 40.
    fn by_order_value() -> Terms {
        Terms::Costs { bands: Bands::new(vec![(0.0, 5.49), (40.0, 0.0)]).unwrap(), currency: "EUR".to_string() }
    }

    /// An offer at another shop, of which nothing is kept.
    fn elsewhere(title: &str, price: f64, shipping: Option<f64>) -> Offer {
        Offer { url: format!("https://other.example/{title}"), shop: Some("other.example".to_string()), ..offer(title, price, 1, shipping) }
    }

    fn row<'a>(out: &'a Comparison, title: &str) -> &'a Row {
        out.rows.iter().find(|r| r.title == title).unwrap_or_else(|| panic!("no row {title:?}"))
    }

    #[test]
    fn kept_terms_price_an_offer_that_states_no_shipping() {
        // On the item's price the shop's widget wins. Delivered, it does not.
        let offers = || vec![offer("at-the-shop", 10.0, 1, None), elsewhere("delivered", 12.0, Some(1.0))];
        let item_only = compare(&args(offers())).unwrap();
        assert_eq!(item_only.best_single.title, "at-the-shop", "as it was ranked before");

        let out = compare_with(&args(offers()), &looked(by_order_value())).unwrap();

        let priced = row(&out, "at-the-shop");
        assert_eq!((priced.price, priced.shipping, priced.landed, priced.per_unit), (10.0, Some(5.49), 15.49, 15.49));
        assert!(priced.shipping_known, "its landed price includes delivery now");
        assert_eq!(out.best_single.title, "delivered", "and it is ranked on that");
        assert_eq!(out.rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>(), vec!["delivered", "at-the-shop"]);
        assert!(!out.notes.iter().any(|n| n.contains("item-only") || n.contains("no offer states shipping")), "got: {:?}", out.notes);

        // The cost is the order's, so a pack carries it once.
        let out = compare_with(&args(vec![offer("3-pack", 30.0, 3, None)]), &looked(by_order_value())).unwrap();
        assert_eq!((out.rows[0].landed, out.rows[0].per_unit), (35.49, 11.83));
    }

    #[test]
    fn an_offer_at_the_free_value_is_delivered_free_and_one_under_it_says_from_what() {
        let out = compare_with(&args(vec![offer("under", 39.99, 1, None), offer("at", 40.0, 1, None)]), &looked(by_order_value())).unwrap();

        let under = row(&out, "under");
        assert_eq!((under.shipping, under.landed, under.free_from), (Some(5.49), 45.48, Some(40.0)));
        let at = row(&out, "at");
        assert_eq!((at.shipping, at.landed, at.free_from), (Some(0.0), 40.0, None), "nothing to say of a value it has reached");
        assert_eq!(at.shipping_source, "delivery_page", "free by the shop's page, not by the listing");
        assert_eq!(out.best_single.title, "at", "the dearer item is the cheaper order");

        // Under the shop's first band it states no cost, and none is made up.
        let from_ten = Terms::Costs { bands: Bands::new(vec![(10.0, 5.49), (40.0, 0.0)]).unwrap(), currency: "EUR".to_string() };
        let out = compare_with(&args(vec![offer("small", 8.0, 1, None)]), &looked(from_ten)).unwrap();
        assert_eq!((out.rows[0].shipping, out.rows[0].shipping_source, out.rows[0].landed), (None, "unknown", 8.0));
        assert!(out.notes.iter().any(|n| n.contains("no offer states shipping")), "got: {:?}", out.notes);
    }

    #[test]
    fn a_stated_shipping_is_never_replaced() {
        // The shop's page says 5.49; the listing said 2.50, and free.
        let out = compare_with(
            &args(vec![offer("stated", 10.0, 1, Some(2.5)), offer("stated-free", 11.0, 1, Some(0.0))]),
            &looked(by_order_value()),
        )
        .unwrap();

        let stated = row(&out, "stated");
        assert_eq!((stated.shipping, stated.landed, stated.shipping_source), (Some(2.5), 12.5, "offer"));
        assert_eq!((stated.delivery_page.as_deref(), stated.free_from), (None, None));
        let free = row(&out, "stated-free");
        assert_eq!((free.shipping, free.landed, free.shipping_source), (Some(0.0), 11.0, "offer"));
        assert!(!out.notes.iter().any(|n| n.contains("delivery page")), "got: {:?}", out.notes);
    }

    #[test]
    fn a_cost_from_the_delivery_page_is_named_as_that_and_never_as_stated() {
        let out = compare_with(
            &args(vec![offer("from-page", 10.0, 1, None), elsewhere("from-listing", 12.0, Some(1.0)), elsewhere("nobody-knows", 9.0, None)]),
            &looked(by_order_value()),
        )
        .unwrap();

        // The field, on every row.
        let from_page = row(&out, "from-page");
        assert_eq!(from_page.shipping_source, "delivery_page");
        assert_eq!(from_page.delivery_page.as_deref(), Some(DELIVERY_PAGE));
        assert_eq!(row(&out, "from-listing").shipping_source, "offer");
        assert_eq!(row(&out, "nobody-knows").shipping_source, "unknown");
        let handed = serde_json::to_value(&out).unwrap();
        let sources: Vec<&str> = handed["rows"].as_array().unwrap().iter().map(|r| r["shipping_source"].as_str().unwrap()).collect();
        assert_eq!(sources, ["unknown", "offer", "delivery_page"], "as the model reads it, cheapest first");

        // And the note, which says whose word the cost is.
        let note = out.notes.iter().find(|n| n.contains("delivery page")).unwrap_or_else(|| panic!("got: {:?}", out.notes));
        assert!(note.starts_with("delivery added from the shop's own delivery page, not from the listing: from-page +5.49 (shop.example; free from 40.00)"), "got: {note}");
        assert!(note.contains("never that the listing states it"), "got: {note}");
        assert!(!note.contains("nobody-knows") && !note.contains("from-listing"), "got: {note}");
        // What nobody knows is still named as item-only, and only that.
        let item_only = out.notes.iter().find(|n| n.contains("item-only")).unwrap_or_else(|| panic!("got: {:?}", out.notes));
        assert!(item_only.contains("nobody-knows") && !item_only.contains("from-page"), "got: {item_only}");
    }

    #[test]
    fn terms_in_another_currency_are_not_applied() {
        let pounds = Terms::Costs { bands: Bands::new(vec![(0.0, 5.49)]).unwrap(), currency: "GBP".to_string() };
        let out = compare_with(&args(vec![offer("in-euros", 10.0, 1, None)]), &looked(pounds)).unwrap();
        assert_eq!((out.rows[0].shipping, out.rows[0].landed, out.rows[0].shipping_source), (None, 10.0, "unknown"));
        assert!(out.notes.iter().any(|n| n.contains("no offer states shipping")), "got: {:?}", out.notes);
    }

    #[test]
    fn a_shop_that_will_not_deliver_or_charges_by_weight_is_said_and_costs_nothing_added() {
        let offers = || vec![offer("unstated", 10.0, 1, None), elsewhere("stated", 12.0, Some(1.0))];
        let out = compare_with(&args(offers()), &looked(Terms::NotDelivered)).unwrap();
        assert_eq!((row(&out, "unstated").shipping, row(&out, "unstated").shipping_source), (None, "unknown"));
        assert!(
            out.notes.contains(
                &"shop.example: its delivery page says it does not deliver to the delivery country (Netherlands) — say so \
                  beside that option"
                    .to_string()
            ),
            "got: {:?}",
            out.notes
        );

        let out = compare_with(&args(offers()), &looked(Terms::Depends)).unwrap();
        assert_eq!(row(&out, "unstated").shipping, None);
        assert!(
            out.notes.contains(&format!(
                "shop.example: delivery is charged by weight or destination, so none was added — its terms: {DELIVERY_PAGE}"
            )),
            "got: {:?}",
            out.notes
        );

        // A page that said nothing, was not found or could not be read
        // leaves the offer as it was, with no word about why.
        for nothing in [Terms::Silent, Terms::NoPage, Terms::Unread] {
            let out = compare_with(&args(offers()), &looked(nothing)).unwrap();
            assert_eq!(row(&out, "unstated").shipping_source, "unknown");
            assert!(!out.notes.iter().any(|n| n.contains("delivery page") || n.contains("its terms")), "got: {:?}", out.notes);
            assert!(out.notes.iter().any(|n| n.contains("item-only") && n.contains("unstated")), "got: {:?}", out.notes);
        }

        // A shop of an offer that states its shipping gets no note: its
        // terms were never asked for.
        let out = compare_with(&args(vec![offer("stated", 10.0, 1, Some(2.5))]), &looked(Terms::NotDelivered)).unwrap();
        assert!(!out.notes.iter().any(|n| n.contains("does not deliver")), "got: {:?}", out.notes);
    }

    #[test]
    fn shops_past_the_allowance_are_named_as_not_looked_up() {
        let asked = Looked {
            by_host: HashMap::new(),
            not_looked_up: vec!["fifth.example".to_string(), "sixth.example".to_string()],
            country: delivery::country("NL"),
        };
        let out = compare_with(&args(vec![offer("a", 10.0, 1, None)]), &asked).unwrap();
        assert!(
            out.notes.contains(
                &"delivery was not looked up at fifth.example, sixth.example: 4 new shops a request — those prices stay \
                  item-only"
                    .to_string()
            ),
            "got: {:?}",
            out.notes
        );
    }

    #[test]
    fn with_nothing_looked_up_the_comparison_is_the_one_it_always_was() {
        // No delivery country, or no desk at all: the same ranking and the
        // same notes, and each row says where its shipping came from.
        let offers = || vec![offer("unstated", 5.0, 1, None), elsewhere("stated", 9.0, Some(1.0))];
        let plain = compare(&args(offers())).unwrap();
        let unasked = compare_with(&args(offers()), &Looked::default()).unwrap();
        assert_eq!(serde_json::to_value(&plain).unwrap(), serde_json::to_value(&unasked).unwrap());
        assert_eq!(plain.best_single.title, "unstated");
        assert_eq!(plain.notes.len(), 2, "item-only, and no bulk option: {:?}", plain.notes);
        assert_eq!((row(&plain, "unstated").shipping_source, row(&plain, "stated").shipping_source), ("unknown", "offer"));
        assert!(plain.rows.iter().all(|r| r.delivery_page.is_none() && r.free_from.is_none()));
    }

    #[test]
    fn the_shops_asked_about_are_those_of_unstated_offers_cheapest_per_unit_first() {
        // The allowance may run out, so the offers that decide the pick go
        // first: by item price per unit, which is all that is known yet.
        let at = |host: &str, price: f64, units: u32, shipping: Option<f64>| Offer {
            url: format!("https://{host}/p"),
            ..offer(host, price, units, shipping)
        };
        let call = args(vec![
            at("dear.example", 30.0, 1, None),
            at("stated.example", 1.0, 1, Some(2.0)),
            at("pack.example", 40.0, 4, None),
            Offer { url: "not an address".to_string(), ..offer("unreadable", 1.0, 1, None) },
            at("cheap.example", 20.0, 1, None),
        ]);
        let asked: Vec<String> = unstated_cheapest_first(&call).iter().map(|url| url.host_str().unwrap().to_string()).collect();
        assert_eq!(asked, ["pack.example", "cheap.example", "dear.example"]);
    }

    #[tokio::test]
    async fn a_malformed_call_is_refused_before_anything_is_read() {
        let w = World::with_a_shop_that_reads_well().await;
        let tool = ComparePricesTool { delivery: Some(Arc::new(w.desk())) };
        let at_the_shop = |title: &str| Offer { url: w.url(PRODUCT).to_string(), ..offer(title, 12.95, 1, None) };

        let mixed = tool
            .call(args(vec![at_the_shop("euros"), Offer { currency: "USD".to_string(), ..at_the_shop("dollars") }]))
            .await
            .unwrap_err();
        assert!(mixed.to_string().contains("same currency"), "got: {mixed}");
        let blank = tool.call(args_in("  ", vec![at_the_shop("unit")])).await.unwrap_err();
        assert!(blank.to_string().contains("unit_name"), "got: {blank}");

        assert_eq!(w.asked().await, (vec![], 0), "neither the shop nor the model was asked");
        assert!(w.rows().is_empty());
    }

    #[tokio::test]
    async fn compare_prices_looks_up_delivery_for_the_offers_that_state_none() {
        let w = World::with_a_shop_that_reads_well().await;
        let tool = ComparePricesTool { delivery: Some(Arc::new(w.desk())) };
        let product = w.url(PRODUCT).to_string();
        let call: CompareArgs = serde_json::from_value(serde_json::json!({
            "unit_name": "widget",
            "offers": [
                {"title": "at the shop", "url": product, "price": 12.95, "currency": "EUR"},
                {"title": "stated", "url": "https://other.example/widget", "price": 13.50, "currency": "EUR", "shipping": 3.95},
            ]
        }))
        .unwrap();

        let out = tool.call(call).await.unwrap();

        let page = w.url(DELIVERY).to_string();
        let priced = row(&out, "at the shop");
        assert_eq!((priced.shipping, priced.landed, priced.shipping_source), (Some(5.49), 18.44, "delivery_page"));
        assert_eq!((priced.delivery_page.as_deref(), priced.free_from), (Some(page.as_str()), Some(40.0)));
        assert_eq!(out.best_single.title, "stated", "17.45 delivered against 18.44");
        // The shop of the offer that stated its shipping was not asked.
        assert_eq!(w.asked().await, (vec![PRODUCT.to_string(), DELIVERY.to_string()], 1));
        // What goes back to the main model is numbers and the page's
        // address: nothing the shop's page said.
        let handed = serde_json::to_string(&out).unwrap();
        for said in ["Wij bezorgen", "werkdagen", "afhaalpunt", "bezorging gratis", "Widget 500"] {
            assert!(!handed.contains(said), "{said:?} reached the model: {handed}");
        }
        let counts = w.metrics.render();
        for line in [
            "scout_compared_offers_total{shipping=\"delivery_page\"} 1",
            "scout_compared_offers_total{shipping=\"offer\"} 1",
            "scout_compared_offers_total{shipping=\"unknown\"} 0",
        ] {
            assert!(counts.contains(&format!("{line}\n")), "{line} is missing:\n{counts}");
        }
    }

    #[tokio::test]
    async fn without_a_delivery_country_nothing_is_read_and_nothing_changes() {
        let w = World::with_a_shop_that_reads_well().await;
        let nowhere = DeliveryDesk::new(&w.core.deps, w.observer.clone(), Arc::new(delivery::ShopPages::default()), None, Vec::new());
        let tool = ComparePricesTool { delivery: Some(Arc::new(nowhere)) };
        let offers = || vec![Offer { url: w.url(PRODUCT).to_string(), ..offer("at the shop", 12.95, 1, None) }, elsewhere("stated", 13.5, Some(3.95))];

        let out = tool.call(args(offers())).await.unwrap();

        assert_eq!(serde_json::to_value(&out).unwrap(), serde_json::to_value(compare(&args(offers())).unwrap()).unwrap());
        assert!(!out.notes.iter().any(|n| n.contains("country") || n.contains("delivery page")), "nothing is said of it: {:?}", out.notes);
        assert_eq!(w.asked().await, (vec![], 0));
        // The offers are counted all the same.
        assert!(w.metrics.render().contains("scout_compared_offers_total{shipping=\"unknown\"} 1\n"), "{}", w.metrics.render());
    }
}
