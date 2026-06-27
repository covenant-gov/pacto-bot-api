# Getting Started — Pacto Ecosystem Development

A developer guide for spinning up a local environment to work on Pacto and its directly-related apps, libraries, and dependencies.

---

## What this covers

This guide gets you from zero to a working local dev environment for:

- `pacto-app` — the Rust/Tauri desktop client.
- `pacto-gov` — Solidity governance contracts ("Nave Pirata").
- `pacto-squad-sponsor` — gas-sponsorship contract.
- `delegated-security-manager` — Hats-based security module.
- `pacto-aztec` — Noir/TypeScript Aztec privacy layer.
- `nostr-k-derivs` — Nostr-key-to-chain-address derivation.
- `pacto-bot-api` daemon — the standalone JSON-RPC bot runtime.
- Local services required by the above: Nostr relay, EVM testnet, Aztec sandbox, NIP-46 bunker.

---

## 1. Prerequisites

You need Docker, Docker Compose, Git, Rust, Node.js, and Foundry.

### 1.1 Docker and Docker Compose

Docker is required because every local service is containerized. Install Docker Engine or Docker Desktop, then verify:

```bash
docker --version
docker compose version
```

Recommended minimum resources allocated to Docker:

| Service | RAM |
|---------|-----|
| Pacto build + relay | 4 GB |
| Aztec sandbox | 8 GB |
| Everything together | 12–16 GB |

### 1.2 Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version
cargo --version
```

### 1.3 Node.js / pnpm

Pacto uses pnpm. Node 20 is recommended:

```bash
corepack enable
corepack prepare pnpm@latest --activate
pnpm --version
```

### 1.4 Foundry (for EVM contracts)

```bash
curl -L https://foundry.paradigm.xyz | bash
foundryup
anvil --version
forge --version
cast --version
```

### 1.5 System dependencies (Ubuntu/Debian)

Tauri needs native libraries:

```bash
sudo apt update
sudo apt install -y \
  build-essential cmake clang libclang-dev curl wget file git pkg-config \
  libvulkan-dev libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev libasound2-dev
```

### 1.6 macOS / Apple Silicon

Install Xcode Command Line Tools, then [Homebrew](https://brew.sh/):

```bash
xcode-select --install
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
```

Then run the Pacto macOS setup script in this repo, which installs the rest:

```bash
./dev-setup/setup-macos-arm64.sh
```

This installs or updates: Docker, rustup, Node, pnpm, Foundry, `cmake`, `llvm`, `pkg-config`, `openssl@3`, and clones the Covenant Gov repos into `~/src/covenant-gov/`.

If you prefer manual steps:

```bash
brew install docker rustup node@20 pnpm foundry cmake llvm pkg-config openssl@3 git wget
echo 'export PATH="$(brew --prefix llvm)/bin:$PATH"' >> ~/.zshrc
echo 'export LIBCLANG_PATH="$(brew --prefix llvm)/lib"' >> ~/.zshrc
echo 'export PKG_CONFIG_PATH="$(brew --prefix openssl@3)/lib/pkgconfig:$PKG_CONFIG_PATH"' >> ~/.zshrc
source ~/.zshrc
```

### 1.7 Windows

Use WSL2 with the Ubuntu instructions above. Tauri desktop builds on native Windows are supported but slower; WSL2 is the recommended dev path.

---

## 2. Clone the ecosystem

Create a workspace directory and clone the public Covenant Gov repositories:

```bash
mkdir -p ~/src/covenant-gov
cd ~/src/covenant-gov

git clone https://github.com/covenant-gov/pacto-app.git
git clone https://github.com/covenant-gov/pacto-gov.git
git clone https://github.com/covenant-gov/pacto-squad-sponsor.git
git clone https://github.com/covenant-gov/pacto-aztec.git
git clone https://github.com/covenant-gov/nostr-k-derivs.git
git clone https://github.com/covenant-gov/delegated-security-manager.git
git clone https://github.com/covenant-gov/pacto-download.git
```

The `pacto-bot-api` daemon is under active development in the `pacto-app` repo under `src-tauri/pacto-bot-api` (or as a standalone crate — see `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md` for the target layout). Clone whichever branch or repository hosts it:

```bash
# If it lives as a standalone repo (planned)
git clone https://github.com/covenant-gov/pacto-bot-api.git
```

---

## 3. Start the local services

All backing services are run via Docker Compose from the **this repo's** `dev-setup/` directory.

```bash
cd dev-setup
mkdir -p data/relay
docker compose up -d --build
```

This builds native arm64 Docker images (no Rosetta emulation) and starts:

- Nostr relay on `ws://localhost:7000`
- Anvil EVM testnet on `http://localhost:8545`

Add optional profiles as needed:

```bash
# For pacto-aztec work
docker compose --profile aztec up -d --build

# For NIP-46 bunker signing tests
docker compose --profile bunker up -d --build
```

### 3.1 Verify the default stack

```bash
export PATH="$HOME/.foundry/bin:$PATH"
cast block-number --rpc-url http://localhost:8545
curl -s http://localhost:7000 | head -5
```

### 3.2 Optional: generate real bunker secrets

The bunker profile starts with placeholder secrets for local development. For anything beyond your laptop, create `dev-setup/.env`:

```bash
cd dev-setup
cat > .env <<EOF
JWT_SECRET=$(openssl rand -base64 48)
JWT_REFRESH_SECRET=$(openssl rand -base64 48)
ENCRYPTION_KEY=$(openssl rand -base64 48)
EOF
docker compose --profile bunker up -d
```

---

## 4. Build and run `pacto-app`

```bash
cd ~/src/covenant-gov/pacto-app
pnpm install
pnpm run tauri:dev
```

First build downloads and compiles many Rust crates — expect several minutes. The app will connect to whatever relays are configured in its settings; point it at `ws://localhost:7000` for local-only development.

To run just the frontend in a browser:

```bash
pnpm dev
```

### Connecting `pacto-app` to the local EVM chain

1. Start the dev services: `cd dev-setup && docker compose up -d`.
2. In `pacto-app`, open **Settings → Wallet / Network**.
3. Add a custom EVM network:
   - Name: `Pacto Local`
   - RPC URL: `http://localhost:8545`
   - Chain ID: `31337`
   - Currency symbol: `ETH`
4. Import one of the default Anvil private keys for a test account (account #0 key is `0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80`). **Never use this key outside of local development.**

### Common build fixes

| Error | Fix |
|-------|-----|
| `webkit2gtk-4.1` not found | `sudo apt install libwebkit2gtk-4.1-dev` |
| `openssl-sys` build fails | `sudo apt install libssl-dev pkg-config` |
| `bindgen` errors | `sudo apt install clang libclang-dev` |
| Vulkan errors on Linux | `sudo apt install libvulkan-dev` |
| macOS `cc` / linker not found | `xcode-select --install` |
| macOS OpenSSL errors | `brew install openssl@3 pkg-config` and export `PKG_CONFIG_PATH` |

---

## 5. Work on Solidity contracts

```bash
cd ~/src/covenant-gov/pacto-gov
forge install
forge build
forge test
```

To deploy against the local Anvil node:

```bash
forge script script/Deploy.s.sol --rpc-url http://localhost:8545 --broadcast --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
```

The default Anvil private key above is account #0. Use the same RPC and key for `pacto-squad-sponsor` and `delegated-security-manager`.

### Make `pacto-app` use the freshly deployed contracts

After running the deploy script, note the printed contract addresses. Then:

1. Find the contract-address config file in `pacto-app` (often under `src-tauri/src/evm/contracts/` or an environment file like `.env.local`).
2. Update the fields for `PactoGov`, `SquadSponsor`, or `DelegatedSecurityManager` to the addresses from the deploy output.
3. Restart `pnpm run tauri:dev` if the values are read only at Tauri startup.

If the app does not expose a config file, search the Rust source for the current contract address constants and replace them temporarily for local testing — but **do not commit hardcoded local addresses**.

---

## 6. Work on `pacto-aztec`

Make sure the Aztec profile is running:

```bash
cd dev-setup
docker compose --profile aztec up -d --build
```

Then follow the Aztec project's own README:

```bash
cd ~/src/covenant-gov/pacto-aztec
pnpm install
pnpm compile   # compiles Noir contracts
pnpm test      # runs tests against the sandbox
```

The Aztec RPC is at `http://localhost:8080`.

---

## 7. Work on `nostr-k-derivs`

```bash
cd ~/src/covenant-gov/nostr-k-derivs
cargo build
cargo test
```

---

## 8. Work on `pacto-bot-api`

The easiest path for bot development is to start with the default Docker stack and use the daemon's local `nsec` signing backend. This avoids setting up a NIP-46 bunker while you are iterating on bot logic.

### Easiest start (nsec backend, no bunker)

1. Start the default services:

   ```bash
   cd dev-setup
   docker compose up -d --build
   ```

   This gives you:
   - Nostr relay on `ws://localhost:7000`
   - Anvil EVM on `http://localhost:8545`

2. Build the daemon:

   ```bash
   cd ~/src/covenant-gov/pacto-app/src-tauri/pacto-bot-api
   cargo build
   ```

   Or, if it is a standalone repo:

   ```bash
   cd ~/src/covenant-gov/pacto-bot-api
   cargo build
   ```

3. Create a test bot identity:

   ```bash
   cargo run --bin pacto-bot-admin -- new echo-bot --backend nsec
   ```

   Save the printed `[[bots]]` snippet to `pacto-bot-api.toml`:

   ```toml
   [daemon]
   data_dir = "~/.local/share/pacto-bot-api"
   socket_path = "~/.local/share/pacto-bot-api/pacto-bot-api.sock"

   [[bots]]
   id = "echo-bot"
   npub = "npub1..."
   signing = { backend = "nsec", nsec = "${PACT_BOT_NSEC}" }
   relays = ["ws://localhost:7000"]
   capabilities = ["ReadMessages", "SendMessages"]
   ```

4. Export the key and run the daemon:

   ```bash
   export PACT_BOT_NSEC="nsec1..."
   cargo run --bin pacto-bot-api -- --config pacto-bot-api.toml
   ```

5. In another terminal, run the example handler:

   ```bash
   cd examples
   python echo_bot.py
   ```

6. Send a DM to the bot's `npub` from any Nostr client pointed at `ws://localhost:7000` and the handler will echo it back.

### Required Docker services

| What you are testing | Docker command |
| -------------------- | -------------- |
| Basic bot logic with `nsec` signing | `docker compose up -d --build` |
| Bot that reads/writes EVM state | `docker compose up -d --build` (Anvil is already included) |
| Bot with a NIP-46 bunker | `docker compose --profile bunker up -d --build` |
| Bot that uses Aztec contracts | `docker compose --profile aztec up -d --build` |

> The `bunker` and `aztec` profiles can be combined:
> `docker compose --profile bunker --profile aztec up -d --build`

### 8.1 Build the daemon (with tests)

If the daemon exists in `pacto-app/src-tauri/pacto-bot-api`:

```bash
cd ~/src/covenant-gov/pacto-app/src-tauri/pacto-bot-api
cargo build
cargo test
```

If it is a standalone repo:

```bash
cd ~/src/covenant-gov/pacto-bot-api
cargo build
cargo test
```

### 8.2 Test the bot with a NIP-46 bunker (advanced)

Enable the bunker profile:

```bash
cd dev-setup
docker compose --profile bunker up -d --build
```

Then point the bot at the local bunker instead of an `nsec` key. The bunker base URL is `http://127.0.0.1:3001`. The exact config key depends on the daemon's TOML format; a typical bunker-backed bot looks like:

```toml
[[bots]]
id = "bunker-bot"
npub = "npub1..."
signing = { backend = "bunker_local", uri = "http://127.0.0.1:3001" }
relays = ["ws://localhost:7000"]
capabilities = ["ReadMessages", "SendMessages"]
```

For production bunkers, use `wss://` URIs and set a real `ENCRYPTION_KEY` in `dev-setup/.env`.

---

## 9. Quick-reference: ports and endpoints

| Service | URL | Purpose |
|---------|-----|---------|
| Nostr relay | `ws://localhost:7000` | Encrypted DM and MLS group messaging |
| Anvil EVM | `http://localhost:8545` | Local Ethereum testnet (chain 31337) |
| Aztec sandbox | `http://localhost:8080` | ZK privacy chain |
| Aztec admin | `http://localhost:8880` | Aztec admin API |
| NIP-46 bunker | `http://127.0.0.1:3001` | Remote signer for bot tests |
| pacto-bot-api Unix socket | `~/.local/share/pacto-bot-api/pacto-bot-api.sock` | JSON-RPC bot API |
| pacto-bot-api HTTP (opt-in) | `http://127.0.0.1:9800` | JSON-RPC bot API (needs `--enable-http`) |

---

## 10. Recommended workflow

1. Start Docker services: `cd dev-setup && docker compose up -d --build`.
2. If working on Aztec, add the profile: `docker compose --profile aztec up -d --build`.
3. In one terminal, run `pacto-app` for UI development: `cd pacto-app && pnpm run tauri:dev`.
4. In another terminal, deploy the governance contracts to Anvil and copy addresses into the app's network config.
5. If working on bots, start `pacto-bot-api` with a test config pointing at the local relay; add the bunker profile if you need NIP-46 signing tests.
6. Iterate. Re-run `cargo test`, `forge test`, or `pnpm test` as appropriate.

---

## 11. Troubleshooting

### Docker containers fail to start

- Confirm Docker has enough RAM (12+ GB when running Aztec).
- Check logs: `cd dev-setup && docker compose logs -f`.

### `pacto-app` cannot connect to the local relay

- Verify the relay is listening: `curl http://localhost:7000` should return a landing page or relay info.
- In Pacto settings, add `ws://localhost:7000` as a relay.

### Bot daemon says "Unknown bot_id" or "Unauthorized"

- Check that the `npub` in `pacto-bot-api.toml` matches the signer backend.
- For `nsec` backend, confirm `PACT_BOT_NSEC` is exported in the same shell.
- For bunker backends, verify the bunker URI and that the bunker's pubkey matches the configured `npub`.

### Foundry/Anvil deployment fails

- Confirm the Anvil container is running and RPC responds:
  `cast block-number --rpc-url http://localhost:8545`.
- Use the default Anvil private key for local deployments; never commit real keys.

### Aztec sandbox is slow or OOMs

- Increase Docker memory limit to at least 8 GB, preferably 12 GB.
- Stop other containers you are not actively using.

---

## 12. Security notes for local development

- All local services bind to `localhost` only by default. Do not expose Anvil, the relay, or the bot API HTTP port to the public internet.
- The `nsec` signing backend logs a warning and is for local testing only. Use a NIP-46 bunker for any shared or production deployment.
- Never commit private keys, bunker URIs, or `PACT_BOT_NSEC` values to Git.

---

## Sources

- Pacto ecosystem overview: `pacto_ecosystem_research.md`
- Bot daemon plan: `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-plan.md`
- Bot daemon executive summary: `docs/plans/2026-06-24-001-feat-pacto-bot-api-daemon-executive-summary.md`
- Pacto architecture: `pacto-bot-architecture-deep-dive.md`
- Upstream Pacto README: https://github.com/covenant-gov/pacto-app/blob/main/README.md
- Upstream build guide: https://github.com/covenant-gov/pacto-app/blob/main/docs/build/ubuntuGuide.md
- Upstream macOS guide: https://github.com/covenant-gov/pacto-app/blob/main/docs/build/macGuide.md
