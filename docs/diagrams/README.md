# Pacto-bot-api Diagrams

High-level, pirate-themed diagrams that explain `pacto-bot-api` to non-developers.
Each diagram is available in three forms:

- **`.excalidraw`** — editable source. Open in [Excalidraw](https://excalidraw.com) (web or desktop) to change shapes, colors, or metaphors.
- **`.png`** — rendered bitmap for embedding in READMEs, markdown docs, or onboarding guides.
- **`.svg`** — vector rendering for web pages or presentations.

> **For non-developers and young audiences:** `pacto-bot-api-big-picture-cartoon` is a kid-friendly cartoon version of the big picture, with actual pirate ships, a treasure chest, and a palm-tree island. No technical jargon.

## The Diagrams

| File | Concept | Pirate Metaphor |
|---|---|---|
| `pacto-bot-api-big-picture` | The daemon, handlers, Nostr relays, and NIP-46 bunker as a fleet | One flagship carries the heavy gear; small ships dock with it |
| `pacto-bot-api-bot-identity` | How a bot identity is created and registered | A captain registers a ship; the flag is public, the key stays in a locked chest/vault |
| `pacto-bot-api-handler-to-daemon` | How a handler connects and calls the daemon | Ships send messages in bottles; the flagship checks permits |
| `pacto-bot-api-message-flow` | End-to-end DM receive and reply | A castaway's bottle reaches the right ship via relay and flagship |
| `pacto-bot-api-security-model` | Who holds keys, secrets, and permissions | The flagship never digs up the treasure; it asks the vault and checks boarding passwords |

## How to use

To replace a PNG with a hand-edited version:

1. Open the `.excalidraw` file in Excalidraw.
2. Edit the drawing.
3. Export as PNG (or SVG) back to `docs/diagrams/`.
4. Update references in `docs/GETTING_STARTED.md` or `README.md` if the filename changes.

## Updating all renderings from the `.excalidraw` source

The `.png` and `.svg` files in this directory are generated from the `.excalidraw` files.
To regenerate them after editing an `.excalidraw` file, re-run the renderer in the repository tooling.
