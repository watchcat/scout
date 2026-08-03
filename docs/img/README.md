# Demo screenshots

`scout-demo.gif` in the project README is built from the screenshots in
`frames/`. To refresh it:

```bash
python3 scripts/build-demo-gif.py
```

## Naming

Frames play in filename order, and the name after the leading number becomes
the caption burned into the top bar:

```
frames/1-price-comparison.png   ->  "Price comparison"
frames/2-photo-search.png       ->  "Photo search"
```

## Shot list

Six frames, each showing something the others don't:

| # | Name | What to capture |
|---|---|---|
| 1 | `1-price-comparison.png` | A cheapest-price answer: both headline picks, per-unit prices, links. The single most convincing screen. |
| 2 | `2-live-progress.png` | Mid-run, while the progress message is still updating — `🔎 searching in 3 languages…` or `🧮 comparing 6 offers per kilo`. Catch it before the answer lands. |
| 3 | `3-photo-search.png` | A product photo you sent, with the drafted search description and the 📋 Copy to edit button underneath. |
| 4 | `4-purchase-memory.png` | Asking *"where did I buy X last time?"* and getting the shop, price and date back. Or the 👍 flow offering to save a purchase. |
| 5 | `5-second-hand.png` | A second-hand search: eBay/Marktplaats results with live prices and conditions. |
| 6 | `6-usage-stats.png` | `/stat 30` — the monospace bar chart. |

## Capturing

- **Dark theme** matches the GIF's padding colour; light theme leaves visible
  bands on frames that aren't the tallest.
- **Crop out** your phone's status bar (carrier, battery) and anything with a
  real name or handle in it — this repo is public.
- Portrait, roughly the same aspect ratio across frames. The script scales
  everything to one width and pads the rest, so exact sizes don't matter.
- PNG or JPG, either is fine.

## Options

```bash
python3 scripts/build-demo-gif.py --seconds 4      # slower cycle
python3 scripts/build-demo-gif.py --width 360      # smaller file
python3 scripts/build-demo-gif.py --no-captions    # no caption bar
```

Keep the result under about 5 MB — GitHub serves larger files, but they stall
on slow connections and the README is the first thing anyone sees.
