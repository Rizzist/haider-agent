# G5 table rendering — mutation notes (EXECUTED kills)

Discipline per kill: working tree committed BEFORE the mutation; one
production mutation; ONE anchor test run by name (`cargo test -p
haider-tui --test g5_table_tests <name>` — "running 1 test" observed
every time); the runtime failure recorded verbatim; mutation reverted
(`git checkout -- crates/haider-tui/src/md.rs`); anchor re-run green.
All four target sites the brief names: the breakpoint comparison, the
column floor, escaped-pipe handling, and the stacked bold labels.

## Kill 1 — the breakpoint comparison (LB1 anchor)

- Committed at: 51be193 (impl 1cd1329 + laws 51be193).
- Mutation: `layout_table` floor-fit test widened —
  `floor.iter().sum::<usize>() + chrome <= budget` →
  `<= budget + 8`.
- Anchor: `lb1_one_source_three_budgets_natural_wrapped_stacked`
  ("running 1 test" observed).
- Recorded RUNTIME failure:
  `assertion failed: stacked.iter().all(|row| !row.contains('│'))` —
  budget 18 (floor total 23) wrongly picked the wrapped grid and border
  glyphs reached the stacked assertion.
- Reverted; anchor green (`1 passed`).

## Kill 2 — the column floor M_c (LG2 anchor) — first attempt SURVIVED

- Committed at: 51be193.
- Mutation: measurement loop —
  `floor[c] = floor[c].max(longest_word(cell));` →
  `floor[c] = floor[c].max(3);` (longest-word floor dropped).
- First run against the original single-competitor fixture
  (`| K | Note |` + one prose column): **test PASSED — mutation
  survived.** Root cause: with no competing column, the word column won
  ≥ its word width from headroom-proportional distribution alone; the
  floor never bound. The law was strengthened with a competing-column
  fixture (a 21-cell unbreakable word vs a 44-cell-natural prose
  column at budget 40, where all headroom belongs to the prose column
  and only the floor protects the word), committed as dd9b1b6, then
  the SAME mutation was re-executed.
- Anchor: `lg2_column_floor_holds_the_longest_word_and_the_minimum`
  ("running 1 test" observed).
- Recorded RUNTIME failure:
  `the word column holds its longest-word floor of 21:
  "┌──────────────┬───────────────────────┐"` — the word column
  collapsed from 21 to 12 cells and `unbreakable_word_here`
  hard-split.
- Reverted; anchor green (`1 passed`).

## Kill 3 — escaped-pipe handling (LP1 anchor)

- Committed at: dd9b1b6.
- Mutation: `split_cells` — the `'\\' if chars.peek() == Some(&'|')`
  arm deleted entirely (backslash-pipe splits cells like a bare pipe).
- Anchor: `lp1_escaped_pipe_stays_literal_inside_its_cell`
  ("running 1 test" observed).
- Recorded RUNTIME failure:
  `assertion 'left == right' failed: the escaped pipe never splits a
  cell — left: 0, right: 3` — the header split into 3 cells, the
  2-cell delimiter no longer matched, and the table dissolved into
  paragraphs (0 tagged rows).
- Reverted; anchor green (`1 passed`).

## Kill 4 — stacked bold labels (LS1 anchor)

- Committed at: dd9b1b6.
- Mutation: `stacked` — the label spans re-kinded to `MdKind::Text` and
  the colon pushed as `Text` instead of `Bold` (labels lose their
  bold).
- Anchor: `ls1_stacked_records_bold_labels_rules_no_header_block`
  ("running 1 test" observed).
- Recorded RUNTIME failure:
  `assertion 'left == right' failed — left: "Feature: Plate ",
  right: "Feature:"` — with every span Text-kinded, the wrap's
  compression merged label and value into one span; the first-span
  `"Feature:"`/Bold identity assertions fail (the render-integration
  law `transcript_flips_grid_to_stacked_with_terminal_width` guards
  the same seam at the frame level via the BOLD modifier).
- Reverted; anchor green (`1 passed`).

## Post-kill state

Full `cargo test -p haider-tui` after the final revert: 851 passed,
0 failed. `git status` clean at every commit boundary.

## Review of record (coordinator, executed post-lane)

Read the branch diff (md.rs parse + pure layout_table, render.rs draw
grouping, style slot, 17 laws). The lane's own catch of its vacuous
first floor fixture (mutation 2) is the doctrine working as intended —
verified in the notes. One structurally-unexercised surface found and
closed:

| # | Mutation (seam) | Verdict | Resolution |
|---|---|---|---|
| RM1 | spans_width measured by chars().count() instead of unicode display width | No fixture exercised double-width glyphs — the whole law set was CJK-blind (would have SURVIVED) | New pin `double_width_glyphs_keep_the_grid_aligned` (equal display width across all table lines + borders survive on the CJK row). Kill verified: widths [17,17,17,20,20,17,17] under the mutation, "running 1 test" observed, reverted, 18/18 green |

Deviations (MdLine.table over a data-carrying Copy MdKind;
floors-first+headroom distribution) verified in-diff and correctly
reasoned. Campaign ACCEPTED. Ledger 2027 -> 2028 with the pin.
