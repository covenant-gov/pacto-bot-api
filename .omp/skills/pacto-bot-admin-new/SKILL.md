# pacto-bot-admin-new

Use this skill when the user asks to validate, test, or exercise the
`pacto-bot-admin new` (and `pacto-bot-admin scaffold`) command end-to-end. The
skill walks through building the debug admin CLI, generating a fully scaffolded
Python bot project, running the Docker Compose stack, running the generated
pytest suite, and reporting what works and what should be improved.

This skill is **not** for generic bot authoring — use `python-pacto-bot` for
that. It is specifically for smoke-testing the scaffold generator and the
developer experience of a freshly created bot project.

## Trigger phrases

- "test the pacto-bot-admin new command"
- "validate the bot scaffold"
- "run through the new bot workflow"
- "create and test a scaffolded bot project"
- "smoke test pacto-bot-admin new"
- "does the bot scaffold actually work"

## Disambiguation

| User says | Load this skill? |
|---|---|
| "test pacto-bot-admin new" | Yes |
| "validate the scaffolded bot project" | Yes |
| "run the bot example" | No — use `python-pacto-bot` |
| "fix the generated bot project" | No — load `python-pacto-bot` and fix directly |
| "review the scaffold code" | No — use a reviewer agent |

## Canonical references

Read these first before running validation:

- `src/admin.rs` — implementation of `new`, `scaffold`, and `update`.
- `src/scaffold/` — template resolution, rendering, and merge logic.
- `tests/fixtures/templates/python-llm/` — local fixture template used by the
  test suite (useful when the upstream template repo is unreachable).
- `python/README.md` — how the generated SDK is installed and used.
- `examples/test_examples_contract.py` — the contract harness new examples
  must pass.

## Validation workflow

Run the following from the repository root. Use the freshly built debug binary
so the capability defaults and template fixes under test are the ones being
validated.

### 1. Build the debug admin CLI

```bash
cargo build --bin pacto-bot-admin
```

Confirm the binary exists at `target/debug/pacto-bot-admin`.

### 2. Generate a scaffolded bot project

Use the real upstream template repository by default:

```bash
rm -rf /tmp/pacto-bot-smoke
target/debug/pacto-bot-admin new --scaffold smoke-bot \
  --backend nsec \
  --relays ws://localhost:7000 \
  --commands hello,help \
  --project-dir /tmp/pacto-bot-smoke
```

With the current fixes, the generated `pacto-bot-api.toml` should contain
`capabilities = ["ReadMessages", "SendMessages"]` even though `--capabilities`
was omitted.

If the upstream template repository is unreachable, fall back to the local
fixture template used by the test suite:

```bash
rm -rf /tmp/pacto-bot-smoke
target/debug/pacto-bot-admin new --scaffold smoke-bot \
  --backend nsec \
  --relays ws://localhost:7000 \
  --commands hello,help \
  --project-dir /tmp/pacto-bot-smoke \
  --template-repo tests/fixtures/templates
```

Inspect the generated project layout:

```bash
find /tmp/pacto-bot-smoke -type f | sort
cat /tmp/pacto-bot-smoke/pacto-bot-api.toml
cat /tmp/pacto-bot-smoke/bots/smoke-bot/smoke_bot.py
```

**What to verify:**

- `pacto-bot-api.toml` is created with mode `0o600`.
- The `[[bots]]` snippet contains an `npub`, `nsec` backend, relay, and
  `capabilities` list.
- The bot handler file is generated at `bots/smoke-bot/smoke_bot.py` and uses
  `from pacto_bot_sdk import Bot, parse_command`.
- The generated `tests/` directory should contain `test_handlers.py` (not
  `test_bot.py`) and the contract test file should import the handler from the
  bot module without conflicts.

### 3. Run the Docker Compose stack

```bash
cd /tmp/pacto-bot-smoke
docker compose up --dry-run
```

If the previous command succeeds, attempt to bring the stack up:

```bash
docker compose up --build
```

**What to verify:**

- Whether the daemon image (`ghcr.io/covenant-gov/pacto-bot-api:latest`) can be
  pulled or if it returns an authorization error.
- Whether the bot image builds successfully from `bots/smoke-bot/Dockerfile`.
- Whether the daemon and bot services start and the bot registers.

### 4. Run the generated pytest suite

```bash
cd /tmp/pacto-bot-smoke/bots/smoke-bot
python -m pip install -e .
python -m pytest tests/ -v
```

If `pip install -e .` fails because `pacto-bot-sdk` is not on PyPI, install the
local SDK from the repository first:

```bash
python -m pip install -e /path/to/pacto-bot-api/python
```

With the current fixes, `pytest tests/` should collect and pass without needing
to rename any files or set `PYTHONPATH`.

**What to verify:**

- Whether the tests collect without import errors.
- Whether all generated tests pass.
- Whether the contract test (`test_contract.py`) validates the declared
  command contracts.

### 5. Run the bot against a local daemon (optional but valuable)

Start the daemon with the generated config:

```bash
mkdir -p /tmp/pacto-smoke-data
chmod 700 /tmp/pacto-smoke-data
target/debug/pacto-bot-api \
  --config /tmp/pacto-bot-smoke/pacto-bot-api.toml \
  --data-dir /tmp/pacto-smoke-data \
  --enable-http
```

In another terminal, run the bot:

```bash
cd /tmp/pacto-bot-smoke/bots/smoke-bot
python smoke_bot.py --socket ~/.local/share/pacto-bot-api/pacto-bot-api.sock
```

**What to verify:**

- The bot connects and registers successfully (look for
  `registered handler_id=...`).
- If registration fails, note the exact error (e.g., missing capability).

## Known issues and recommendations

Report these findings as observations, not fixes. The goal is to document the
gap between the current scaffold and a "works out of the box" experience.

### 1. ✅ Default capabilities (fixed)

`pacto-bot-admin new --scaffold` now defaults to
`["ReadMessages", "SendMessages"]` when `--capabilities` is omitted. Verify that
the generated `pacto-bot-api.toml` includes both capabilities.

### 2. ✅ Generated test file naming (fixed)

The template now renders `tests/test_handlers.py` instead of `tests/test_bot.py`,
avoiding a module-name collision with the bot module. Verify that `pytest
tests/` collects and passes without renaming files.

### 3. `pacto-bot-sdk` is not available on PyPI

The generated `pyproject.toml` depends on `pacto-bot-sdk>=0.2.0`. On a fresh
machine `pip install -e .` fails because the package is not published (or not
findable).

**Recommendation:** Either publish `pacto-bot-sdk` to PyPI, or make the scaffold
vendor a local `python/` directory with an editable dependency and document the
extra install step. Update the generated `README.md` and `AGENTS.md` to stop
claiming the PyPI package installs automatically.

### 4. Docker Compose image is not publicly accessible

The generated `docker-compose.yml` uses
`ghcr.io/covenant-gov/pacto-bot-api:latest`. Anonymous pulls currently return
`401 Unauthorized`, so `docker compose up` fails before the bot can be tested.

**Recommendation:** Provide a public image, or change the generated compose to
build the daemon image locally with a `dockerfile: ../Dockerfile` context and
document the build step.

### 5. The `--commands` argument is positional-comma only

Command names can be passed as `--commands hello,help` or repeated
`--commands hello --commands help`. The interactive prompt does not show
example usage for repeated flags.

**Recommendation:** Accept the more natural `--commands hello help` (space
separated) or improve the help text/after-help examples.

### 6. Capability comma-separated parsing is surprising

`--capabilities ReadMessages,SendMessages` is parsed as a single capability
string and rejected. Users must repeat the flag.

**Recommendation:** Support comma-separated capabilities in a single flag and
update the help text to show this.

## What to report

At the end of the validation, summarize:

1. The exact command used to generate the project.
2. Whether the generated project files are complete and coherent.
3. Whether `docker compose up` succeeded or failed, with the error.
4. Whether `python -m pytest tests/` passed out of the box, and what manual
   steps were required to make it pass (if any).
5. Whether the bot registered against a local daemon.
6. A prioritized list of changes that would make the scaffold "just work" for a
   new developer.

Do not fix the scaffold generator or the template as part of this skill. Only
report findings and recommendations.
