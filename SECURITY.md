# Security Policy

## Reporting a Vulnerability

Please report security vulnerabilities privately, not through a public issue.

The preferred channel is GitHub's private vulnerability reporting: open the
**Security** tab of this repository and choose **Report a vulnerability**. This
reaches the maintainers directly and keeps the report confidential until a fix
is available.

If you cannot use that channel, email `info@stackable.tech` with `SECURITY` in
the subject line.

Please include the driver version, the platform and Driver Manager in use, the
Trino version where relevant, and the steps needed to reproduce the issue.

## What to Expect

We aim to acknowledge a report within three working days and to give an initial
assessment within ten. We will keep you informed while a fix is prepared, and we
will credit you in the advisory unless you ask us not to.

## Supported Versions

Security fixes are made against the most recent release and the `main` branch.
While the driver is below 1.0, fixes are not backported to earlier releases:
upgrade to the current release to receive them.

## Disclosure

Fixed vulnerabilities are published as GitHub Security Advisories against this
repository, naming the affected versions and the release that carries the fix.
