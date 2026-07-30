#!/usr/bin/env python3
"""Shared machinery for the integration suites.

Standard library only, and `pyodbc` is imported lazily inside `Stack.connect`.
`test_c_abi.py` and `test_type_matrix.py` load the driver's `.so` with `ctypes`
and deliberately have no dependency on a Driver Manager, on `uv` or on pyodbc;
a module-scope pyodbc import here would give them one silently.
"""

import os
import time

DEFAULT_STACK_ENV = os.path.join(
    os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    "generated",
    "stack.env",
)


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
        """An observation the driver is entitled to make either way. Not a
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


class Stack:
    """The running test stack, as described by `generated/stack.env`.

    One file describes the stack and both bash and Python read it, so a port,
    a credential or a certificate path is stated once. Every suite builds its
    connection strings from here rather than hardcoding one, which is what
    keeps a change like the move to HTTPS out of every suite at once.
    """

    def __init__(self, values):
        self._values = values

    @classmethod
    def load(cls, path=None):
        path = path or os.environ.get("ODBC_TEST_STACK_ENV") or DEFAULT_STACK_ENV
        if not os.path.exists(path):
            raise SystemExit(
                f"stack.env not found at {path}\nrun: ./integration-tests/setup.sh"
            )
        values = {}
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                key, _, value = line.partition("=")
                value = value.strip()
                if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                    value = value[1:-1]
                values[key.strip()] = value
        return cls(values)

    def get(self, key, default=None):
        return self._values.get(key, default)

    @property
    def profiles(self):
        return [p for p in self.get("PROFILES", "").split(",") if p]

    def has_profile(self, name):
        return name in self.profiles

    def conn_str(self, **overrides):
        """A DSN-less connection string. An override of `None` removes the key,
        which is how a suite tests, say, connecting with no `Password`."""
        params = {
            "Driver": self.get("DRIVER_PATH"),
            "Host": self.get("TRINO_HOST"),
            "Port": self.get("TRINO_HTTPS_PORT"),
            "User": self.get("TRINO_USER"),
            "Password": self.get("TRINO_PASSWORD"),
            "Protocol": "https",
            "Catalog": self.get("TRINO_CATALOG"),
            "Certificate": self.get("CA_CERT"),
        }
        params.update(overrides)
        return ";".join(f"{k}={v}" for k, v in params.items() if v is not None)

    def dsn(self, name):
        return f"DSN={name}"

    def connect(self, **overrides):
        """Connect through the Driver Manager. pyodbc is imported here rather
        than at module scope so the ctypes suites keep their zero dependencies."""
        import pyodbc

        return pyodbc.connect(self.conn_str(**overrides), autocommit=True)
