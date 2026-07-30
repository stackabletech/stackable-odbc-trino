#!/usr/bin/env python3
"""Unit tests for the shared suite harness.

Standard library only, and needs no running stack -- this is the one suite in
`suites/` that tests the test infrastructure rather than the driver.
"""

import io
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stdout

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Results  # noqa: E402


class TestResults(unittest.TestCase):
    def test_ok_counts_and_prints_pass(self):
        r = Results("t")
        out = io.StringIO()
        with redirect_stdout(out):
            r.ok("a thing")
        self.assertEqual(r.passed, 1)
        self.assertEqual(r.failed, 0)
        self.assertIn("PASS  a thing", out.getvalue())

    def test_bad_counts_and_prints_fail_with_detail(self):
        r = Results("t")
        out = io.StringIO()
        with redirect_stdout(out):
            r.bad("a thing", "because reasons")
        self.assertEqual(r.failed, 1)
        self.assertIn("FAIL  a thing: because reasons", out.getvalue())

    def test_check_returns_its_condition(self):
        r = Results("t")
        with redirect_stdout(io.StringIO()):
            self.assertTrue(r.check("yes", True))
            self.assertFalse(r.check("no", False))
        self.assertEqual((r.passed, r.failed), (1, 1))

    def test_run_records_a_raising_callable_as_a_failure(self):
        r = Results("t")
        out = io.StringIO()
        with redirect_stdout(out):
            r.run("explodes", lambda: (_ for _ in ()).throw(ValueError("boom")))
        self.assertEqual(r.failed, 1)
        self.assertIn("boom", out.getvalue())

    def test_run_prints_elapsed_time(self):
        r = Results("t")
        out = io.StringIO()
        with redirect_stdout(out):
            r.run("quick", lambda: None)
        self.assertEqual(r.passed, 1)
        self.assertRegex(out.getvalue(), r"PASS  quick  \(\d+\.\ds\)")

    def test_skip_is_counted_separately_and_names_a_reason(self):
        r = Results("t")
        out = io.StringIO()
        with redirect_stdout(out):
            r.skip("tls", "profile 'oauth' is not active")
        self.assertEqual((r.passed, r.failed, r.skipped), (0, 0, 1))
        self.assertIn("SKIP  tls: profile 'oauth' is not active", out.getvalue())

    def test_note_is_neither_pass_nor_fail(self):
        r = Results("t")
        with redirect_stdout(io.StringIO()):
            r.note("observation", "a statement may be allocated before connecting")
        self.assertEqual((r.passed, r.failed, r.notes), (0, 0, 1))

    def test_summary_exit_code_is_zero_only_when_nothing_failed(self):
        r = Results("t")
        with redirect_stdout(io.StringIO()):
            r.ok("a")
            self.assertEqual(r.summary(), 0)
            r.bad("b")
            self.assertEqual(r.summary(), 1)

    def test_summary_reports_skips_so_they_cannot_read_as_passes(self):
        r = Results("t")
        out = io.StringIO()
        with redirect_stdout(out):
            r.ok("a")
            r.skip("b", "no profile")
            r.summary()
        self.assertIn("1 passed, 0 failed, 1 skipped", out.getvalue())


if __name__ == "__main__":
    unittest.main()
