# Building pacto-bot-api

## Native build

Requires Rust 1.96 or later.

```bash
cargo build
cargo build --release
```

Release binaries are written to:

- `target/release/pacto-bot-api` — the daemon
- `target/release/pacto-bot-admin` — lifecycle/admin CLI

For the full development workflow (tests, linting, coverage), see [DEVELOPMENT.md](DEVELOPMENT.md).

## Cross-compilation

We use [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) so Zig acts as the C compiler and linker. This handles the C code in `rusqlite` (bundled SQLite) and the cryptography crates inside `nostr-sdk` without maintaining per-target sysroots.

### Supported targets

| Platform | Architecture | Rust target |
|---|---|---|
| macOS | x86_64 | `x86_64-apple-darwin` |
| macOS | Apple Silicon (arm64) | `aarch64-apple-darwin` |
| Linux | x86_64 | `x86_64-unknown-linux-musl` |
| Linux | arm64 | `aarch64-unknown-linux-musl` |
| Windows | x86_64 | `x86_64-pc-windows-gnu` |
| FreeBSD | x86_64 | `x86_64-unknown-freebsd` |

### Install tooling

macOS:

```bash
brew install zig cargo-zigbuild
```

Linux:

```bash
# Install Zig from https://ziglang.org/download/
cargo install cargo-zigbuild
```

Install the Rust targets:

```bash
make cross-setup
```

### Build everything

```bash
make cross-compile
```

### Package release artifacts

```bash
make package
```

This creates `dist/` with archives named `pacto-bot-api_<version>_<os>_<arch>.<ext>` plus `.sha256` checksums, e.g.

- `pacto-bot-api_0.1.0_darwin_amd64.tar.gz`
- `pacto-bot-api_0.1.0_darwin_amd64.tar.gz.sha256`
- `pacto-bot-api_0.1.0_windows_amd64.zip`
- `pacto-bot-api_0.1.0_windows_amd64.zip.sha256`

Binaries are written to `target/<triple>/release/`.

### Build per platform

```bash
make cross-compile-macos     # x86_64 + arm64
make cross-compile-linux     # x86_64 + arm64 static musl
make cross-compile-windows   # x86_64
make cross-compile-freebsd   # x86_64
```

### Direct `cargo-zigbuild` commands

```bash
cargo zigbuild --release --target x86_64-apple-darwin
cargo zigbuild --release --target aarch64-apple-darwin
cargo zigbuild --release --target x86_64-unknown-linux-musl
cargo zigbuild --release --target aarch64-unknown-linux-musl
cargo zigbuild --release --target x86_64-pc-windows-gnu
cargo zigbuild --release --target x86_64-unknown-freebsd
```

## Important notes

- **Why do Linux and FreeBSD targets contain `unknown`?** Rust target triples follow the form `<arch>-<vendor>-<os>-<env>`. `unknown` is the vendor field for generic/community targets without a specific hardware vendor (e.g., generic Linux or FreeBSD). It is part of Rust’s canonical target name and cannot be changed. The release archives use friendly names like `linux_amd64` and `freebsd_amd64` so the vendor component never appears in shipped artifacts.
- **Build macOS binaries on macOS.** Apple’s SDK license prevents legally cross-compiling to `*-apple-darwin` from Linux/Windows without Apple’s SDK. The release workflow therefore runs on `macos-latest`.
- **Linux targets use musl** for portable, mostly-static binaries that run on any Linux distro without worrying about glibc versions. If you need glibc, use `x86_64-unknown-linux-gnu` or `aarch64-unknown-linux-gnu` (and optionally pin a minimum glibc version, e.g. `x86_64-unknown-linux-gnu.2.31`).
- **Windows uses the GNU ABI.** `x86_64-pc-windows-gnu` is used because it can be cross-linked with Zig. MSVC targets are not supported here.
- **Run cross-compiled binaries on the target platform.** `cargo run --target …` from the host will not work unless you are using an emulator.
- **Windows is HTTP-only.** The Unix socket transport is unavailable on Windows, so the daemon must be started with `--enable-http`. Handlers then connect to `http://127.0.0.1:9800` (or whatever `http_bind` is configured to).

## Automated releases

The `.github/workflows/release.yml` workflow builds all supported targets when a `v*` tag is pushed and creates a GitHub release with packaged artifacts.

Trigger a release:

```bash
git tag -a v0.1.0 -m "Release v0.1.0"
git push origin v0.1.0
```

You can also run the workflow manually from the GitHub Actions UI to produce artifacts without creating a release.

## Troubleshooting

### `linker not found`

Ensure `zig` is in `PATH` and that you are invoking `cargo zigbuild`, not `cargo build --target …`.

### `rust-lld: error: ...` on the Windows target

Make sure you installed the `x86_64-pc-windows-gnu` target:

```bash
rustup target add x86_64-pc-windows-gnu
```

### macOS universal2 build fails

Install both macOS targets first:

```bash
rustup target add x86_64-apple-darwin aarch64-apple-darwin
```

### Fallback: `cross`

If `cargo-zigbuild` fails for a specific target, try [`cross`](https://github.com/cross-rs/cross), which builds inside a Docker container with the target sysroot:

```bash
cargo install cross
cross build --release --target aarch64-unknown-linux-gnu
```

It is slower and requires Docker, but it can work around linker/C-library edge cases.
