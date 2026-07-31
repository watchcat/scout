# Cheapest-Price Requests Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Answer "find the cheapest X" with two grounded picks — cheapest one-off and best price per unit — both on a landed-cost basis, with the arithmetic done in Rust instead of the model.

**Architecture:** A new `compare_prices` tool takes the offers the model extracted (price, currency, pack size, optional shipping) and returns a deterministic ranking: landed cost, per-unit price, headline picks, notes. Offers with unknown shipping are ranked on item price alone and barred from headline picks. The eBay client starts passing through the shipping figure the Browse API already returns, and a preamble rule tells the agent when and how to call the tool.

**Tech Stack:** Rust, rig 0.40 (`Tool` trait), serde/serde_json, thiserror, wiremock (eBay test), tokio test harness.

Spec: `docs/superpowers/specs/2026-07-31-cheapest-price-design.md`

---

## File Structure

- **Create `src/tools/prices.rs`** — the whole feature's logic: `Offer`/`CompareArgs` input types, `Row`/`Comparison` output types, the pure `compare()` function (all validation, arithmetic, ranking), and the `ComparePricesTool` rig wrapper. Pure function first, tool trait as a thin shell over it — the same split `stats.rs` uses, so the rules are testable without a model or network.
- **Modify `src/tools/mod.rs`** — declare the module.
- **Modify `src/tools/ebay.rs`** — `EbayItem.shipping`, parsed from `shippingOptions[0].shippingCost.value`.
- **Modify `src/tools/secondhand.rs`** — render shipping into the eBay result snippet so it survives into the model's `compare_prices` call.
- **Modify `src/agent.rs`** — register the tool, add the cheapest-request protocol to `PREAMBLE`.

---

### Task 1: Comparison core — types and the happy path

**Files:**
- Create: `src/tools/prices.rs`
- Modify: `src/tools/mod.rs`
- Test: `src/tools/prices.rs` (`#[cfg(test)] mod tests`, as everywhere else in this codebase)

- [ ] **Step 1: Declare the module**

In `src/tools/mod.rs`, keep the list alphabetical:

```rust
pub mod ebay;
pub mod fetch;
pub mod kagi;
pub mod marktplaats;
pub mod memory;
pub mod prices;
pub mod purchases;
pub mod reminders;
pub mod secondhand;
```

- [ ] **Step 2: Write the failing test**

Create `src/tools/prices.rs` containing only this test module (the types it references come in Step 4):

```rust
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
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --lib prices`
Expected: FAIL — `cannot find type Offer in this scope` (and friends).

- [ ] **Step 4: Write the implementation**

Put this above the test module in `src/tools/prices.rs`:

```rust
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
```

Add the error type (used by `compare`'s signature, fleshed out with validation in Task 2):

```rust
/// Returned to the model as the tool's error text, so it says how to fix the
/// call rather than just what went wrong.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct PriceCompareError(pub String);
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --lib prices`
Expected: PASS — `test tools::prices::tests::ranks_by_landed_price_per_unit ... ok`

Note: `compare` indexes `args.offers[0]` and would panic on an empty list; Task 2 adds the guard that makes it unreachable. If `cargo test` is run between the two tasks it still passes — no test exercises the empty case yet.

- [ ] **Step 6: Commit**

```bash
git add src/tools/prices.rs src/tools/mod.rs
git commit -m "feat: landed-cost per-unit price comparison core"
```

---

### Task 2: Validation and the remaining ranking rules

**Files:**
- Modify: `src/tools/prices.rs`
- Test: `src/tools/prices.rs` (same test module)

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib prices`
Expected: FAIL — `rejects_unusable_input` panics on `compare(&args(vec![]))` (index out of bounds) or on `unwrap_err` where `unwrap` succeeded.

- [ ] **Step 3: Write the validation**

In `src/tools/prices.rs`, insert this function above `compare`:

```rust
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
```

Call it as the first line of `compare`:

```rust
pub fn compare(args: &CompareArgs) -> Result<Comparison, PriceCompareError> {
    validate(args)?;
    let currency = args.offers[0].currency.trim().to_uppercase();
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib prices`
Expected: PASS — 5 tests in `tools::prices::tests`.

- [ ] **Step 5: Commit**

```bash
git add src/tools/prices.rs
git commit -m "feat: validate compare_prices input and pin the ranking rules"
```

---

### Task 3: Expose it as the `compare_prices` tool

**Files:**
- Modify: `src/tools/prices.rs`
- Modify: `src/agent.rs:143-170` (`build_agent`)
- Test: `src/tools/prices.rs` (same test module)

- [ ] **Step 1: Write the failing test**

Append inside `mod tests`:

```rust
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib prices`
Expected: FAIL — `cannot find value ComparePricesTool in this scope`.

- [ ] **Step 3: Write the tool wrapper**

Add these two imports to the existing `use` block at the top of the file, then append the rest above the test module:

```rust
use rig::tool::Tool;
use serde_json::json;
```

```rust
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
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib prices`
Expected: PASS — 6 tests in `tools::prices::tests`.

- [ ] **Step 5: Register the tool on the agent**

In `src/agent.rs`, add the import next to the other tool imports:

```rust
use crate::tools::prices::ComparePricesTool;
```

and add the tool in `build_agent`, right after the `SecondhandSearchTool` block so search and comparison sit together:

```rust
        .tool(ComparePricesTool)
```

- [ ] **Step 6: Verify the agent still builds**

Run: `cargo test --lib`
Expected: PASS, no warnings about an unused import.

- [ ] **Step 7: Commit**

```bash
git add src/tools/prices.rs src/agent.rs
git commit -m "feat: compare_prices tool available to the agent"
```

---

### Task 4: Pass eBay's shipping cost through

**Files:**
- Modify: `src/tools/ebay.rs:19-26` (`EbayItem`), `:53-71` (raw types), `:139-157` (mapping)
- Modify: `src/tools/secondhand.rs:147-174` (eBay snippet)
- Test: `src/tools/ebay.rs`, `src/tools/secondhand.rs`

- [ ] **Step 1: Write the failing test**

In `src/tools/ebay.rs`, replace the response body and assertion in `searches_with_cached_token_and_maps_items` so the first item carries shipping and add a second item that has none:

```rust
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "total": 3,
                "itemSummaries": [
                    {"title": "USB hub 3.0", "itemWebUrl": "https://ebay.nl/itm/1",
                     "price": {"value": "12.50", "currency": "EUR"}, "condition": "Used",
                     "shippingOptions": [
                         {"shippingCostType": "FIXED",
                          "shippingCost": {"value": "4.95", "currency": "EUR"}}
                     ]},
                    {"title": "USB hub, pickup only", "itemWebUrl": "https://ebay.nl/itm/2",
                     "price": {"value": "9.00", "currency": "EUR"}},
                    {"title": "no url item"}
                ]
            })))
```

```rust
        let items = c.search("usb hub", 5).await.unwrap();
        assert_eq!(
            items,
            vec![
                EbayItem {
                    title: "USB hub 3.0".into(),
                    url: "https://ebay.nl/itm/1".into(),
                    price: Some("12.50 EUR".into()),
                    shipping: Some("4.95 EUR".into()),
                    condition: Some("Used".into()),
                },
                EbayItem {
                    title: "USB hub, pickup only".into(),
                    url: "https://ebay.nl/itm/2".into(),
                    price: Some("9.00 EUR".into()),
                    // no shippingOptions -> unknown, NOT free
                    shipping: None,
                    condition: None,
                },
            ]
        );
        // second call: token endpoint must NOT be hit again (expect(1) above)
        let again = c.search("usb hub", 5).await.unwrap();
        assert_eq!(again.len(), 2);
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib ebay`
Expected: FAIL — `struct EbayItem has no field named shipping`.

- [ ] **Step 3: Write the implementation**

In `src/tools/ebay.rs`, add the field to `EbayItem`:

```rust
/// A live listing from the eBay Browse API.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EbayItem {
    pub title: String,
    pub url: String,
    pub price: Option<String>, // "49.99 EUR"
    /// Shipping cost as eBay states it for this marketplace ("4.95 EUR").
    /// `None` means the listing does not state one — not that it is free.
    pub shipping: Option<String>,
    pub condition: Option<String>,
}
```

Add the raw field and its type:

```rust
#[derive(Deserialize)]
struct RawItem {
    #[serde(default)]
    title: Option<String>,
    #[serde(default, rename = "itemWebUrl")]
    item_web_url: Option<String>,
    #[serde(default)]
    price: Option<RawPrice>,
    #[serde(default, rename = "shippingOptions")]
    shipping_options: Vec<RawShippingOption>,
    #[serde(default)]
    condition: Option<String>,
}

#[derive(Deserialize)]
struct RawShippingOption {
    #[serde(default, rename = "shippingCost")]
    shipping_cost: Option<RawPrice>,
}
```

Reuse the money formatting for both fields — replace the mapping closure in `search`:

```rust
        Ok(parsed
            .item_summaries
            .into_iter()
            .filter_map(|raw| {
                let url = raw.item_web_url?;
                Some(EbayItem {
                    title: raw.title.unwrap_or_default(),
                    url,
                    price: raw.price.and_then(money),
                    shipping: raw
                        .shipping_options
                        .into_iter()
                        .next()
                        .and_then(|o| o.shipping_cost)
                        .and_then(money),
                    condition: raw.condition,
                })
            })
            .take(limit)
            .collect())
```

and add the helper next to the raw types:

```rust
/// "12.50" + "EUR" -> "12.50 EUR"; currency alone is not a price.
fn money(p: RawPrice) -> Option<String> {
    match (p.value, p.currency) {
        (Some(v), Some(c)) => Some(format!("{v} {c}")),
        (Some(v), None) => Some(v),
        _ => None,
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib ebay`
Expected: PASS. `cargo test --lib` still fails to compile `secondhand.rs` — the next step fixes that.

- [ ] **Step 5: Write the failing snippet test**

In `src/tools/secondhand.rs`, find `ebay_sites_use_the_browse_api_when_configured` and add `shippingOptions` to the mocked item, then assert the snippet. The mocked eBay search response gains:

```rust
                     "shippingOptions": [
                         {"shippingCost": {"value": "4.95", "currency": "EUR"}}
                     ],
```

and the snippet assertion becomes:

```rust
        assert_eq!(
            out[0].results[0].snippet,
            "12.50 EUR + 4.95 EUR shipping · Used · live eBay listing"
        );
```

- [ ] **Step 6: Run it to verify it fails**

Run: `cargo test --lib secondhand`
Expected: FAIL — snippet is `"12.50 EUR · Used · live eBay listing"`.

- [ ] **Step 7: Render shipping into the snippet**

In `src/tools/secondhand.rs`, replace the eBay `match (i.price, i.condition)` snippet expression with a parts list — the four-arm match does not extend to three optional fields:

```rust
                                .map(|i| SearchResult {
                                    title: i.title,
                                    url: i.url,
                                    snippet: {
                                        let mut parts: Vec<String> = Vec::new();
                                        match (i.price, i.shipping) {
                                            (Some(p), Some(s)) => {
                                                parts.push(format!("{p} + {s} shipping"))
                                            }
                                            (Some(p), None) => parts.push(p),
                                            (None, Some(s)) => parts.push(format!("{s} shipping")),
                                            (None, None) => {}
                                        }
                                        if let Some(c) = i.condition {
                                            parts.push(c);
                                        }
                                        parts.push("live eBay listing".to_string());
                                        parts.join(" · ")
                                    },
                                })
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, all tests.

- [ ] **Step 9: Commit**

```bash
git add src/tools/ebay.rs src/tools/secondhand.rs
git commit -m "feat: surface eBay shipping cost in results"
```

---

### Task 5: Teach the agent when to compare

**Files:**
- Modify: `src/agent.rs` (`PREAMBLE`)
- Test: `src/agent.rs` (existing `profile_is_appended_when_present` covers the constant's shape; no new test)

- [ ] **Step 1: Add the protocol rule**

In `src/agent.rs`, insert this rule into `PREAMBLE` immediately before the `- Always include the price (with currency) and a direct link...` rule, so price presentation rules stay together:

```
- Cheapest/best-price requests ('cheapest X', 'best price', 'how cheap can I \
get X'): while reading results, note each offer's pack size (how many units \
the listing contains) and its shipping cost when the result or page states \
them - never invent either, omit what is not stated. Then call compare_prices \
ONCE with every candidate offer and take all numbers from its output \
verbatim; do not do the arithmetic yourself. Present best_single as \
'Cheapest one-off' and, when bulk_advantage is true, best_per_unit as 'Best \
per unit' with the pack size and the saving; when it is false, say plainly \
that buying more does not save. Add at most 3 runners-up from rows. Follow \
the tool's notes: name the offers whose shipping is unknown, and state the \
pack size you assumed when a listing did not spell it out. All offers in one \
call must share a currency - compare the user's currency and mention offers \
in other currencies separately.
```

- [ ] **Step 2: Verify the preamble still compiles and tests pass**

Run: `cargo test --lib agent`
Expected: PASS — `profile_is_appended_when_present`.

- [ ] **Step 3: Commit**

```bash
git add src/agent.rs
git commit -m "prompt: cheapest requests go through compare_prices"
```

---

### Task 6: Full verification and deploy

**Files:** none modified unless a check fails

- [ ] **Step 1: Run the whole suite**

Run: `cargo test`
Expected: PASS — 101 existing tests plus 6 new `tools::prices` tests, no failures.

- [ ] **Step 2: Lint**

Run: `cargo clippy --all-targets`
Expected: no warnings from the `scout` crate (the `proc-macro-error2` future-incompat note from dependencies is pre-existing and unrelated).

- [ ] **Step 3: Sanity-check the ranking against live data**

Run:

```bash
set -a && . ./.env && set +a
TOK=$(curl -s -u "$EBAY_CLIENT_ID:$EBAY_CLIENT_SECRET" \
  -d 'grant_type=client_credentials&scope=https://api.ebay.com/oauth/api_scope' \
  https://api.ebay.com/identity/v1/oauth2/token \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["access_token"])')
curl -s -H "Authorization: Bearer $TOK" -H "X-EBAY-C-MARKETPLACE-ID: ${EBAY_MARKETPLACE:-EBAY_NL}" \
  "https://api.ebay.com/buy/browse/v1/item_summary/search?q=philips%20oneblade%20blades&limit=4" \
  | python3 -c '
import sys,json
d=json.load(sys.stdin)
for i in d.get("itemSummaries",[])[:4]:
    ship=[o.get("shippingCost") for o in i.get("shippingOptions",[])]
    print(i["title"][:50], i.get("price",{}).get("value"), ship)
'
```

Expected: listings print with a `shippingCost` value — confirming the field the parser now reads is really there for this marketplace.

- [ ] **Step 4: Deploy**

```bash
docker compose up -d --build
docker compose logs --tail 20 scout
```

Expected: `Container scout-scout-1 Started` and a `scout is up` log line.

- [ ] **Step 5: Manual check in Telegram**

Send: `what's the cheapest way to buy philips oneblade blades?`
Expected: a reply with a "Cheapest one-off" pick and either a "Best per unit" pick with a pack size and saving, or an explicit statement that bulk does not save. Prices include shipping where known; unknown-shipping offers are labelled.

Then run `docker compose logs --tail 50 scout` and confirm a `compare_prices` tool call appears rather than the model doing its own arithmetic.

- [ ] **Step 6: Push**

```bash
git push origin main
```

---

## Notes for the implementer

- **No new dependencies.** `thiserror`, `serde`, `serde_json`, `rig`, `wiremock` and `tokio` are all already in `Cargo.toml`.
- **f64 comparison:** use `total_cmp`, not `partial_cmp().unwrap()` — the latter panics on NaN, and `validate` is the only thing keeping NaN out.
- **Money rounding happens once**, inside `compare`. Do not round again when rendering; the model is told to use the numbers verbatim.
- **Unknown shipping is never zero.** If you find yourself writing `shipping.unwrap_or(0.0)` anywhere outside the `landed` computation (where the flag records the difference), stop — that is the bug this feature exists to prevent.
