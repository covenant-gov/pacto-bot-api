#!/usr/bin/env bash
set -euo pipefail

# Package cross-compiled release binaries into friendly archives with SHA-256
# checksums.
#
# Usage:
#   ./scripts/package-release.sh [version] [output-dir]
#
# Defaults:
#   version  -> parsed from Cargo.toml
#   out-dir  -> dist

version="${1:-$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2)}"
outdir="${2:-dist}"

mkdir -p "$outdir"

checksum() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" > "$1.sha256"
  else
    shasum -a 256 "$1" > "$1.sha256"
  fi
}

asset_for_target() {
  case "$1" in
    x86_64-apple-darwin) echo darwin_amd64 ;;
    aarch64-apple-darwin) echo darwin_arm64 ;;
    x86_64-unknown-linux-musl) echo linux_amd64 ;;
    aarch64-unknown-linux-musl) echo linux_arm64 ;;
    x86_64-pc-windows-gnu) echo windows_amd64 ;;
    x86_64-unknown-freebsd) echo freebsd_amd64 ;;
  esac
}

targets=(
  x86_64-apple-darwin
  aarch64-apple-darwin
  x86_64-unknown-linux-musl
  aarch64-unknown-linux-musl
  x86_64-pc-windows-gnu
  x86_64-unknown-freebsd
)

for target in "${targets[@]}"; do
  asset="$(asset_for_target "$target")"
  name="pacto-bot-api_${version}_$asset"
  dir="target/$target/release"

  if [[ ! -d "$dir" ]]; then
    echo "warning: $dir not found, skipping $target"
    continue
  fi

  binaries=()
  for bin in pacto-bot-api pacto-bot-admin create-mls-group; do
    if [[ -f "$dir/$bin" ]]; then
      binaries+=("$bin")
    elif [[ -f "$dir/$bin.exe" ]]; then
      binaries+=("$bin.exe")
    fi
  done

  if [[ ${#binaries[@]} -eq 0 ]]; then
    echo "warning: no binaries found in $dir, skipping $target"
    continue
  fi

  if [[ "$asset" == windows* ]]; then
    (cd "$dir" && zip -r "../../../$outdir/${name}.zip" "${binaries[@]}")
    checksum "$outdir/${name}.zip"
  else
    tar -czf "$outdir/${name}.tar.gz" -C "$dir" "${binaries[@]}"
    checksum "$outdir/${name}.tar.gz"
  fi
  echo "packaged $outdir/${name}.*"
done
