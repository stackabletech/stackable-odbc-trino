#!/usr/bin/env python3
"""Shared machinery for the integration suites.

Standard library only, and `pyodbc` is imported lazily inside `Stack.connect`.
`test_c_abi.py` and `test_type_matrix.py` load the driver's `.so` with `ctypes`
and deliberately have no dependency on a Driver Manager, on `uv` or on pyodbc;
a module-scope pyodbc import here would give them one silently.
"""

import time


class Results:
    """PASS/FAIL/NOTE/SKIP accounting for one suite run.

    `bad` rather than `fail` because `test_type_matrix.py` has its own
    module-level `fail(kind, detail)` with different semantics, and a silent
    collision is worse than an unlovely name.
    """

    def __init__(self, title):
        self.title = title
        self.passed = 0
        self.failed = 0
        self.notes = 0
        self.skipped = 0

    def ok(self, label, detail=""):
        self.passed += 1
        print(f"PASS  {label}{': ' + detail if detail else ''}")

    def bad(self, label, detail=""):
        self.failed += 1
        print(f"FAIL  {label}{': ' + detail if detail else ''}")

    def check(self, label, cond, detail=""):
        """Record a boolean assertion. Returns the condition, so a caller can
        skip dependent work without re-evaluating it."""
        if cond:
            self.ok(label, detail)
        else:
            self.bad(label, detail)
        return bool(cond)

    def run(self, label, fn):
        """Run a callable, recording an exception as a failure with its
        message. Prints elapsed time: a suite that slows down is a finding."""
        t0 = time.monotonic()
        try:
            fn()
            print(f"PASS  {label}  ({time.monotonic() - t0:.1f}s)")
            self.passed += 1
        except Exception as e:
            print(f"FAIL  {label}  ({time.monotonic() - t0:.1f}s): {e}")
            self.failed += 1

    def note(self, label, text):
        """An observation the driver is entitled to make either way -- not a
        gap, and never counted as a pass."""
        self.notes += 1
        print(f"NOTE  {label}: {text}")

    def skip(self, label, reason):
        """A test that did not run. The reason is mandatory: an unrun test must
        never be indistinguishable from a passing one."""
        self.skipped += 1
        print(f"SKIP  {label}: {reason}")

    def summary(self):
        parts = [f"{self.passed} passed", f"{self.failed} failed"]
        if self.skipped:
            parts.append(f"{self.skipped} skipped")
        if self.notes:
            parts.append(f"{self.notes} notes")
        print(f"\n{', '.join(parts)}")
        return 1 if self.failed else 0
