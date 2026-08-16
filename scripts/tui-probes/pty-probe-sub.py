#!/usr/bin/env python3
"""TUI3b commit-2 probe: drive the REAL runtime through the subagent tree and
the aura stage. Starts a two-subagent turn, waits for the chips + the amber ?
question, then opens /aura, runs an orchestrate turn, and escapes back.
GATES (review TUI4.1 P2-3): every surface paint the size can show, plus
alt-screen balance, no panic, clean child exit on ⌃C ⌃C. Size-shed checks
(chip glyphs / the amber question / the aura spawn log at short frames, the
sticky band at tall frames where the turn fits) print SKIP explicitly —
bypassed by design, never silently folded into a pass."""
import os, re, signal, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib

def _visible(raw):
    """Escape-stripped text: ratatui's cell-diff renderer can split any
    needle across cursor-positioning escapes (round-15's dump caught
    `testcontainer` + CUP + ` or mocks?`), so raw-stream substring checks
    are unsound. Match on de-escaped text with a splittable-tail-tolerant
    needle."""
    return re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", raw)


def amber_painted(raw):
    return bool(re.search(r"testcontainer|or mocks\?", _visible(raw)))

cols, rows = int(sys.argv[1]), int(sys.argv[2])
binary = sys.argv[3] if len(sys.argv) > 3 else "/usr/local/bin/haider"

pid, fd = probelib.spawn(cols, rows, binary)
sink = [b""]
pump = probelib.make_pump(fd, sink)


def mark():
    return len(sink[0])


def since(pre):
    return sink[0][pre:].decode("utf-8", "replace")


# Poll until the composer is READY before typing: a fixed boot pump let a
# cold CI runner swallow the prompt's leading characters, so the truncated
# text parsed to a DIFFERENT demo script (one generic child, no amber card —
# rounds 9-12). The composer placeholder is the readiness truth.
_boot_deadline = time.time() + 30.0
while time.time() < _boot_deadline:
    pump(0.5)
    if "message haider" in since(0):
        break
os.write(fd, b"use two subagents to split this work\r")
# Poll up to 45s for the amber card: the demo's beat chain (parent
# preamble → child scripts → the question) runs 3-4x slower on cold CI
# runners — the round-11 frame dump showed WAITING still ticking at 18s.
# Fast hosts exit the loop in a few seconds; the check itself is unchanged.
_card_deadline = time.time() + 45.0
while time.time() < _card_deadline:
    pump(1.0)
    if amber_painted(since(0)):
        break
sub_paint = since(0)
# The tests chip is holding its amber ? — the parent turn is idle by now.
pre = mark()
# TUI4c: an idle esc now really DETACHES (the session checks into its
# slot), so /aura is typed from the SESSION — entered that way, esc from
# the aura returns to it (sim: exit goes back to where you came from).
os.write(fd, b"/aura\r")
pump(1.2)
aura_paint = since(pre)
# TUI6 item 6: the aura band's two-rule anatomy, read off a resize-forced
# FULL repaint of the aura stage (the composer is still empty here, so
# the band's row is the placeholder).
pre_band = mark()
probelib.set_size(fd, cols + 1, rows)
os.kill(pid, signal.SIGWINCH)
pump(1.2)
aura_grid = probelib.screen_rows(sink[0][pre_band:])
os.write(fd, b"spin up billing on workstation and run its tests\r")
pump(4.5)
pre2 = mark()
os.write(fd, b"\x1b")  # exit aura
pump(0.8)
final = since(pre2)
# TUI4b item 11: wheel into history — the sticky origin band pins the
# producing prompt on the barBg band (Desert Dawn barBg = rgb(237,225,207);
# only the /help overlay shares that ground and /help is never opened
# here). The band pins at sizes where the transcript actually overflows —
# 90x10 is the pinning run; at tall frames the turn fits and no sticky is
# BY DESIGN.
pre4 = mark()
for _ in range(10):
    os.write(fd, b"\x1b[<64;40;8M")  # SGR mouse wheel-up
    pump(0.08)
pump(0.5)
scrolled = since(pre4)
# Force a full final repaint, refreshing `final` for the back-to-session
# checks.
pre3 = mark()
probelib.set_size(fd, cols + 2, rows)
os.kill(pid, signal.SIGWINCH)
pump(1.5)
final = since(pre3)
final_bytes = sink[0][pre3:]

# ---- TUI6.1 fix 4 (review r1 finding 4): the CHIP VIEW's band, over the
# wire — the exact surface of the owner's TUI6 screenshot. TestBackend
# covers it at generous heights; this drives the real runtime INTO the
# view by clicking subtree rows and asserts both rules on full repaints:
# once for the composer form ("message <callsign> — steer this
# subagent…"), once for the question card (the amber ? chip, still
# unanswered by design). Tall runs only — 90x10 sheds the subtree rows
# the click needs.
chipview_band = "SKIP"
question_band_cv = "SKIP"
if rows >= 30:
    size_flip = [0]

    def full_grid():
        pre = mark()
        size_flip[0] ^= 1
        probelib.set_size(fd, cols + (1 if size_flip[0] else 0), rows)
        os.kill(pid, signal.SIGWINCH)
        pump(1.2)
        return probelib.screen_rows(sink[0][pre:])

    def rule_at(g, r):
        return r in g and g[r].count("\u2500") >= 20

    def band_two(g, top_needle, bottom_needle):
        tops = [r for r, t in sorted(g.items()) if top_needle in t]
        bots = [r for r, t in sorted(g.items()) if bottom_needle in t]
        if not tops or not bots:
            return False
        top, bot = tops[0], max(bots)
        return rule_at(g, top - 1) and any(rule_at(g, bot + d) for d in (1, 2, 3))

    def click(row_number):
        os.write(fd, b"\x1b[<0;6;%dM" % row_number)
        pump(0.15)
        os.write(fd, b"\x1b[<0;6;%dm" % row_number)
        pump(0.9)

    def chip_rows(g):
        return sorted(r for r, t in g.items() if ("\u251c\u2500" in t or "\u2514\u2500" in t))

    g = full_grid()  # session, fresh coordinates
    plain = [r for r in chip_rows(g) if "?" not in g[r]]
    chipview_band = False
    question_band_cv = False
    if plain:
        click(plain[0])
        cv = full_grid()
        if not any("steer this subagent" in t for t in cv.values()):
            # The chip is holding a recovery/question card (the demo's
            # docs chip ERRORS into a ⌁ card) — answer option 1; the chip
            # resumes and the view shows the composer form.
            os.write(fd, b"1")
            pump(2.0)
            cv = full_grid()
        chipview_band = band_two(cv, "steer this subagent", "steer this subagent")
        os.write(fd, b"\x1b")  # esc: back to the session
        pump(0.8)
    g = full_grid()
    holding = [r for r in chip_rows(g) if "?" in g[r]]
    if holding:
        click(holding[0])
        cv = full_grid()
        question_band_cv = band_two(cv, "Run the suite", "mocks")
        os.write(fd, b"\x1b")
        pump(0.8)

try:
    # TUI4b item 10: ctrl-C is NAVIGATION from a session (back to the
    # launcher); only the second ctrl-C, now at the launcher, quits.
    os.write(fd, b"\x03")
    pump(0.4)
    os.write(fd, b"\x03")
except OSError:
    pass
probelib.drain_quiet(fd, sink)
child_clean = probelib.reap(pid)
out = sink[0]
text = out.decode("utf-8", "replace")

cups = re.findall(rb"\x1b\[(\d+);(\d+)H", out)
max_row = max((int(r) for r, c in cups), default=0)
print(
    f"bytes={len(out)} alt_enter={out.count(b'\x1b[?1049h')} "
    f"alt_leave={out.count(b'\x1b[?1049l')} max_row_addressed={max_row}"
)
# Which checks this SIZE can show: short frames shed the chip rows and the
# activity column; tall frames fit the whole turn so no sticky band pins.
tall = rows >= 30


# ---- TUI6 item 6: two-rule band anatomy on the captured frames ----
def rule_in(grid, r):
    return r in grid and grid[r].count("\u2500") >= 20


def band_rules(grid, needle, below_span=3):
    band = sorted(r for r, t in grid.items() if needle in t)
    if not band:
        return False
    above = rule_in(grid, band[0] - 1)
    below = any(rule_in(grid, band[-1] + d) for d in range(1, below_span + 1))
    return above and below


final_grid = probelib.screen_rows(final_bytes)
aura_band = band_rules(aura_grid, "speak or type")
session_band = band_rules(final_grid, "message haider")
detail = "SKIP" if not tall else None
sticky = "SKIP" if tall else None
# CI-as-debugger: the amber card misses ONLY on CI runners (local passes at
# every pacing/env parity tried) — on a miss, dump the captured frames so
# the runner names what actually painted.
if tall and not amber_painted(sub_paint):
    # Every DISTINCT printable run painted since boot — names the script
    # variant the runner actually played (byte tails only showed diffs).
    # ONE line: the ladder tails only 25 lines of a failing probe's output,
    # so a multi-line dump gets cut by the verdict booleans (round 14).
    _runs = re.findall(r"[ -~]{8,}", sub_paint)
    _uniq = list(dict.fromkeys(_runs))[-60:]
    sys.stderr.write("AMBER-MISS RUNS: " + " │ ".join(_uniq) + "\n")
probelib.verdict(
    "PTY_PROBE_SUB",
    out,
    child_clean,
    [
        ("subtree_header_painted", "subagents" in sub_paint),
        ("chip_glyph_painted", detail or (("├─" in sub_paint) or ("└─" in sub_paint))),
        ("amber_question_painted", detail or amber_painted(sub_paint)),
        ("aura_bar_painted", "AURA" in aura_paint),
        # TUI6.1 fix 2 re-scope: at 90x10 the orb is OPTIONAL content and
        # now yields to the reserved closing rule (review r1's aura
        # repro demanded exactly this trade), so the orb check gates on
        # tall frames.
        (
            "aura_orb_painted",
            (("IDLE" in aura_paint) or ("THINKING" in aura_paint)) if tall else "SKIP",
        ),
        ("aura_spawn_logged", detail or ("tests green" in text)),
        # NB: the header's product/version are separate styled spans, so an
        # SGR escape splits "haider v" — the session line's dim run is
        # contiguous.
        ("back_to_session", "branch main" in final),
        ("final_has_subtree", "subagents" in final),
        ("sticky_prompt_pinned", sticky or ("use two subagents" in scrolled)),
        # S2 neutral dark: barBg = white @0.04 over #0f0f0f → 25;25;25.
        ("sticky_band_ground", sticky or ("48;2;25;25;25" in scrolled)),
        # TUI6 item 6: both band rules on the aura stage and the session
        # (the closing rule sheds by the ledger at short frames — tall
        # runs enforce, short runs SKIP loudly).
        ("aura_band_two_rules", aura_band if tall else "SKIP"),
        ("session_band_two_rules", session_band if tall else "SKIP"),
        # TUI6.1 fix 4: the chip view's own band, composer AND question
        # forms, off full repaints of the live runtime.
        ("chipview_composer_band_two_rules", chipview_band),
        ("chipview_question_band_two_rules", question_band_cv),
    ],
)
