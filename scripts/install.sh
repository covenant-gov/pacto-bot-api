#!/usr/bin/env bash
#
# Install the latest (or a specific) pacto-bot-api release from GitHub.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/logicminds/pacto-bot-api/main/scripts/install.sh | bash
#
# Environment variables:
#   INSTALL_PREFIX   installation prefix (default: /usr/local; binaries go to $INSTALL_PREFIX/bin)
#   PACTO_VERSION    release to install, e.g. "0.1.0" or "v0.1.0" (default: latest)
#   PACTO_REPO       owner/repo to download from (default: logicminds/pacto-bot-api)
#   GITHUB_TOKEN     optional GitHub token to raise API rate limits
#
set -euo pipefail

REPO="${PACTO_REPO:-logicminds/pacto-bot-api}"
INSTALL_PREFIX="${INSTALL_PREFIX:-/usr/local}"
BIN_DIR="${INSTALL_PREFIX}/bin"
REQUESTED_VERSION="${PACTO_VERSION:-latest}"

say() {
  printf '%s\n' "$*"
}

err() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

command_exists() {
  command -v "$1" >/dev/null 2>&1
}

detect_platform() {
  local os arch
  os=$(uname -s)
  arch=$(uname -m)

  case "$os" in
    Linux)   os=linux ;;
    Darwin)  os=darwin ;;
    FreeBSD) os=freebsd ;;
    *) err "unsupported operating system: $os (expected Linux, Darwin, or FreeBSD)" ;;
  esac

  case "$arch" in
    x86_64|amd64) arch=amd64 ;;
    arm64|aarch64) arch=arm64 ;;
    *) err "unsupported architecture: $arch (expected x86_64/amd64 or arm64/aarch64)" ;;
  esac

  printf '%s_%s\n' "$os" "$arch"
}

resolve_version() {
  local tag version

  if [ "$REQUESTED_VERSION" = "latest" ]; then
    local api_url="https://api.github.com/repos/${REPO}/releases/latest"
    local curl_opts=(-fsSL)
    if [ -n "${GITHUB_TOKEN:-}" ]; then
      curl_opts+=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
    fi

    local json
    json=$(curl "${curl_opts[@]}" "$api_url") || \
      err "failed to fetch latest release from GitHub API (rate limited? try GITHUB_TOKEN)"

    tag=$(printf '%s\n' "$json" | grep -o '"tag_name": "[^"]*' | head -1 | cut -d'"' -f4)
    [ -n "$tag" ] || err "could not parse latest release tag from GitHub API response"
  else
    tag="$REQUESTED_VERSION"
    # Normalize a bare version to a "v" tag.
    case "$tag" in
      v*) : ;;
      *) tag="v${tag}" ;;
    esac
  fi

  version="${tag#v}"
  printf '%s\n' "$tag"
}

download() {
  local url="$1" out="$2"
  local curl_opts=(-fsSL --progress-bar)
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl_opts+=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
  fi
  curl "${curl_opts[@]}" "$url" -o "$out"
}

verify_checksum() {
  local archive_path="$1"
  local checksum_file="$2"

  local expected actual
  expected=$(awk '{print $1}' "$checksum_file")
  [ -n "$expected" ] || err "could not read expected checksum from ${checksum_file}"

  if command_exists sha256sum; then
    actual=$(sha256sum "$archive_path" | awk '{print $1}')
  elif command_exists shasum; then
    actual=$(shasum -a 256 "$archive_path" | awk '{print $1}')
  else
    say "warning: neither sha256sum nor shasum found; skipping checksum verification"
    return 0
  fi

  if [ "$expected" != "$actual" ]; then
    return 1
  fi
}

ensure_bin_dir() {
  if [ -d "$BIN_DIR" ]; then
    return 0
  fi

  if mkdir -p "$BIN_DIR" 2>/dev/null; then
    return 0
  fi

  if command_exists sudo; then
    sudo mkdir -p "$BIN_DIR"
    sudo chmod 755 "$BIN_DIR"
  else
    err "cannot create $BIN_DIR and sudo is not available"
  fi
}

install_binary() {
  local src="$1" dst="$2"

  if [ -w "$BIN_DIR" ]; then
    install -m 755 "$src" "$dst"
  elif command_exists sudo; then
    sudo install -m 755 "$src" "$dst"
  else
    err "cannot write to $BIN_DIR and sudo is not available"
  fi
}

main() {
  if ! command_exists curl; then
    err "curl is required but not installed"
  fi
  if ! command_exists tar; then
    err "tar is required but not installed"
  fi

  local suffix
  suffix=$(detect_platform)
  say "Detected platform: $suffix"

  local tag version
  tag=$(resolve_version)
  version="${tag#v}"
  say "Installing pacto-bot-api ${tag} for ${suffix}"

  local asset="pacto-bot-api_${version}_${suffix}.tar.gz"
  local base_url="https://github.com/${REPO}/releases/download/${tag}"
  local archive_url="${base_url}/${asset}"
  local checksum_url="${archive_url}.sha256"

  local tmpdir
  tmpdir=$(mktemp -d)
  trap 'rm -rf "$tmpdir"' EXIT

  local archive_path="${tmpdir}/${asset}"
  local checksum_path="${tmpdir}/${asset}.sha256"

  say "Downloading ${asset} ..."
  download "$archive_url" "$archive_path" || err "failed to download ${archive_url}"

  say "Downloading checksum ..."
  if download "$checksum_url" "$checksum_path" 2>/dev/null; then
    say "Verifying checksum ..."
    verify_checksum "$archive_path" "$checksum_path" || err "checksum verification failed"
  else
    say "warning: no checksum file found; skipping verification"
  fi

  say "Extracting archive ..."
  tar -xzf "$archive_path" -C "$tmpdir"

  ensure_bin_dir

  local bin binaries=(pacto-bot-api pacto-bot-admin)
  for bin in "${binaries[@]}"; do
    local src="${tmpdir}/${bin}"
    if [ -f "$src" ]; then
      install_binary "$src" "${BIN_DIR}/${bin}"
      say "Installed ${BIN_DIR}/${bin}"
    else
      say "warning: ${bin} not found in archive"
    fi
  done

  if command_exists "${BIN_DIR}/pacto-bot-admin"; then
    say ""
    "${BIN_DIR}/pacto-bot-admin" --version || true
  fi

  rm -rf "$tmpdir"
  trap - EXIT
}

main "$@"
