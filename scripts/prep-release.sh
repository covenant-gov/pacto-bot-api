#!/usr/bin/env bash
set -euo pipefail

# Prepare the repository for a new release.
#
# Usage:
#   ./scripts/prep-release.sh <daemon-version> [--sdk-version <version>] [--contract-version <version>]
#
# Examples:
#   ./scripts/prep-release.sh 0.6.0
#   ./scripts/prep-release.sh 0.6.0 --sdk-version 0.3.0 --contract-version 0.2.0
#
# The script performs these steps:
#   1. Validate the requested version(s) and working tree state.
#   2. Bump the daemon crate version in Cargo.toml and update Cargo.lock.
#   3. Bump the JSON-RPC contract version in schemas/jsonrpc.json.
#   4. Bump the Python SDK version in python/pyproject.toml and __init__.py.
#   5. Update compatibility ranges in the bundled python-llm template.
#   6. Update AGENTS.md, README.md, examples/requirements.txt version references.
#   7. Move the current [Unreleased] CHANGELOG entries into a new dated section
#      for both CHANGELOG.md and python/CHANGELOG.md.
#   8. Regenerate Rust types, Python SDK, and docs/pacto-bot-admin-llms.txt.
#   9. Run fmt-check, clippy, Python SDK tests, and the full Rust test suite.

usage() {
  cat <<'EOF' >&2
Usage: ./scripts/prep-release.sh <daemon-version> [--sdk-version <version>] [--contract-version <version>]

Options:
  --sdk-version       Override the Python SDK version. Defaults to the next
                      minor of the current SDK version.
  --contract-version  Override the JSON-RPC contract artifact version.
                      Defaults to the requested daemon version.

Examples:
  ./scripts/prep-release.sh 0.6.0
  ./scripts/prep-release.sh 0.6.0 --sdk-version 0.3.0 --contract-version 0.2.0
EOF
  exit 1
}

if [[ $# -lt 1 ]]; then
  usage
fi

new_daemon_version="$1"
shift

new_sdk_version=""
new_contract_version=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --sdk-version)
      [[ $# -ge 2 ]] || usage
      new_sdk_version="$2"
      shift 2
      ;;
    --contract-version)
      [[ $# -ge 2 ]] || usage
      new_contract_version="$2"
      shift 2
      ;;
    *)
      usage
      ;;
  esac
done

semver_re='^[0-9]+\.[0-9]+\.[0-9]+(-[a-zA-Z0-9.-]+)?$'
if [[ ! "$new_daemon_version" =~ $semver_re ]]; then
  echo "error: '$new_daemon_version' does not look like a valid SemVer version" >&2
  exit 1
fi
if [[ -n "$new_sdk_version" && ! "$new_sdk_version" =~ $semver_re ]]; then
  echo "error: '$new_sdk_version' does not look like a valid SemVer version" >&2
  exit 1
fi
if [[ -n "$new_contract_version" && ! "$new_contract_version" =~ $semver_re ]]; then
  echo "error: '$new_contract_version' does not look like a valid SemVer version" >&2
  exit 1
fi

# Ensure we are in the repository root.
cd "$(dirname "$0")/.."

if git status --short | grep -q .; then
  echo "error: working tree is not clean. Commit or stash changes first." >&2
  git status --short
  exit 1
fi

current_daemon_version="$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)"
if [[ "$new_daemon_version" == "$current_daemon_version" ]]; then
  echo "error: requested daemon version '$new_daemon_version' is already the current version" >&2
  exit 1
fi

current_sdk_version="$(python3 -c "import tomllib; print(tomllib.load(open('python/pyproject.toml','rb'))['project']['version'])")"
current_contract_version="$(python3 -c "import json; print(json.load(open('schemas/jsonrpc.json'))['info']['version'])")"

# Default SDK to next minor of current SDK version.
if [[ -z "$new_sdk_version" ]]; then
  new_sdk_version="$(python3 - "$current_sdk_version" <<'PY'
import sys
parts = sys.argv[1].split('-')[0].split('.')
parts[1] = str(int(parts[1]) + 1)
parts[2] = '0'
print('.'.join(parts))
PY
  )"
fi

# Default contract to daemon version.
if [[ -z "$new_contract_version" ]]; then
  new_contract_version="$new_daemon_version"
fi

repo_url="$(grep '^repository' Cargo.toml | head -1 | cut -d'"' -f2)"
# Strip a trailing .git from the URL for the compare links.
repo_url="${repo_url%.git}"
release_date="$(date +%Y-%m-%d)"

echo "==> Preparing release"
echo "    daemon:   $current_daemon_version -> $new_daemon_version"
echo "    sdk:      $current_sdk_version -> $new_sdk_version"
echo "    contract: $current_contract_version -> $new_contract_version"

# 1. Bump Cargo.toml version.
echo "==> Updating Cargo.toml version"
perl -pi -e "s/^version = \"$current_daemon_version\"/version = \"$new_daemon_version\"/" Cargo.toml

# 2. Update Cargo.lock.
echo "==> Updating Cargo.lock"
cargo update -p pacto-bot-api

# 3. Bump contract version in schemas/jsonrpc.json.
echo "==> Updating schemas/jsonrpc.json contract version"
python3 - "$current_contract_version" "$new_contract_version" <<'PY'
import json, sys
old, new = sys.argv[1:3]
with open("schemas/jsonrpc.json", "r") as f:
    data = json.load(f)
if data.get("info", {}).get("version") != old:
    print(f"warning: schemas/jsonrpc.json info.version was {data['info']['version']!r}, expected {old!r}", file=sys.stderr)
data["info"]["version"] = new
with open("schemas/jsonrpc.json", "w") as f:
    json.dump(data, f, indent=2)
    f.write("\n")
print(f"schemas/jsonrpc.json: {old} -> {new}")
PY

# 4. Bump Python SDK version.
echo "==> Updating Python SDK version"
perl -pi -e "s/^version = \"$current_sdk_version\"/version = \"$new_sdk_version\"/" python/pyproject.toml
perl -pi -e "s/^__version__ = \"$current_sdk_version\"/__version__ = \"$new_sdk_version\"/" python/src/pacto_bot_sdk/__init__.py

# 5. Update template compatibility ranges to match the new daemon/sdk/contract.
echo "==> Updating template compatibility ranges"
python3 - "$new_daemon_version" "$new_sdk_version" "$new_contract_version" <<'PY'
import re, sys
daemon, sdk, contract = sys.argv[1:4]

def upper(version):
    parts = version.split('-')[0].split('.')
    return f"{parts[0]}.{int(parts[1]) + 1}.0"

path = "tests/fixtures/templates/python-llm/manifest.toml"
with open(path, "r") as f:
    text = f.read()

text = re.sub(
    r'contract = \{ name = "pacto-contract", range = ">=[^,]+, <[^"]+" \}',
    f'contract = {{ name = "pacto-contract", range = ">={contract}, <{upper(contract)}" }}',
    text,
)
text = re.sub(
    r'sdk = \{ name = "pacto-bot-sdk", range = ">=[^,]+, <[^"]+" \}',
    f'sdk = {{ name = "pacto-bot-sdk", range = ">={sdk}, <{upper(sdk)}" }}',
    text,
)
text = re.sub(
    r'daemon = \{ range = ">=[^,]+, <[^"]+" \}',
    f'daemon = {{ range = ">={daemon}, <{upper(daemon)}" }}',
    text,
)

with open(path, "w") as f:
    f.write(text)
print(f"updated {path}")
PY

# Update the bot template's SDK floor.
perl -pi -e "s/pacto-bot-sdk>=$current_sdk_version/pacto-bot-sdk>=$new_sdk_version/g" tests/fixtures/templates/python-llm/bot/pyproject.toml

# 6. Update AGENTS.md and README.md daemon version references.
echo "==> Updating AGENTS.md version references"
perl -pi -e "s/\b$current_daemon_version\b/$new_daemon_version/g" AGENTS.md

echo "==> Updating README.md version reference"
perl -pi -e "s/PACTO_VERSION=$current_daemon_version/PACTO_VERSION=$new_daemon_version/g" README.md

# 7. Update examples/requirements.txt SDK floor.
echo "==> Updating examples/requirements.txt SDK version"
perl -pi -e "s/pacto-bot-sdk>=$current_sdk_version/pacto-bot-sdk>=$new_sdk_version/g" examples/requirements.txt

# 8. Update CHANGELOG.md.
echo "==> Updating CHANGELOG.md"
python3 - "$new_daemon_version" "$current_daemon_version" "$release_date" "$repo_url" <<'PY'
import sys

new_version, prev_version, release_date, repo_url = sys.argv[1:5]

with open("CHANGELOG.md", "r") as f:
    content = f.read()

lines = content.splitlines()

# Find the [Unreleased] header and the next version header.
unreleased_idx = None
next_version_idx = None
for i, line in enumerate(lines):
    if line.strip() == "## [Unreleased]":
        unreleased_idx = i
    elif unreleased_idx is not None and line.startswith("## ["):
        next_version_idx = i
        break

if unreleased_idx is None:
    print("error: could not find ## [Unreleased] section in CHANGELOG.md", file=sys.stderr)
    sys.exit(1)

if next_version_idx is None:
    print("error: could not find next version section after [Unreleased] in CHANGELOG.md", file=sys.stderr)
    sys.exit(1)

# Extract the body between the Unreleased header and the next version header.
unreleased_body = lines[unreleased_idx + 1:next_version_idx]

# Build the new section. Keep the body intact (may be empty).
new_section_lines = [
    "## [Unreleased]",
    "",
    f"## [{new_version}] - {release_date}",
] + unreleased_body

# Replace the old Unreleased section with the new one.
new_lines = lines[:unreleased_idx] + new_section_lines + lines[next_version_idx:]

# Update the footer compare links.
footer_lines = []
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
    print("error: could not find [Unreleased] compare link in CHANGELOG.md", file=sys.stderr)
    sys.exit(1)

if not new_link_added:
    print("error: could not find [prev_version] compare link in CHANGELOG.md", file=sys.stderr)
    sys.exit(1)

with open("CHANGELOG.md", "w") as f:
    f.write("\n".join(footer_lines) + "\n")

print(f"Updated CHANGELOG.md with [{new_version}] - {release_date}")
PY

# 9. Update python/CHANGELOG.md.
echo "==> Updating python/CHANGELOG.md"
python3 - "$new_sdk_version" "$current_sdk_version" "$release_date" <<'PY'
import sys

new_version, prev_version, release_date = sys.argv[1:4]

with open("python/CHANGELOG.md", "r") as f:
    content = f.read()

lines = content.splitlines()

# Find the [Unreleased] header and the next version header.
unreleased_idx = None
next_version_idx = None
for i, line in enumerate(lines):
    if line.strip() == "## [Unreleased]":
        unreleased_idx = i
    elif unreleased_idx is not None and line.startswith("## ["):
        next_version_idx = i
        break

if unreleased_idx is None:
    print("error: could not find ## [Unreleased] section in python/CHANGELOG.md", file=sys.stderr)
    sys.exit(1)

if next_version_idx is None:
    print("error: could not find next version section after [Unreleased] in python/CHANGELOG.md", file=sys.stderr)
    sys.exit(1)

unreleased_body = lines[unreleased_idx + 1:next_version_idx]

new_section_lines = [
    "## [Unreleased]",
    "",
    f"## [{new_version}] - {release_date}",
] + unreleased_body

new_lines = lines[:unreleased_idx] + new_section_lines + lines[next_version_idx:]

with open("python/CHANGELOG.md", "w") as f:
    f.write("\n".join(new_lines) + "\n")

print(f"Updated python/CHANGELOG.md with [{new_version}] - {release_date}")
PY

# 10. Regenerate generated code and operator guide.
echo "==> Regenerating Rust types and Python SDK via cargo xtask codegen"
cargo xtask codegen

echo "==> Regenerating docs/pacto-bot-admin-llms.txt"
cargo xtask docs

# 11. Validate that all embedded versions are consistent.
echo "==> Validating version consistency"
python3 - "$new_daemon_version" "$new_sdk_version" "$new_contract_version" <<'PY'
import json, re, sys, tomllib

daemon, sdk, contract = sys.argv[1:4]
errors = []

# Daemon version in Cargo.toml.
with open("Cargo.toml", "rb") as f:
    cargo = tomllib.load(f)
if cargo["package"]["version"] != daemon:
    errors.append(f"Cargo.toml: version {cargo['package']['version']!r}, expected {daemon!r}")

# Contract version.
with open("schemas/jsonrpc.json", "r") as f:
    rpc = json.load(f)
if rpc["info"]["version"] != contract:
    errors.append(f"schemas/jsonrpc.json: {rpc['info']['version']!r}, expected {contract!r}")

# SDK version.
with open("python/pyproject.toml", "rb") as f:
    pyproject = tomllib.load(f)
if pyproject["project"]["version"] != sdk:
    errors.append(f"python/pyproject.toml: {pyproject['project']['version']!r}, expected {sdk!r}")

with open("python/src/pacto_bot_sdk/__init__.py", "r") as f:
    init = f.read()
init_version = re.search(r'__version__\s*=\s*"([^"]+)"', init)
if not init_version or init_version.group(1) != sdk:
    errors.append(f"python/src/pacto_bot_sdk/__init__.py: __version__ {init_version.group(1) if init_version else 'missing'!r}, expected {sdk!r}")

# Template ranges.
with open("tests/fixtures/templates/python-llm/manifest.toml", "r") as f:
    manifest = f.read()
if f'daemon = {{ range = ">={daemon}' not in manifest:
    errors.append(f"tests/fixtures/templates/python-llm/manifest.toml: missing daemon range for {daemon}")
if f'sdk = {{ name = "pacto-bot-sdk", range = ">={sdk}' not in manifest:
    errors.append(f"tests/fixtures/templates/python-llm/manifest.toml: missing sdk range for {sdk}")
if f'contract = {{ name = "pacto-contract", range = ">={contract}' not in manifest:
    errors.append(f"tests/fixtures/templates/python-llm/manifest.toml: missing contract range for {contract}")

# Template bot dependency.
with open("tests/fixtures/templates/python-llm/bot/pyproject.toml", "r") as f:
    bot_pyproject = f.read()
if f'pacto-bot-sdk>={sdk}' not in bot_pyproject:
    errors.append(f"tests/fixtures/templates/python-llm/bot/pyproject.toml: missing sdk floor {sdk}")

# Examples requirements.
with open("examples/requirements.txt", "r") as f:
    req = f.read()
if f'pacto-bot-sdk>={sdk}' not in req:
    errors.append(f"examples/requirements.txt: missing sdk floor {sdk}")

if errors:
    print("error: version consistency check failed:", file=sys.stderr)
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    sys.exit(1)

print("version consistency check passed")
PY

# 12. Run validation gates.
echo "==> Running make validate"
make validate

echo "==> Running Python SDK tests"
python -m pip install -e python/ >/dev/null 2>&1 || python3 -m pip install -e python/ >/dev/null 2>&1
python -m pytest python/tests/ -q || python3 -m pytest python/tests/ -q

echo "==> Running cargo test --all-targets --all-features"
cargo test --all-targets --all-features

echo ""
echo "Release prep complete for daemon $new_daemon_version, SDK $new_sdk_version, contract $new_contract_version."
echo "Review the changes, then commit and tag:"
echo "  git add -u"
echo "  git commit -m \"chore: release $new_daemon_version\""
echo "  git tag v$new_daemon_version"
echo "  git push && git push origin v$new_daemon_version"
