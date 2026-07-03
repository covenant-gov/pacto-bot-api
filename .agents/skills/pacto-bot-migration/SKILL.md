# pacto-bot-migration

Use this skill when a project was scaffolded before `pacto-bot-admin` started
writing per-bot lock files (`scaffold.lock`) and the user wants to migrate it
to the new lock-tracked structure so `pacto-bot-admin update` works.

## Trigger phrases

- "migrate pre-lock project"
- "scaffold.lock missing"
- "update says scaffold lock file not found"
- "migrate old pacto bot project"
- "pacto-bot-admin update migration"

## When to use this skill

`pacto-bot-admin update` only works for projects that have a per-bot lock file
at `.pacto/bots/<bot-id>/scaffold.lock`. Projects created before the lock file
was introduced are not supported by `update`. This skill walks through the safe
migration path.

## Migration path

### 1. Back up the existing project

Before making changes, back up the project directory:

```bash
cp -r my-project my-project-backup
```

### 2. Scaffold a fresh project with the same bot id

Create a new project in a temporary directory. This gives you the new lock file
and the latest template files:

```bash
pacto-bot-admin new --scaffold my-bot --backend nsec --relays ws://localhost:7000 --commands echo --project-dir /tmp/my-bot-migration
```

Make sure `--language` and `--kind` match the original project if they were
non-default.

### 3. Copy the lock file into the existing project

Copy the generated lock file from the fresh project into the original project,
creating the `.pacto/bots/` directory if it does not exist:

```bash
mkdir -p my-project/.pacto/bots/my-bot
cp /tmp/my-bot-migration/.pacto/bots/my-bot/scaffold.lock my-project/.pacto/bots/my-bot/scaffold.lock
```

### 4. Decide how to handle the generated bot files

`pacto-bot-admin update` will compare the template output against the existing
project. Protected files declared in the template's `manifest.toml` (for
example, the bot handler file) are skipped by default. Non-protected files that
differ from the template are shown with a diff preview before overwriting.

If the user wants to keep all existing bot files and only record the lock file,
no further action is needed.

If the user wants to bring the project in line with the latest template, run
`update` and review each diff:

```bash
cd my-project
pacto-bot-admin update my-bot
```

To overwrite protected files as well, use `--force`:

```bash
pacto-bot-admin update my-bot --force
```

### 5. Verify the project still works

After migration, check that the daemon config still loads and the bot handler
starts:

```bash
pacto-bot-admin validate-config
python bots/my-bot/my_bot.py
```

## Lock file fields

`.pacto/bots/<bot-id>/scaffold.lock` is a TOML file with this structure:

```toml
lock_version = 1

[template]
path = "python-llm"
ref = "v0.1.0"
resolved_commit = "abc123..."

[contract]
name = "pacto-contract"
version = "0.1.0"

[sdk]
name = "pacto-bot-sdk"
version = "0.2.0"

[admin]
version = "0.4.1"
```

- `lock_version`: schema version for future migrations.
- `[template]`: the selected template path, requested git ref, and resolved commit hash.
- `[contract]`: the published contract artifact name and version.
- `[sdk]`: the published PyPI SDK package name and version.
- `[admin]`: the `pacto-bot-admin` version that created the lock.

## Common pitfalls

- Do not hand-edit `scaffold.lock` unless you also understand the template
  repository's `manifest.toml` compatibility ranges.
- `update` requires `cargo-generate` to be installed. Install it with:
  `cargo install cargo-generate --version 0.23.0`.
- `update` on a branch- or commit-pinned template ref will re-render the same
  commit, not advance to a newer version. Semver tags are allowed to float to
  the latest compatible version.
- Keep the lock file in version control; it is the source of truth for updates.

## Anti-patterns

- Do not run `pacto-bot-admin update` on a pre-lock project without first
  creating a lock file. The command will error and reference this skill.
- Do not delete the original project before copying the lock file.
- Do not commit real `nsec` values or daemon secrets.
