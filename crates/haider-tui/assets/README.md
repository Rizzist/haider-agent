# Wordmark assets — حيدر

`wordmark-haider.png` is the حيدر (Haider) wordmark shown in the boot banner and
session header on terminals that speak a real graphics protocol (Kitty, iTerm2,
Sixel). It is loaded by `crate::wordmark` and drawn through `ratatui-image`.
Terminals without a graphics protocol fall back to the hand-crafted half-block
pixel art in `crate::mark` — the two share one cell footprint, so only the
fidelity changes, never the layout.

## Regenerating

`gen-wordmark.swift` rasterizes the word with **CoreText**, which shapes Arabic
natively (contextual joining, ligatures, RTL) and then tight-crops to the ink:

```
swift gen-wordmark.swift <font-family> <pt> <hexRRGGBB> <out.png> [text]
# canonical (Desert-Dawn gold):
swift gen-wordmark.swift "Damascus" 256 D9A441 wordmark-haider.png
```

`wordmark-haider.png` is the bundled asset (`Damascus`, `#D9A441`). Alternate
calligraphies are kept for comparison — swap by copying one over the canonical:

- `wordmark-haider-damascus.png` — bold Naskh, best landscape proportions (current)
- `wordmark-haider-decotype.png` — DecoType Naskh, more formal/elegant
- `wordmark-haider-baghdad.png`  — slender, near-square

The gold `#D9A441` matches the header's `theme.gold`; the background is
transparent so it composites over any terminal ground. To retheme, re-run with a
different hex. Amiri (the sim's font) is not a macOS system face; install it and
pass `"Amiri"` to match the sim exactly.
