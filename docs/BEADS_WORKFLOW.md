# Beads Workflow — Pacto Development Issue Tracking

This project uses [Beads](https://github.com/steveyegge/beads) for issue tracking. Issues are stored in a Dolt database that is synced to Dolthub, so every developer works from the same task list without relying on a web UI.

> **Note:** The legacy `~/.beads-planning` additional repo has been merged into the primary `pacto-bot-api` database. New setups only need the primary repo.

---

## 1. Install the Beads CLI

```bash
curl -sSL https://raw.githubusercontent.com/steveyegge/beads/main/scripts/install.sh | bash
```

Verify:

```bash
bd version
```

Beads bundles Dolt in embedded mode, so no separate Dolt installation is required for normal use.

---

## 2. First-time setup after cloning

```bash
cd pacto-bot-api
bd bootstrap
```

`.beads/config.yaml` already contains `sync.remote`, so `bd bootstrap` will clone the issue database from Dolthub:

```text
Bootstrap plan: clone from remote
  Remote: https://doltremoteapi.dolthub.com/opselite/pacto-bot-api
  Database: pacto_bot_api
```

Verify the data is present:

```bash
bd status
bd list
```

You should see ~510 total issues. If you see 0, see the troubleshooting section below.

---

## 3. Set your role

Beads routes new issues based on your role. Set it once per clone:

```bash
# If you are a maintainer
git config beads.role maintainer

# If you are a contributor
git config beads.role contributor
```

For contributors, confirm issues are routed to the primary repo (not the old planning repo):

```bash
bd config get routing.contributor
```

Expected output:

```text
.
```

If it shows `~/.beads-planning`, override it:

```bash
bd config set routing.contributor "."
```

---

## 4. Daily workflow

Before starting work:

```bash
bd dolt pull
```

Find work to pick up:

```bash
bd ready          # issues that are open and unblocked
bd list           # all issues
bd show <id>      # issue details
```

Create or update issues:

```bash
bd create "P1: fix relay reconnect race"
bd update <id> --claim
bd note <id> "Reproduced with mock relay"
bd close <id>
```

After making changes:

```bash
bd dolt push
```

Because `dolt.auto-commit` is on, each `bd` command is committed automatically. You only need to pull and push.

---

## 5. Multi-machine sync test

To verify two machines stay in sync:

1. On machine A, create a test issue:

   ```bash
   bd create "SYNC-TEST: verify A to B propagation"
   bd dolt push
   ```

2. On machine B, pull and verify:

   ```bash
   bd dolt pull
   bd list | grep SYNC-TEST
   ```

3. On machine B, update the issue:

   ```bash
   bd note <id> "Updated from machine B"
   bd dolt push
   ```

4. On machine A, pull and verify:

   ```bash
   bd dolt pull
   bd show <id>
   ```

5. Close the test issue on either machine and push/pull to confirm the close propagates.

---

## 6. Config reference

Key values in `.beads/config.yaml`:

| Key | Purpose |
|-----|---------|
| `repos.primary` | The main repo that holds the issue database. |
| `sync.remote` | Dolt remote URL on Dolthub. `bd bootstrap` clones from here. |
| `federation.remote` | Peer-to-peer federation remote (currently same Dolthub URL). |
| `routing.contributor` | Where contributor-created issues are written. Must be `.` for the primary repo. |
| `export.auto` | Whether to auto-export issues to `.beads/issues.jsonl`. Disabled by default. |

Do not hand-edit `src/*_generated.rs` or the Dolt data files under `.beads/embeddeddolt/`. Update `.beads/config.yaml` and commit it through git.

---

## 7. Troubleshooting

### `bd bootstrap` says "no beads database found"

Confirm `.beads/config.yaml` exists and contains `sync.remote`. If it does, run:

```bash
bd bootstrap
```

If it does not, add:

```yaml
sync.remote: "https://doltremoteapi.dolthub.com/opselite/pacto-bot-api"
```

### `bd status` shows 0 issues after `bd dolt pull`

Check that you are in the right repo and that the remote is correct:

```bash
pwd
bd where
bd context
bd dolt remote list
bd dolt show
```

If `bd where` resolves to the wrong directory, set `BEADS_DIR` or run `bd` from the repo root.

### New issues disappear or go to `~/.beads-planning`

You are likely in `contributor` role with the old routing. Fix it:

```bash
bd config set routing.contributor "."
```

Then move any misplaced issues if needed.

### `bd dolt push` fails

Check your network connection and Dolthub access. If you have local commits that conflict with the remote, run:

```bash
bd dolt status
bd dolt pull
bd dolt push
```

If the conflict persists, ask for help — do not force-push the Dolt remote without understanding the divergence.

---

## 8. Related docs

- `AGENTS.md` — Beads integration rules for AI assistants.
- `docs/plans/` — Feature plans and requirements tracked in Beads.
- Beads CLI docs: https://github.com/steveyegge/beads/tree/main/docs
