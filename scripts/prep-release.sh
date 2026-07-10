#!/usr/bin/env bash
set -euo pipefail

# Prepare the repository for a new release.
#
# Usage:
#   ./scripts/prep-release.sh <daemon-version> [--sdk-version <version>] [--contract-version <version>]
#
# Examples:
#   ./scripts/prep-release.sh 0.7.0
#   ./scripts/prep-release.sh 0.7.0 --sdk-version 0.4.0 --contract-version 0.7.0

usage() {
  cat <<'EOF' >&2
Usage: ./scripts/prep-release.sh <daemon-version> [--sdk-version <version>] [--contract-version <version>]

Options:
  --sdk-version       Override the Python SDK version. Defaults to the next
                      minor of the current SDK version.
  --contract-version  Override the JSON-RPC contract artifact version.
                      Defaults to the requested daemon version.

Examples:
  ./scripts/prep-release.sh 0.7.0
  ./scripts/prep-release.sh 0.7.0 --sdk-version 0.4.0 --contract-version 0.7.0
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

release_date="$(date +%Y-%m-%d)"

echo "==> Preparing release $new_daemon_version"

# 1. Bump all version strings.
python3_args=(
  scripts/version_bump.py
  "$new_daemon_version"
  --release-date "$release_date"
)
if [[ -n "$new_sdk_version" ]]; then
  python3_args+=(--sdk-version "$new_sdk_version")
fi
if [[ -n "$new_contract_version" ]]; then
  python3_args+=(--contract-version "$new_contract_version")
fi

python3 "${python3_args[@]}"

# 2. Update Cargo.lock.
echo "==> Updating Cargo.lock"
cargo update -p pacto-bot-api

# 3. Regenerate generated code and operator guide.
echo "==> Regenerating Rust types and Python SDK via cargo xtask codegen"
cargo xtask codegen

echo "==> Regenerating docs/pacto-bot-admin-llms.txt"
cargo xtask docs

# 4. Run validation gates.
echo "==> Running make validate"
make validate

echo "==> Running Python SDK tests"
python -m pip install -e 'python/[dev]' >/dev/null 2>&1 || python3 -m pip install -e 'python/[dev]' >/dev/null 2>&1
python -m pytest python/tests/ -q || python3 -m pytest python/tests/ -q

echo "==> Running cargo test --all-targets --all-features"
cargo test --all-targets --all-features

echo ""
new_sdk_version="${new_sdk_version:-$(python3 -c "import tomllib; print(tomllib.load(open('python/pyproject.toml','rb'))['project']['version'])")}"
new_contract_version="${new_contract_version:-$(python3 -c "import json; print(json.load(open('schemas/jsonrpc.json'))['info']['version'])")}"
echo "Release prep complete for daemon $new_daemon_version, SDK $new_sdk_version, contract $new_contract_version."
echo "Review the changes, then commit and tag:"
echo "  git add -u"
echo "  git commit -m \"chore: release $new_daemon_version\""
echo "  git tag v$new_daemon_version"
echo "  git push && git push origin v$new_daemon_version"
