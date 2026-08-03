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

## Current frames

| # | Name | Shows |
|---|---|---|
| 1 | `1-Cheapest-price search.png` | A full cheapest-price answer: both headline picks, per-kg maths, runners-up, item-only warning |
| 2 | `2-Alternatives request.png` | Asking for alternatives and getting a fresh comparison |
| 3 | `3-Follow-up request.png` | Changing the requirement mid-conversation ("white, not colour") and having it re-run |
| 4 | `4-Live progress.png` | The streaming commentary — composited from the three captures in `raw/` |

`raw/` holds the original progress captures. They are single lines, 11:1 wide,
and padding them into a square canvas leaves them 90% empty — so they are
stacked into one frame instead. Rebuild that composite if you replace them.

### Worth adding

Nothing yet covers photo search (send a picture, edit the drafted query), the
👍 purchase-memory flow, second-hand results, or `/stat`. Drop them in as
`5-…`, `6-…` and rerun.

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
