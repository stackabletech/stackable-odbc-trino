#!/usr/bin/env python3
"""The browser half of Trino's interactive OAuth 2.0 flow, for tests.

The driver presents a login URL through `open::that`, which on Linux runs
`xdg-open` first and unconditionally. `open` 5.4.0 ignores `$BROWSER`, as its own
documentation says, so `test_oauth.py` writes a shim named `xdg-open` that execs
this file and prepends its directory to PATH before connecting.

**This script always exits 0.** A non-zero exit sends `open::that` on to the next
opener in its list, `gio open`, which opens a real browser window on any machine
with a desktop session. Outcomes are reported through the JSONL file named by
ODBC_TEST_OAUTH_RECORD instead, one object per invocation. That file is also
what turns a broken login into a diagnosis rather than a suite that waits out
`ExternalAuthenticationTimeout` with nothing to show.

Modes, from ODBC_TEST_OAUTH_MODE:

    login   complete the login against Keycloak
    noop    record the URL and complete nothing, so the login is abandoned
    deny    answer Trino's callback with error=access_denied, which is the
            redirect an identity provider sends when the person declines

Usage, normally through the shim:

    ODBC_TEST_OAUTH_MODE=login ODBC_TEST_OAUTH_RECORD=/tmp/rec.jsonl \\
        python3 integration-tests/suites/oauth_browser.py <login-url>
"""

import html as html_mod
import http.cookiejar
import json
import os
import re
import ssl
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from harness import Stack  # noqa: E402

# Keycloak's login form. Its action carries a single-use session code, so it has
# to be read out of the page rather than constructed. The id and the action can
# appear in either order in the tag, hence two expressions rather than one.
LOGIN_FORM_TAG = re.compile(r"<form[^>]*kc-form-login[^>]*>", re.I)
ANY_FORM_TAG = re.compile(r"<form[^>]*>", re.I)
FORM_ACTION = re.compile(r'action="([^"]+)"', re.I)

TIMEOUT_SECONDS = 30


def record(path, entry):
    if not path:
        return
    with open(path, "a", encoding="utf-8") as f:
        f.write(json.dumps(entry) + "\n")


def build_opener(ca_cert):
    """An opener that trusts the test CA and keeps cookies.

    Keycloak carries its authentication session in cookies and the CA is not a
    public one, so both are required; a default opener fails the handshake.
    """
    ctx = ssl.create_default_context(cafile=ca_cert)
    return urllib.request.build_opener(
        urllib.request.HTTPSHandler(context=ctx),
        urllib.request.HTTPCookieProcessor(http.cookiejar.CookieJar()),
    )


def fetch(op, url, data=None):
    """GET, or POST when `data` is given, following redirects.

    Returns (status, final_url, body). An HTTP error status is returned rather
    than raised: Trino answers its own callback with a status this script does
    not control, and a 4xx there is still a delivered redirect.
    """
    req = urllib.request.Request(
        url,
        data=urllib.parse.urlencode(data).encode() if data else None,
        headers={"User-Agent": "stackable-odbc-trino-test-browser"},
    )
    try:
        with op.open(req, timeout=TIMEOUT_SECONDS) as resp:
            return resp.status, resp.geturl(), resp.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.geturl(), e.read().decode("utf-8", "replace")


def login_form_action(page, base_url):
    """The absolute URL the login form posts to, or None."""
    tag = LOGIN_FORM_TAG.search(page) or ANY_FORM_TAG.search(page)
    if not tag:
        return None
    action = FORM_ACTION.search(tag.group(0))
    if not action:
        return None
    # The action is HTML-escaped in the page and may be relative.
    return urllib.parse.urljoin(base_url, html_mod.unescape(action.group(1)))


def do_login(op, url, user, password):
    """Follow the login URL to Keycloak, submit the form, land on the callback."""
    _status, page_url, page = fetch(op, url)
    action = login_form_action(page, page_url)
    if action is None:
        raise RuntimeError(f"no login form at {page_url}; page begins {page[:200]!r}")

    _status, final_url, page = fetch(
        op, action, {"username": user, "password": password, "credentialId": ""}
    )
    if LOGIN_FORM_TAG.search(page):
        # Keycloak re-renders the login page with an error rather than
        # redirecting, so the credentials were rejected.
        raise RuntimeError(
            f"still on the Keycloak login page after posting credentials: {final_url}"
        )
    return final_url


def do_deny(op, url):
    """Answer Trino's callback the way a refusing identity provider would.

    The login URL redirects to Keycloak's authorization endpoint, whose query
    carries both the `state` Trino signed and the `redirect_uri` it asked to be
    called back on. Handing that state back on that URL with `error` set, and no
    code, is exactly what an identity provider sends when the person declines,
    so this needs no Keycloak interaction at all.
    """
    _status, auth_url, _page = fetch(op, url)
    query = urllib.parse.parse_qs(urllib.parse.urlparse(auth_url).query)
    state = query.get("state", [None])[0]
    redirect_uri = query.get("redirect_uri", [None])[0]
    if not state or not redirect_uri:
        raise RuntimeError(f"no state or redirect_uri on the authorization URL: {auth_url}")

    denied = (
        redirect_uri
        + "?"
        + urllib.parse.urlencode(
            {
                "error": "access_denied",
                "error_description": "the test declined the login",
                "state": state,
            }
        )
    )
    status, final_url, _page = fetch(op, denied)
    return f"{final_url} (HTTP {status})"


def main(argv):
    started = time.monotonic()
    record_path = os.environ.get("ODBC_TEST_OAUTH_RECORD")
    mode = os.environ.get("ODBC_TEST_OAUTH_MODE", "login")
    url = argv[1] if len(argv) > 1 else ""
    entry = {"mode": mode, "url": url}

    try:
        if not url:
            raise RuntimeError("no URL argument")
        stack = Stack.load()
        if mode == "noop":
            entry["outcome"] = "presented"
        elif mode == "login":
            op = build_opener(stack.get("CA_CERT"))
            entry["final_url"] = do_login(
                op, url, stack.get("KEYCLOAK_USER"), stack.get("KEYCLOAK_PASSWORD")
            )
            entry["outcome"] = "logged-in"
        elif mode == "deny":
            entry["final_url"] = do_deny(build_opener(stack.get("CA_CERT")), url)
            entry["outcome"] = "denied"
        else:
            raise RuntimeError(f"unknown ODBC_TEST_OAUTH_MODE {mode!r}")
    except Exception as e:  # noqa: BLE001 - every failure is recorded, never raised
        entry["outcome"] = "error"
        entry["error"] = f"{type(e).__name__}: {e}"

    entry["seconds"] = round(time.monotonic() - started, 2)
    record(record_path, entry)
    # Always zero, whatever happened. A non-zero exit makes `open::that` try
    # `gio open` next, and a real browser window appears.
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
