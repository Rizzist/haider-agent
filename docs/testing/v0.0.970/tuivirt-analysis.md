# tuivirt analysis (owner-approved 2026-09-02)
Current: per-row pre-render cache, cold fill O(N) (~25 µs/row → 10k rows ≈ 250 ms), memory O(N). Target: frames O(viewport) (~50 rows ≈ 1–2 ms layout +
draw diff), first frame ≤ 33 ms at any N, memory bounded by a viewport window (rendered) + raw transcript (bytes + small metadata). Mechanisms: viewport-only
layout with overscan; incremental/idle layout with estimated heights corrected on measurement (prefix-sum for scroll mapping); bounded LRU-by-distance
render cache; interned styles; byte ranges into an arena/rope; search/export off the frame path. Risks: jump accuracy with estimated heights (measure around
the target before landing), Unicode width/grapheme cost (cache width tables at ingest), extreme single rows (cap + expander). Expected: 2–5 ms frames at any
N; 50k-row session ~6–8 MB client memory instead of 20+. Conformance bench unaffected (headless never enters the TUI).
