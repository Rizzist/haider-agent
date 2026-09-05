"""Pin command-owned card anatomy without accepting a footer-only flash."""

import ast
from pathlib import Path
import unittest
from types import SimpleNamespace
from unittest import mock

from gate.loader import load_check
from gate.tui_probe import TuiProcess


class TerminalPlainTextTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # Execute the actual portable parser definitions without importing
        # the unrelated POSIX process harness on Windows test runners.
        path = Path(__file__).resolve().parents[2] / "tui-probes/probelib.py"
        nodes = [
            node for node in ast.parse(path.read_text()).body
            if (isinstance(node, ast.Import) and any(alias.name == "re" for alias in node.names))
            or (isinstance(node, ast.Assign) and any(
                isinstance(target, ast.Name) and target.id == "ANSI_RE" for target in node.targets
            ))
            or (isinstance(node, ast.FunctionDef) and node.name == "plain")
        ]
        namespace = {}
        exec(compile(ast.Module(body=nodes, type_ignores=[]), str(path), "exec"), namespace)
        cls.plain = staticmethod(namespace["plain"])

    def test_osc_bel_and_st_leave_following_visible_text(self):
        for terminator in (b"\x07", b"\x1b\\"):
            with self.subTest(terminator=terminator):
                self.assertEqual(
                    self.plain(b"\x1b]7791;attached=session-test" + terminator + b"message haider"),
                    b"message haider",
                )

    def test_mixed_osc_spans_cannot_erase_intervening_composer_frame(self):
        raw = (
            b"\x1b]7791;attached=session-test\x1b\\"
            b"\x1b[34;1Hmessage haider"
            b"\x1b]111\x07visible tail"
        )
        self.assertEqual(self.plain(raw), b"message haidervisible tail")

    def test_unterminated_osc_cannot_consume_a_later_visible_frame(self):
        raw = b"\x1b]unfinished\x1b[34;1Hmessage haider\x1b]111\x07"
        self.assertIn(b"message haider", self.plain(raw))


class PaletteActivationTests(unittest.TestCase):
    def setUp(self):
        self.module = load_check(
            Path(__file__).resolve().parents[1]
            / "checks/t0/t0.tui.palette_activation_closure.py", "t0"
        ).module

    def signature(self, before, after):
        return self.module._signature(
            "monitors", before, after, self.module.SURFACE_SIGNATURES
        )

    def test_empty_monitors_card_is_an_actual_new_surface(self):
        # Actual wide and narrow TUI card text from the real-daemon T0 probe.
        for controls in (
            "monitors  ↑↓/jk select · x stop · p pause · t trigger · e edit · y copy id · esc",
            "monitors  ↑↓/jk select · x stop · p pause · t trigger",
        ):
            with self.subTest(controls=controls):
                self.assertIsNotNone(self.signature("session ready", controls + "\n  no active monitors"))

    def test_monitors_flash_or_stale_card_cannot_satisfy_surface_oracle(self):
        card = "monitors · x stop · p pause · t trigger\nno active monitors"
        for before, after in (
            ("session ready", "session ready\n/monitors"),
            ("session ready", "session ready\nno active monitors"),
            ("session ready", "monitors · x stop · p pause · t trigger"),
            (card, card),
        ):
            with self.subTest(before=before, after=after):
                self.assertIsNone(self.signature(before, after))


class SessionComposerReadinessTests(unittest.TestCase):
    def probe(self, prior, frame, remaining=25.0):
        clock = [100.0]
        sizes = []
        tui = object.__new__(TuiProcess)
        tui.fd = 7
        tui.sink = [prior]
        tui.probe = SimpleNamespace(
            set_size=lambda _fd, cols, rows: sizes.append((cols, rows)),
            screen_rows=lambda raw: {34: raw.decode("utf-8")},
        )

        def pump(seconds):
            clock[0] += seconds
            if sizes[-1] == (118, 36):
                tui.sink[0] += frame

        tui.pump = pump
        with mock.patch("gate.tui_probe.time.monotonic", side_effect=lambda: clock[0]):
            ready = tui._wait_for_session_composer(clock[0] + remaining)
        return ready, clock[0] - 100.0, sizes

    def test_new_full_frame_proves_visible_composer_after_replay(self):
        ready, elapsed, sizes = self.probe(
            b"QA_PALETTE_HISTORY_READY [ IDLE ]",
            b"\x1b[34;1H message haider - send",
        )
        self.assertTrue(ready)
        self.assertLess(elapsed, 25)
        self.assertEqual(sizes, [(118, 35), (118, 36)])

    def test_stale_placeholder_and_session_replay_are_not_readiness(self):
        ready, elapsed, _sizes = self.probe(
            b"message haider from a previous paint",
            b"\x1b[34;1H start a session\x1b[10;1H QA_PALETTE_HISTORY_READY [ IDLE ]",
        )
        self.assertFalse(ready)
        self.assertAlmostEqual(elapsed, 25)

    def test_repaint_cannot_restart_or_overrun_existing_boot_deadline(self):
        ready, elapsed, sizes = self.probe(
            b"", b"\x1b[34;1H message haider - send", remaining=0.2,
        )
        self.assertFalse(ready)
        self.assertAlmostEqual(elapsed, 0.2)
        self.assertEqual(sizes, [(118, 35)])


if __name__ == "__main__":
    unittest.main()
