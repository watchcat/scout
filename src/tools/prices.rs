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

/// Rank offers by landed cost per unit. Pure: no I/O, no model, no clock.
pub fn compare(args: &CompareArgs) -> Result<Comparison, PriceCompareError> {
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
}
