# What a browser can reach on a shop: ten hosts, three ways

Gate 2 of `docs/superpowers/specs/2026-11-17-price-watch-and-shop-browsing-design.md`
asked whether a session that a model drives gets anything a read does not.
This is the reading of 2026-11-18: the ten hosts the first gate chose
(`docs/2026-11-18-unread-pages.md`), one product page on each, read three
ways from the production node's address, which is the one shops see. Four
hosts are named; the other six were each used by a single account and are
lettered E to J. The products are ones picked for this reading, not ones
anybody asked Scout about.

One product a shop, two sessions each, one afternoon. Read it as shape.

## How each way reads

- **Plain request.** The request `fetch_page` makes, by Scout's own client
  and headers, leaving through a tunnel to the node.
- **One-shot render.** The headless render `fetch_page` falls back to, with
  Scout's flags: a laptop's Chrome 157, leaving by the same tunnel.
- **Driven session.** A throwaway prototype of the spec's loop
  (`scripts/shop-reach.mjs`): the same Chrome, driven over the DevTools
  protocol by Scout's model (`minimax-m3`) with six actions — open, read,
  click, type, select, back — and the spec's rules: one shop, nothing
  personal typed, no sign-in, no bot check attempted, twelve turns, 25
  actions, 120 seconds.

The first two are judged by the extractor itself
(`scout_core::measure::page_as_read`), and then the model is handed what
`fetch_page` would have handed it and asked what the session is asked: the
price, the price of another variant, what delivery to a Dutch postcode
costs, and when it comes. The postcode is Amsterdam's city hall, an
address that is nobody's.

An amount counts only where the text the model was shown writes it. That
check is necessary and it is not enough: it passes a number that is on the
page and answers another question. Where it did, the table below follows
the page, and each such cell is listed under it.

## What each way read

Sessions are the first of the two; where the second differed it is said
below. Tokens are prompt and completion together.

| shop | way | price | another variant | delivery to the postcode | when | how it went |
|---|---|---|---|---|---|---|
| amazon.nl | plain request | — | — | — | — | HTTP 503; earlier the same day a 200 that was a "continue shopping" page of 4 KB |
| | one-shot render | 9.99 | — | free | stated | read |
| | driven session | 9.99 | 9.99 | free | stated | reported; 12 turns, 11 actions, 81,158 tokens, 137 s |
| bol.com | plain request | — | — | — | — | HTTP 403 |
| | one-shot render | — | — | — | — | bol's "access blocked" page, which `fetch_page` takes for the page |
| | driven session | — | — | — | — | refused: HTTP 403 |
| coolblue.nl | plain request | 259.00 | — | — | — | read, priced by the page's own data |
| | one-shot render | 259.00 | — | free | — | read, priced by the page's own data |
| | driven session | 259.00 | 281.00 | free | — | reported; 11 turns, 11 actions, 107,581 tokens, 101 s |
| zooplus.nl | plain request | 5.94 | — | — | — | read, priced by the page's own data |
| | one-shot render | 5.94 | — | — | — | read, priced by the page's own data |
| | driven session | 6.99 | 26.49 | 4.99 | stated | reported; 12 turns, 11 actions, 80,896 tokens, 39 s |
| shop E | plain request | 259.95 | — | — | — | read |
| | one-shot render | — | — | — | — | read, but the text kept was the cookie notice |
| | driven session | 259.95 | — | — | — | reported; 12 turns, 11 actions, 80,515 tokens, 61 s |
| shop F | plain request | 12.95 | — | — | — | read |
| | one-shot render | 12.95 | — | — | — | read |
| | driven session | 12.95 | 14.95 | not delivered there | — | reported; 12 turns, 15 actions, 117,161 tokens, 118 s |
| shop G | plain request | — | — | — | — | HTTP 403 |
| | one-shot render | — | — | — | — | nothing usable |
| | driven session | — | — | — | — | refused: HTTP 403 |
| shop H | plain request | — | — | — | — | HTTP 403 |
| | one-shot render | 89.90 | 109.05 | — | stated | read |
| | driven session | 89.90 | 109.05 | — | stated | cut at its seconds; 11 turns, 10 actions, 98,610 tokens, 225 s |
| shop I | plain request | 21.95 | — | — | stated | read, priced by the page's own data |
| | one-shot render | 21.95 | — | — | — | read, priced by the page's own data |
| | driven session | 21.95 | 40.38 | 6.90 | — | reported; 11 turns, 10 actions, 72,753 tokens, 55 s |
| shop J | plain request | — | — | — | — | HTTP 429 |
| | one-shot render | 99.00 | — | — | stated | read |
| | driven session | 99.00 | 99.00 | 17.20 | stated | reported; 12 turns, 11 actions, 68,791 tokens, 57 s |

Where the table follows the page and not the check:

- **Two "variants" that were not.** Handed a fetched page, the model
  reported as another variant an accessory's price (coolblue.nl, 21.99)
  and a crossed-out old price (shop J). Both amounts were on the page.
  They are left out.
- **Three "free" deliveries that were not answers.** Shop F states that it
  charges no delivery and, on the same page, that it delivers inside its
  own country only; the session reported free. Shop H is a comparison
  site whose price includes delivery to its own country. On zooplus.nl a
  second session put 0 where it had found nothing, and a banner about free
  delivery above an amount let the check pass it. None of the three is a
  cost to the postcode.
- **Two "when"s that were not.** "Can be collected in 13 shops" and "in
  stock" are not delivery times.

One cell goes the other way: shop I's session reported a delivery window
in words that are not the page's, so the check did not pass it and the
table leaves it out.

Between the two sessions: on coolblue.nl the first chose a bundle with a
charger (281.00) and the second another colour (259.00); on shop J the
first chose another colour (99.00) and the second another size (249.00);
on zooplus.nl the second did not reach the basket's delivery line. The
rest agreed.

## What was found

- **Two shops refuse the node whatever asks.** bol.com and shop G answered
  403 to the plain request, the render and the session alike. A session
  does not get past a shop that has said no, and is not meant to.
- **Three more refuse the plain request and let a browser in.** amazon.nl
  (503, and earlier a 200 that was not the page), shop H (403) and shop J
  (429). For these the render Scout already has is what reads the page.
- **A session read a number the reads did not on six hosts.** Another
  variant's price on amazon.nl, coolblue.nl, zooplus.nl, shop F and shop
  I; a delivery cost above zero on zooplus.nl (4.99), shop I (6.90) and
  shop J (17.20).
- **Those three delivery costs are on a page a plain read reaches.** Each
  shop has a page of its own about delivery, one link from the product.
  Read plainly, shop I's gave 6.90 for the Netherlands and shop J's gave
  17.20; zooplus.nl's gave its table by order value, 4.99 among the rows.
  Shop J's sessions took the figure from that page both times. Shop I's
  and zooplus.nl's took it from the basket.
- **The postcode was hardly ever asked for.** Shops ask for a country in
  the basket and for an address at checkout. amazon.nl offers a visitor a
  country list and a sign-in; the postcode was typed once in twenty
  sessions, on a checkout page the session should not have opened.
- **On zooplus.nl the page's own data does not state the price it
  offers.** The page offers 6.99 for a single delivery and 5.94 with a
  discount option beside it. The data `fetch_page` reads says 5.94, and
  so does Scout.
- **A session spends its whole budget.** Of the 16 sessions that were not
  refused, 13 used all twelve turns or were cut; none stopped because it
  had what it came for. The median session took 83,000 tokens (68,000 to
  117,000) and 78 seconds (39 to 225); 4 of the 16 ran past the 120
  seconds. The twenty together took 1.45 million tokens.

## What the sessions did that the loop must not leave to a model

- **Persisting at a wall.** Told to report a refusal and stop, the model
  did so at once or after one more look in three sessions of four. In
  the fourth it went on for eleven actions on a shop that had blocked it,
  nine of them refused. The script now ends a session itself when the
  product page is refused, or two pages running.
- **Going on to checkout.** Told to go no further than the basket, one
  session opened the checkout page and typed the postcode there. Nothing
  personal was typed and nothing ordered, but the rule has to be an
  address the loop refuses, not a sentence.
- **Stating what no page said.** One session reported a free-delivery
  banner that none of its pages showed; the check caught it. Over all
  twenty, the only things typed were a quantity, the postcode, search
  terms and list choices.

## A correction to the first reading

The first gate counted 167 pages "read with no price". 23 of them were
refusals that `fetch_page` did not recognise and handed to the model as
the page: 20 on bol.com, every page read there, and 3 on other hosts.
bol's block page is 9 KB of ordinary HTML with none of the titles a
challenge has, so it is neither a shell nor a challenge by the tests in `tools/browser.rs`; amazon.nl's "continue
shopping" page is the same case, seen today though not in those sixty
days. So a wall stopped 54 of the 282 pages asked for, not 31, and the
model was not told so for 23 of them.

## Links that pick a variant or fill a basket

| shop | a link that picks the variant | a link that fills the basket |
|---|---|---|
| amazon.nl | each variant has its own product address | none without signing in: the add link leads to the sign-in page |
| bol.com | not read: refused | not read: refused |
| coolblue.nl | each variant has its own product address | none found |
| zooplus.nl | in the address, for a browser; a read of it returns the same data as before | none found |
| shop E | not needed: one price for every size | the site has no basket |
| shop F | each variant has its own product address | none found |
| shop G | not read: refused | not read: refused |
| shop H | each variant has its own page | not a shop: it links out to shops |
| shop I | none: a pack size is chosen on the page | none found |
| shop J | in the address | yes: a basket address that carries the variant, tested |

No shop's own markup offers a link that fills a basket; baskets are filled
by a form or by script. Shop J runs on Shopify, whose basket address is a
platform convention, and it is the one that worked: the link landed on a
checkout holding the product at its price.

## The rule

Named before measuring: sessions and the desk are built only if, on at
least three of the ten hosts, a driven session gets a number the other
two ways do not, and the shop does not refuse it.

| | measured | the rule asks | |
|---|---|---|---|
| hosts where a session read a number the reads did not | 6 of 10 | 3 | met |
| of those, a delivery cost above zero | 3 of 10 | | |
| hosts that refused the session | 2 of 10 | | |

The *possible* half is met, and with the first gate's *needed* half the
rule as written says build. What the reading adds is what a session's
numbers cost and where else they are:

- All three delivery costs sit on a page a plain read reaches, for 3,000
  to 6,000 tokens where a session takes 83,000. What is left to a session
  alone is the price of a variant that has no address of its own, on two
  shops here; no delivery cost in this reading was stated by a basket
  only.
- A session costs 83,000 tokens and over a minute, spends its budget
  whether or not it has an answer, and its report needs more checking than
  an amount on a page.
- The two shops Scout cannot read today stay unread. One of them is
  bol.com, where the first gate counted more pages asked for than on any
  of the other nine.

## What follows

- **Browser sessions** and **the shop desk** are no longer gated. The
  order worth considering is below them, not above: the three cheaper
  things first, and the desk for what they leave.
- **Refusal pages read as pages.** `fetch_page` should know bol's block
  page and amazon's "continue shopping" page for what they are, and say
  so as it does for a challenge. This is a defect today, with or without
  a browser.
- **The shop's delivery page.** One more plain read per shop gave the
  delivery cost on three shops of three. It is a fact about a shop, not a
  product, so it keeps.
- **The price the page offers.** Where the page's data and its first offer
  disagree, as on zooplus.nl, Scout quotes the lower one. Price watch
  leans on exactly this read, so it is worth settling before that card.
- **Cart links** has one shop of ten, by a platform's convention. A
  function for Shopify's basket address covers every shop on it; nothing
  else in the table earns one.
- If the desk is built: the loop ends a session at a refusal and refuses
  checkout addresses itself; the budget is nearer 180 seconds than 120;
  and a reported delivery cost is checked against where the shop delivers
  and from what order value, not only for being written.

## What this reading cannot say

- One product a shop and two sessions each. A different product, or a
  third session, could move a cell.
- Every browser here was a laptop's Chrome 157 saying what Scout's render
  says of itself. The image's Chromium was not run. The two refusals did
  not turn on it: Scout's own client was refused first.
- A shop's answer to a datacentre's address on one afternoon. amazon.nl
  gave the plain request two different answers within hours, neither of
  them the page.
- Which cells are right is, in the eight listed above, a reading of the
  page and not a check a program made.
