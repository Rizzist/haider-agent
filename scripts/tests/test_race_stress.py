import os
import pathlib
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "race-stress.sh"


class RaceStressHarnessTests(unittest.TestCase):
    def test_catalog_mode_lists_real_linux_cases_without_building(self):
        result = subprocess.run(
            ["bash", str(SCRIPT), "--catalog", "--platform", "linux"],
            cwd=ROOT,
            check=True,
            text=True,
            capture_output=True,
        )
        lines = result.stdout.splitlines()
        self.assertEqual(
            lines[0], "id\tplatform\tpackage\tsuite\ttest\tsource"
        )
        self.assertGreater(len(lines), 40)
        self.assertTrue(any("accounts-vault-replacement" in line for line in lines))

    def test_counted_run_refuses_to_claim_an_unloaded_pass(self):
        result = subprocess.run(
            [
                "bash",
                str(SCRIPT),
                "--iterations",
                "1",
                "--platform",
                "linux",
                "--case",
                "android-completion-waiters",
            ],
            cwd=ROOT,
            check=False,
            text=True,
            capture_output=True,
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("counted runs require --under-load", result.stderr)

    def test_loaded_run_precompiles_then_counts_exactly_one_test(self):
        with tempfile.TemporaryDirectory() as directory:
            temporary = pathlib.Path(directory)
            log = temporary / "cargo.log"
            cargo = temporary / "cargo"
            cargo.write_text(
                """#!/usr/bin/env bash
printf '%s\\n' "$*" >> "$FAKE_CARGO_LOG"
case " $* " in
  *" --no-run "*) exit 0 ;;
esac
printf 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'
""",
                encoding="utf-8",
            )
            cargo.chmod(0o755)
            env = dict(os.environ)
            env["PATH"] = f"{temporary}:{env['PATH']}"
            env["FAKE_CARGO_LOG"] = str(log)
            result = subprocess.run(
                [
                    "bash",
                    str(SCRIPT),
                    "--under-load",
                    "--iterations",
                    "1",
                    "--platform",
                    "linux",
                    "--case",
                    "android-completion-waiters",
                ],
                cwd=ROOT,
                env=env,
                check=True,
                text=True,
                capture_output=True,
            )
            calls = log.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(calls), 2)
            self.assertIn("--no-run", calls[0])
            self.assertNotIn("--no-run", calls[1])
            self.assertIn("all 1 cases passed 1/1 under load", result.stdout)


if __name__ == "__main__":
    unittest.main()
