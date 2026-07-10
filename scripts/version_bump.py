#!/usr/bin/env python3
"""Bump pacto-bot-api version strings across the repository.

Usage:
    python3 scripts/version_bump.py <daemon-version> [--sdk-version <version>] [--contract-version <version>]

This script is invoked by scripts/prep-release.sh. It updates all embedded
version references in a single pass and then validates consistency.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path


def update_cargo_toml(root: Path, daemon_version: str) -> None:
    path = root / "Cargo.toml"
    text = path.read_text()
    text = re.sub(
        r'^version = "[^"]+"',
        f'version = "{daemon_version}"',
        text,
        count=1,
        flags=re.MULTILINE,
    )
    path.write_text(text)
    print(f"Cargo.toml: version -> {daemon_version}")


def update_jsonrpc_contract(root: Path, contract_version: str) -> None:
    path = root / "schemas" / "jsonrpc.json"
    text = path.read_text()
    text = re.sub(
        r'"version": "[^"]+"',
        f'"version": "{contract_version}"',
        text,
        count=1,
    )
    path.write_text(text)
    print(f"schemas/jsonrpc.json: info.version -> {contract_version}")


def update_python_sdk(root: Path, sdk_version: str) -> None:
    pyproject = root / "python" / "pyproject.toml"
    text = pyproject.read_text()
    text = re.sub(
        r'^version = "[^"]+"',
        f'version = "{sdk_version}"',
        text,
        count=1,
        flags=re.MULTILINE,
    )
    pyproject.write_text(text)

    init = root / "python" / "src" / "pacto_bot_sdk" / "__init__.py"
    text = init.read_text()
    text = re.sub(
        r'^__version__ = "[^"]+"',
        f'__version__ = "{sdk_version}"',
        text,
        count=1,
        flags=re.MULTILINE,
    )
    init.write_text(text)
    print(f"python SDK: version -> {sdk_version}")


def upper_minor(version: str) -> str:
    parts = version.split("-")[0].split(".")
    return f"{parts[0]}.{int(parts[1]) + 1}.0"


def update_template_manifest(
    root: Path, daemon_version: str, sdk_version: str, contract_version: str
) -> None:
    path = root / "tests" / "fixtures" / "templates" / "python-llm" / "manifest.toml"
    text = path.read_text()

    text = re.sub(
        r'contract = \{ name = "pacto-contract", range = ">=[^,]+, <[^"]+" \}',
        f'contract = {{ name = "pacto-contract", range = ">={contract_version}, <{upper_minor(contract_version)}" }}',
        text,
    )
    text = re.sub(
        r'sdk = \{ name = "pacto-bot-sdk", range = ">=[^,]+, <[^"]+" \}',
        f'sdk = {{ name = "pacto-bot-sdk", range = ">={sdk_version}, <{upper_minor(sdk_version)}" }}',
        text,
    )
    text = re.sub(
        r'daemon = \{ range = ">=[^,]+, <[^"]+" \}',
        f'daemon = {{ range = ">={daemon_version}, <{upper_minor(daemon_version)}" }}',
        text,
    )
    path.write_text(text)
    print(f"tests/fixtures/templates/python-llm/manifest.toml: compatibility ranges updated")


def update_template_bot_pyproject(root: Path, sdk_version: str) -> None:
    path = root / "tests" / "fixtures" / "templates" / "python-llm" / "bot" / "pyproject.toml"
    text = path.read_text()
    text = re.sub(
        r'pacto-bot-sdk>=\d+\.\d+\.\d+',
        f'pacto-bot-sdk>={sdk_version}',
        text,
    )
    path.write_text(text)
    print(f"bot template pyproject.toml: SDK floor -> {sdk_version}")


def update_examples_requirements(root: Path, sdk_version: str) -> None:
    path = root / "examples" / "requirements.txt"
    if not path.exists():
        print("examples/requirements.txt: not present, skipping")
        return
    text = path.read_text()
    text = re.sub(
        r'pacto-bot-sdk>=\d+\.\d+\.\d+',
        f'pacto-bot-sdk>={sdk_version}',
        text,
    )
    path.write_text(text)
    print(f"examples/requirements.txt: SDK floor -> {sdk_version}")


def update_bundled_version_tests(root: Path, sdk_version: str, contract_version: str) -> None:
    path = root / "src" / "scaffold" / "cache.rs"
    text = path.read_text()

    text = re.sub(
        r'fn bundled_contract_version_parses\(\) \{\s*let version = bundled_contract_version\(\)\.unwrap\(\);\s*assert_eq!\(version, semver::Version::new\(\d+, \d+, \d+\)\);',
        f'fn bundled_contract_version_parses() {{\n        let version = bundled_contract_version().unwrap();\n        assert_eq!(version, semver::Version::new({contract_version.replace(".", ", ")}));',
        text,
    )

    text = re.sub(
        r'fn bundled_sdk_version_parses\(\) \{\s*let version = bundled_sdk_version\(\)\.unwrap\(\);\s*assert_eq!\(version, semver::Version::new\(\d+, \d+, \d+\)\);',
        f'fn bundled_sdk_version_parses() {{\n        let version = bundled_sdk_version().unwrap();\n        assert_eq!(version, semver::Version::new({sdk_version.replace(".", ", ")}));',
        text,
    )

    path.write_text(text)
    print(f"src/scaffold/cache.rs: bundled version test expectations updated")


def update_scaffold_lock_versions(root: Path, current_daemon: str, new_daemon: str) -> None:
    paths = [
        root / "src" / "scaffold" / "lock.rs",
        root / "src" / "scaffold" / "update.rs",
        root / "tests" / "admin_cli_scaffold.rs",
    ]
    for path in paths:
        text = path.read_text()
        new_text = re.sub(rf'"{re.escape(current_daemon)}"', f'"{new_daemon}"', text)
        # Fallback for the integration-test assertion that checks the generated
        # lock file text: version = "X.Y.Z".
        if new_text == text:
            new_text = re.sub(
                rf'version = "{re.escape(current_daemon)}"',
                f'version = "{new_daemon}"',
                text,
            )
        if new_text != text:
            path.write_text(new_text)
            print(f"{path.relative_to(root)}: admin version {current_daemon} -> {new_daemon}")
        else:
            print(f"{path.relative_to(root)}: no admin version {current_daemon} found")


def update_agents_readme(root: Path, current_daemon: str, new_daemon: str) -> None:
    agents = root / "AGENTS.md"
    text = agents.read_text()
    text = re.sub(rf"\b{re.escape(current_daemon)}\b", new_daemon, text)
    agents.write_text(text)
    print(f"AGENTS.md: {current_daemon} -> {new_daemon}")

    readme = root / "README.md"
    text = readme.read_text()
    text = re.sub(
        rf"PACTO_VERSION={re.escape(current_daemon)}",
        f"PACTO_VERSION={new_daemon}",
        text,
    )
    readme.write_text(text)
    print(f"README.md: PACTO_VERSION -> {new_daemon}")


def move_changelog_unreleased(
    path: Path, new_version: str, prev_version: str, release_date: str, repo_url: str | None
) -> None:
    text = path.read_text()
    lines = text.splitlines()

    unreleased_idx = None
    next_version_idx = None
    for i, line in enumerate(lines):
        if line.strip() == "## [Unreleased]":
            unreleased_idx = i
        elif unreleased_idx is not None and line.startswith("## ["):
            next_version_idx = i
            break

    if unreleased_idx is None:
        print(f"error: could not find ## [Unreleased] section in {path}", file=sys.stderr)
        sys.exit(1)
    if next_version_idx is None:
        print(f"error: could not find version section after [Unreleased] in {path}", file=sys.stderr)
        sys.exit(1)

    unreleased_body = lines[unreleased_idx + 1 : next_version_idx]
    new_section = [
        "## [Unreleased]",
        "",
        f"## [{new_version}] - {release_date}",
    ] + unreleased_body

    new_lines = lines[:unreleased_idx] + new_section + lines[next_version_idx:]

    if repo_url is not None:
        footer_lines: list[str] = []
        unreleased_link_found = False
        new_link_added = False
        for line in new_lines:
            if line.startswith("[Unreleased]:"):
                footer_lines.append(f"[Unreleased]: {repo_url}/compare/v{new_version}...HEAD")
                unreleased_link_found = True
            elif line.startswith(f"[{prev_version}]:"):
                footer_lines.append(f"[{new_version}]: {repo_url}/compare/v{prev_version}...v{new_version}")
                footer_lines.append(line)
                new_link_added = True
            else:
                footer_lines.append(line)

        if not unreleased_link_found:
            print(f"error: could not find [Unreleased] compare link in {path}", file=sys.stderr)
            sys.exit(1)
        if not new_link_added:
            print(f"error: could not find [{prev_version}] compare link in {path}", file=sys.stderr)
            sys.exit(1)
        new_lines = footer_lines

    path.write_text("\n".join(new_lines) + "\n")
    print(f"{path}: added [{new_version}] - {release_date}")


def validate(
    root: Path, daemon_version: str, sdk_version: str, contract_version: str
) -> None:
    errors: list[str] = []

    cargo = tomllib.loads((root / "Cargo.toml").read_text())
    if cargo["package"]["version"] != daemon_version:
        errors.append(
            f"Cargo.toml: version {cargo['package']['version']!r}, expected {daemon_version!r}"
        )

    rpc = json.loads((root / "schemas" / "jsonrpc.json").read_text())
    if rpc["info"]["version"] != contract_version:
        errors.append(
            f"schemas/jsonrpc.json: {rpc['info']['version']!r}, expected {contract_version!r}"
        )

    pyproject = tomllib.loads((root / "python" / "pyproject.toml").read_text())
    if pyproject["project"]["version"] != sdk_version:
        errors.append(
            f"python/pyproject.toml: {pyproject['project']['version']!r}, expected {sdk_version!r}"
        )

    init = (root / "python" / "src" / "pacto_bot_sdk" / "__init__.py").read_text()
    match = re.search(r'__version__\s*=\s*"([^"]+)"', init)
    if not match or match.group(1) != sdk_version:
        errors.append(
            f"python/src/pacto_bot_sdk/__init__.py: __version__ {match.group(1) if match else 'missing'!r}, expected {sdk_version!r}"
        )

    manifest = (root / "tests" / "fixtures" / "templates" / "python-llm" / "manifest.toml").read_text()
    if f'daemon = {{ range = ">={daemon_version}' not in manifest:
        errors.append(f"manifest.toml: missing daemon range for {daemon_version}")
    if f'sdk = {{ name = "pacto-bot-sdk", range = ">={sdk_version}' not in manifest:
        errors.append(f"manifest.toml: missing sdk range for {sdk_version}")
    if f'contract = {{ name = "pacto-contract", range = ">={contract_version}' not in manifest:
        errors.append(f"manifest.toml: missing contract range for {contract_version}")

    bot_pyproject = (
        root / "tests" / "fixtures" / "templates" / "python-llm" / "bot" / "pyproject.toml"
    ).read_text()
    if f"pacto-bot-sdk>={sdk_version}" not in bot_pyproject:
        errors.append(f"bot template pyproject.toml: missing sdk floor {sdk_version}")

    req = root / "examples" / "requirements.txt"
    if req.exists():
        req_text = req.read_text()
        if f"pacto-bot-sdk>={sdk_version}" not in req_text:
            errors.append(f"examples/requirements.txt: missing sdk floor {sdk_version}")
    else:
        print("examples/requirements.txt: not present, skipping validation")

    cache_rs = (root / "src" / "scaffold" / "cache.rs").read_text()
    expected_contract_assert = f'assert_eq!(version, semver::Version::new({contract_version.replace(".", ", ")}));'
    expected_sdk_assert = f'assert_eq!(version, semver::Version::new({sdk_version.replace(".", ", ")}));'
    if expected_contract_assert not in cache_rs:
        errors.append(f"src/scaffold/cache.rs: bundled contract test expectation missing {contract_version}")
    if expected_sdk_assert not in cache_rs:
        errors.append(f"src/scaffold/cache.rs: bundled SDK test expectation missing {sdk_version}")

    if errors:
        print("error: version consistency check failed:", file=sys.stderr)
        for e in errors:
            print(f"  - {e}", file=sys.stderr)
        sys.exit(1)
    print("version consistency check passed")


def next_minor(version: str) -> str:
    parts = version.split("-")[0].split(".")
    parts[1] = str(int(parts[1]) + 1)
    parts[2] = "0"
    return ".".join(parts)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Bump pacto-bot-api version strings across the repository."
    )
    parser.add_argument("daemon_version", help="New daemon version (e.g. 0.7.0)")
    parser.add_argument(
        "--sdk-version",
        default=None,
        help="Override Python SDK version (default: next minor of current SDK).",
    )
    parser.add_argument(
        "--contract-version",
        default=None,
        help="Override JSON-RPC contract version (default: daemon version).",
    )
    parser.add_argument(
        "--release-date",
        default=None,
        help="Release date used in changelogs (default: today).",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    root = Path(__file__).resolve().parent.parent

    current_daemon = tomllib.loads((root / "Cargo.toml").read_text())["package"]["version"]
    current_sdk = tomllib.loads((root / "python" / "pyproject.toml").read_text())["project"][
        "version"
    ]
    current_contract = json.loads((root / "schemas" / "jsonrpc.json").read_text())["info"][
        "version"
    ]

    new_daemon = args.daemon_version
    new_sdk = args.sdk_version or next_minor(current_sdk)
    new_contract = args.contract_version or new_daemon
    release_date = args.release_date or "TODAY"

    if new_daemon == current_daemon:
        print(f"error: {new_daemon} is already the current daemon version", file=sys.stderr)
        return 1

    repo_url = None
    with open(root / "Cargo.toml", "rb") as f:
        cargo = tomllib.load(f)
    if "package" in cargo and "repository" in cargo["package"]:
        repo_url = cargo["package"]["repository"].removesuffix(".git")

    update_cargo_toml(root, new_daemon)
    update_jsonrpc_contract(root, new_contract)
    update_python_sdk(root, new_sdk)
    update_template_manifest(root, new_daemon, new_sdk, new_contract)
    update_template_bot_pyproject(root, new_sdk)
    update_examples_requirements(root, new_sdk)
    update_bundled_version_tests(root, new_sdk, new_contract)
    update_scaffold_lock_versions(root, current_daemon, new_daemon)
    update_agents_readme(root, current_daemon, new_daemon)

    move_changelog_unreleased(
        root / "CHANGELOG.md", new_daemon, current_daemon, release_date, repo_url
    )
    move_changelog_unreleased(
        root / "python" / "CHANGELOG.md", new_sdk, current_sdk, release_date, None
    )

    validate(root, new_daemon, new_sdk, new_contract)
    return 0


if __name__ == "__main__":
    sys.exit(main())
