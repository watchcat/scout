# Pages Scout could not price: what the traces say

Gate 1 of `docs/superpowers/specs/2026-11-17-price-watch-and-shop-browsing-design.md`
asked what the run traces say before anything is built for browsing a
shop: how often a product page comes back with no price, and how often an
offer reaches `compare_prices` with no shipping cost. This is the reading
of 2026-11-18, from `scripts/unread-pages.py` run over the nightly backup:
sixty days, 2026-09-19 to 2026-11-17, 71 runs that opened a page or
compared prices. Small numbers; read them as shape, not precision.

> **Corrected the same day** by `docs/2026-11-18-shop-reach.md`: 23 of the
> 167 pages counted below as "read with no price" were refusals that
> `fetch_page` did not recognise and handed over as the page, 20 of them
> on bol.com. A wall stopped 54 of the 282 pages asked for, not 31. Both
> rows of the rule stay met.
>
> The script has since been taught to count such a page apart, as "a block
> notice", by the rule `fetch_page` now refuses it with. It has not been
> run on a backup since, so the tables below stand as they were. Its rule
> is narrower than the query that found the 23: bol's twenty are bol's
> wording and should be counted by it; the three on other hosts have not
> been read against it.

## How a read ends

`fetch_page` hands back the page's text, its links, and — when the page's
own data names the product at that address — a `product` with a price
Rust read from schema.org markup, microdata or the shop's embedded state.
A read is *priced* when that price is there and has *no price* when it is
not, in which case the model has the page's text and nothing else.
*Blocked* is a wall the headless render did not clear either: 401, 403,
429, 503, or a bot check served with a 200. A call *refused rather than
read* never reached the shop: the run had spent its five pages.

Most pages a run opens are not product pages, and a review or a forum
thread with no price is no failure. Two things mark a page as a product
page here, neither of them a guess about its address: the run went on to
hand that address to `compare_prices` as an offer, or the page's own
markup named a product.

## What was measured

- **A quarter of the pages asked for come back with a price.** Of 339
  calls, 57 were refused rather than read, and in 28 of the 71 runs (39%)
  the page budget refused at least one. Of the 282 pages asked for, 70
  (25%) were read with a price, 167 (59%) with none, 31 (11%) were
  blocked, 7 were gone and 7 failed otherwise.
- **Between three and four product pages in ten had no price.** Of the 63
  pages a run read and went on to offer, 28 (44%) had come back with no
  price. Another 37 were product pages by their markup, and 35 of those
  were priced — markup that names a product nearly always carries its
  price, so this mark leans that way. Together it is 30 of 100.
- **Mostly the amount was on the page.** For those 28 the model offered an
  amount anyway, and the page's text wrote that amount in 21 of them, by
  the extractor's own rule (`text_writes_amount`). That shows where the
  model found its number, not that the number is the product's price —
  which is why the extractor does not read prices out of text. amazon.nl
  is 9 of the 21: 15 pages read there, none with a price. In the other 7
  the text Scout kept did not write the amount — 2 of them cut at the
  tool's 6,000 characters — and bol.com is 3 of the 7: 20 pages read
  there, none with a price.
- **Six offers in ten state no shipping cost.** 116 of 197, and in 10 of
  the 35 comparisons no offer stated it at all. The two marketplaces
  whose listings come through an API sit at the ends: eBay's 34 offers all
  carried shipping, Marktplaats's 15 never did (there it is arranged
  between two people). Leaving both out, 101 of the other 148 offers (68%)
  had none.
- **What a read adds is "free".** Of the 63 offers whose page had been
  read, 21 said delivery was free; of the 60 from shops whose page was
  never asked for, 7 did. A cost above zero was no more common after a
  read: 5 of 63 against 7 of 60. This is read off the offer the model
  handed over, not off the page.
- **More than half the offered addresses were never asked for.** 109 of
  187: 49 of them listings from eBay and Marktplaats, 60 of them shops'.
  11 more were asked for and refused by the page budget, and 4 failed.
  The script does not follow where the figures for an unread address came
  from; a search result and a page that lists several products are both
  possible.
- **A wall stopped one page in nine.** 31 reads on 16 hosts, one to eight
  reads each; one forum is 8 of them and was blocked every time. No page
  that was blocked was offered.

## What the traces cannot say

- **Which reads needed the headless render.** The card asked for it and
  the traces do not hold it. `fetch_page` tries the render when the plain
  request is refused or returns an empty shell, and hands back the same
  shape either way; a call's duration does not tell the two apart, being
  measured across the whole batch of calls it ran in. The next card reads
  each host both ways, which is the measurement.
- **Product pages the model gave up on.** A page with no markup and no
  price that was never offered looks like any other page, so both shares
  above leave it out.
- **Where an amount came from when the text did not write it** — a search
  result, another page, or the part of the page past the cut.
- **Much about any one host.** 183 of the 189 hosts were used by a single
  account, most of them in a single run. A host's row is often one
  conversation.

Traces are kept for the last 300 runs. The database held 197, so the
window is whole.

## The rule

The spec named it before measuring. Browsing is *needed* if at least one
product page in ten was read with no price, or at least one offer in
three reached `compare_prices` with no shipping cost.

| | measured | the rule asks | |
|---|---|---|---|
| product pages read with no price | 30 of 100 | 1 in 10 | met |
| offers with no shipping cost | 116 of 197 | 1 in 3 | met |

Both hold, so the *needed* half is met. The first row is the lower of the
two counts above; by the pages a run offered it is 28 of 63. What stands
behind each half is not the same, and the next card should know it:

- The price half is mostly not a matter of reach. In 21 of 28 cases the
  amount was in the text Scout already had, and a browser adds nothing to
  a page that was read. Whether the page's own data carries the price,
  which is all the extractor reads, is the thing to look at there.
- The shipping half is the one a read does not answer. After a read an
  offer more often says delivery is free; a cost above zero it states no
  more often than without one.

## The hosts

Ranked by what fell short on each: offers with no shipping cost, plus
offered pages read with no price, plus blocked reads. Ties go to the host
with more pages asked for.

A host name is something somebody looked at, and here it is nearly always
one person: so this file, which anyone can read, names a host only when
at least two accounts used it. That is the script's default. Four of the
ten are named:

| host | runs | accounts | pages: priced | no price | blocked | offers | no shipping | offered pages with no price (amount in text) | short |
|---|---|---|---|---|---|---|---|---|---|
| amazon.nl | 17 | 3 | 0 | 15 | 0 | 13 | 4 | 9 (9) | 13 |
| bol.com | 18 | 3 | 0 | 20 | 0 | 7 | 6 | 3 (0) | 9 |
| coolblue.nl | 6 | 2 | 4 | 4 | 0 | 6 | 6 | 0 | 6 |
| zooplus.nl | 4 | 2 | 5 | 0 | 0 | 6 | 2 | 0 | 2 |

The other six were each used by a single account, and the operator has
their names: `scripts/unread-pages.py --min-accounts 1` prints them from this
backup. Between them: 37 pages asked for — 1 read with a price, 32 with
none, 4 blocked — and 26 offers, 24 with no shipping cost.

Six hosts ranked among the ten and were passed over, each for a reason
the next card's questions give: Marktplaats's link host (15), where the
listings are between people and no shop states a shipping cost; a forum
(8), which sells nothing; a second storefront of a shop already in (4);
two storefronts that serve other countries (4 and 3), unlikely to have
an answer about shipping to a Dutch postcode; and one that ships nothing
(3).

zooplus.nl is there for contrast as much as for its rank: `fetch_page`
priced all five pages it read there.

## What follows

- The *needed* half of the rule is met, so **Shop reach** goes ahead on
  these ten. Its first question is what delivery costs where it is not
  free; the price is the smaller part.
- amazon.nl and bol.com are 35 of the 167 pages read with no price, and
  not one page on either came back priced. On amazon.nl the model's amount
  was in the text of all 9 offered pages read there. That is work on the
  extractor, with or without a browser.
- The page budget refused a page in two runs out of five, and 11 offered
  pages went unread for it. Whatever browses a shop spends pages faster
  than a read does, so it needs a budget of its own rather than a share
  of these five.
- `fetch_page` does not record how a page was read. If the split between
  the plain request and the render matters after Shop reach, it is a
  counter on `/metrics`, not something to dig out of the traces.
