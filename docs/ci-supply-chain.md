---
title: "CI and release supply-chain controls"
description: "How pacto-bot-api pins Actions, scopes permissions, and publishes to PyPI without silent OIDC switches."
date: 2026-11-07
---

# CI and release supply-chain controls

This document describes the GitHub Actions hardening used by
[`ci.yml`](https://github.com/covenant-gov/pacto-bot-api/blob/main/.github/workflows/ci.yml),
[`release.yml`](https://github.com/covenant-gov/pacto-bot-api/blob/main/.github/workflows/release.yml),
and
[`prep-release.yml`](https://github.com/covenant-gov/pacto-bot-api/blob/main/.github/workflows/prep-release.yml).

## Controls in place

| Control | Behavior |
|---|---|
| Action pins | Third-party `uses:` entries are full commit SHAs with a trailing version comment (for example `# v4.4.0`). |
| Default permissions | Workflows set a restrictive default (`contents: read` or `{}`). Jobs that comment, open PRs, create releases, or push packages restate the permissions they need. |
| Checkout credentials | `persist-credentials: false` on checkouts that do not need the default git remote credentials. |
| Tool installs | Version-pinned `cargo install ... --locked` for `cargo-zigbuild` and `cargo-generate` (both publish a `Cargo.lock`). |
| Secret isolation | Python SDK install/build runs in `pypi-build` with no `pypi` environment. Only `pypi-publish` receives `secrets.PYPI_API_TOKEN`. |
| Manual release path | Tag push / `workflow_dispatch` release behavior is unchanged. Prep Release still opens a review PR; tagging remains manual after merge. |

Dependabot also watches the `github-actions` ecosystem so SHA pins can be refreshed through normal dependency PRs.

## PyPI publishing today (compatible path)

Release publishing still uses the existing GitHub Environment named `pypi` and
the long-lived repository/environment secret `PYPI_API_TOKEN` with
[`pypa/gh-action-pypi-publish`](https://github.com/pypa/gh-action-pypi-publish).

That is intentional. Trusted Publishing (OIDC) requires maintainer configuration
on both GitHub and PyPI. Switching the workflow to `id-token: write` / OIDC
before that configuration exists would break releases.

## Migrating to PyPI Trusted Publishing (admin prerequisites)

Do **not** merge an OIDC-only publish change until every step below is verified.

1. Confirm the PyPI project [`pacto-bot-sdk`](https://pypi.org/p/pacto-bot-sdk)
   is owned by a maintainer who can manage publishing.
2. In PyPI → project publishing settings, add a Trusted Publisher for this
   repository:
   - Owner: `covenant-gov`
   - Repository: `pacto-bot-api`
   - Workflow: `release.yml`
   - Environment: `pypi`
3. Confirm the GitHub Environment `pypi` still exists and that any required
   reviewers / wait timers match the release process you want.
4. In a follow-up PR (after the publisher is live), change `pypi-publish` to:
   - `permissions: { id-token: write }` (plus any other required scopes)
   - remove `password: ${{ secrets.PYPI_API_TOKEN }}` from the publish action
   - keep the build/publish job split so install/build never sees publish creds
5. Run one non-production verification (for example TestPyPI trusted publisher,
   or a canary tag agreed by maintainers) before deleting `PYPI_API_TOKEN`.
6. Only after a successful trusted publish, rotate/delete the long-lived token
   in GitHub and PyPI.

Until step 4–6 are complete, retain `PYPI_API_TOKEN` and the current password-
based publish step.

## What this does not change

- Branch filters (`main` for CI; `v*` tags for release publish jobs)
- Manual `workflow_dispatch` for Release and Prep Release
- Existing test / coverage / cargo-deny gates
- [`CODEOWNERS`](https://github.com/covenant-gov/pacto-bot-api/blob/main/CODEOWNERS)
  ownership (`@covenant-gov/pacto-maintainers`)
