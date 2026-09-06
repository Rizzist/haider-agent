"""Model POSIX archive modes even on Windows, which cannot chmod execute bits.

Only supplied fixture paths have synthetic modes. Native postpack CI additionally
checks the real modes extracted by dpkg/rpm/pkgutil on their own platforms.
"""
import os
from pathlib import Path
from unittest.mock import patch


def model_modes(test, modes):
    original = Path.stat

    def stat(path, *args, **kwargs):
        result = original(path, *args, **kwargs)
        if path in modes:
            values = list(result)
            values[0] = (values[0] & ~0o777) | modes[path]
            return os.stat_result(values)
        return result

    replacement = patch.object(Path, 'stat', stat)
    replacement.start()
    test.addCleanup(replacement.stop)
