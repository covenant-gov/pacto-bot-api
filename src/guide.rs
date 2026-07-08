//! LLM-readable operator's guide for `pacto-bot-admin`.
//!
//! The guide is emitted as Markdown and covers admin CLI workflows, daemon
//! configuration, handler JSON-RPC basics, and when to use each surface.

use std::fmt::Write;

/// Render the complete operator's guide as Markdown.
pub fn render_llm_guide() -> String {
    let mut out = String::new();

    render_overview(&mut out);
    render_cli_reference(&mut out);
    render_daemon_config(&mut out);
    render_handler_jsonrpc(&mut out);
    render_when_to_use(&mut out);

    out
}

fn render_overview(out: &mut String) {
    out.push_str("# Pacto Bot Operator's Guide\n\n");
    out.push_str(
        "This guide covers running and operating a `pacto-bot-api` daemon and its bots.\n",
    );
    out.push_str("It is intended for bot operators who configure identities, manage state, and monitor health.\n\n");
    out.push_str("- The **admin CLI** (`pacto-bot-admin`) manages bot lifecycle, configuration, diagnostics, and state migration.\n");
    out.push_str("- The **daemon** (`pacto-bot-api`) is the long-lived runtime that connects to Nostr relays, handles NIP-17/44/59 DMs, and routes events to handler processes.\n");
    out.push_str("- **Handlers** are separate processes written in any language that connect over the daemon's JSON-RPC API.\n\n");
}

fn render_cli_reference(out: &mut String) {
    out.push_str("## Admin CLI reference\n\n");
    out.push_str("Global options apply to every subcommand:\n\n");
    out.push_str("- `--config <PATH>` — path to the bot configuration file (default: `pacto-bot-api.toml`).\n");
    out.push_str("- `--data-dir <DIR>` — directory for runtime data (database, socket, token).\n");
    out.push_str("- `--llm-help` — print this operator's guide and exit.\n\n");

    render_command(
        out,
        "new",
        "Create a new bot identity config snippet. Run with no positional arguments to start an interactive interview. Use `--scaffold` to also generate a complete handler project.",
        r#"# Interactive wizard
pacto-bot-admin new

# Non-interactive scripting
pacto-bot-admin new echo-bot --backend nsec --relays ws://localhost:7000 --capabilities ReadMessages --capabilities SendMessages

pacto-bot-admin new echo-bot --backend bunker_remote --uri bunker://<PUBKEY>?relay=wss://relay.nsec.app

# Create a bot and scaffold a Python handler project in one command
pacto-bot-admin new --scaffold echo-bot --backend nsec --relays ws://localhost:7000 --commands echo"#,
        "- `--backend` — `nsec` (dev-only), `bunker_local`, or `bunker_remote`.\n- `--relays` — relay URLs for the bot.\n- `--capabilities` — `ReadMessages`, `SendMessages`, `ManageProfile`, `SendGroupMessages`.\n- `--uri` — bunker URI (required for bunker backends; omit to prompt).\n- `--scaffold` — also generate a handler project for the new identity (non-interactive).\n- `--language` — handler language (default: `python`).\n- `--commands` — slash-command stubs to generate.\n- `--no-tests` — skip pytest files when using `--scaffold`.\n- In interactive mode, the wizard asks whether to scaffold a handler project and where to place it; the generated `pacto-bot-api.toml` is written into that directory.
- Optional profile fields (`display_name`, `about`, `picture`) are collected only in interactive mode.",
    );

    render_command(
        out,
        "scaffold",
        "Generate or update a handler project for an existing bot identity from the daemon config.",
        r#"# Scaffold a project for an existing bot
pacto-bot-admin scaffold echo-bot --commands echo

# Add a second bot to an existing multi-bot project
pacto-bot-admin scaffold price-bot --commands price

# Add tests to an existing scaffolded bot without overwriting handler code
pacto-bot-admin scaffold echo-bot --with-tests"#,
        "- `--language` — handler language (default: `python`).\n- `--commands` — slash-command stubs to generate.\n- `--with-tests` — generate pytest files.\n- `--force` — overwrite existing files without prompting (never overwrites signing material or populated `pacto-bot-api.toml`).\n- `--project-dir` — target directory (default: current directory).\n- The bot identity must already exist in `pacto-bot-api.toml`.",
    );

    render_command(
        out,
        "update",
        "Re-render a scaffolded bot project from its lock file while preserving user edits to protected files.",
        r#"# Update an existing bot project from its lock file
pacto-bot-admin update echo-bot

# Force overwrite protected files
pacto-bot-admin update echo-bot --force

# Update a bot in a specific project directory
pacto-bot-admin update echo-bot --project-dir /path/to/my-project"#,
        "- `--force` — overwrite protected files without prompting (never overwrites signing material or populated `pacto-bot-api.toml`).\n- `--refresh` — re-fetch cached artifacts from the template repository before resolving.\n- `--allow-hooks` — allow `cargo-generate` to execute pre/post-generation hooks.\n- `--project-dir` — target directory containing the bot (default: current directory).\n- The project must contain a `.pacto/bots/<bot-id>/scaffold.lock` file.",
    );

    render_command(
        out,
        "publish-profile",
        "Publish a bot profile (kind:0) event.",
        "pacto-bot-admin publish-profile echo-bot",
        "Requires the bot to exist in the config file.",
    );

    render_command(
        out,
        "test-bunker",
        "Test a NIP-46 bunker connection and verify the returned pubkey matches config.",
        "pacto-bot-admin test-bunker echo-bot",
        "Exits 0 on pubkey match and non-zero on mismatch or connection failure.",
    );

    render_command(
        out,
        "export",
        "Export bot daemon-local state to JSON.",
        "pacto-bot-admin export echo-bot > echo-bot-state.json",
        "Refuses to run while the daemon is running. Does not export nsec or bunker URI.",
    );

    render_command(
        out,
        "import",
        "Import bot daemon-local state from JSON.",
        "pacto-bot-admin import echo-bot echo-bot-state.json",
        "Refuses to run while the daemon is running.",
    );

    render_command(
        out,
        "validate-config",
        "Validate the daemon configuration file.",
        "pacto-bot-admin validate-config",
        "Checks config file permissions, bot uniqueness, and consistency with agent.db.",
    );

    render_command(
        out,
        "rotate-http-token",
        "Rotate the HTTP secret token used by the optional localhost HTTP transport.",
        "pacto-bot-admin rotate-http-token",
        "The daemon must be restarted or sent SIGHUP to reload the token.",
    );

    render_command(
        out,
        "diagnose",
        "Emit structured daemon diagnostics.",
        "pacto-bot-admin diagnose\npacto-bot-admin diagnose --format json",
        "`--format` accepts `text` or `json`.",
    );

    render_command(
        out,
        "doctor",
        "Run a quick health check and print colored PASS/FAIL results with fix suggestions.",
        "pacto-bot-admin doctor",
        "Checks config validity, data directory, daemon lock, configured bots, relay reachability, registered handlers, and HTTP token permissions. Non-zero exit if any check fails.",
    );

    render_command(
        out,
        "logs",
        "Tail the daemon log file (if present).",
        "pacto-bot-admin logs\npacto-bot-admin logs --follow",
        "`--follow` keeps streaming new lines as they are appended. Logs are only available if the daemon writes to a log file; if the daemon is running in a terminal, logs are in that terminal.",
    );

    render_command(
        out,
        "send-test-dm",
        "Send a test DM as a bot from the daemon and print the resulting event ID.",
        r#"pacto-bot-admin send-test-dm echo-bot npub1recipient... "hello""#,
        "The bot must have the `Admin` capability. Useful for verifying the daemon can sign and publish without involving a client.",
    );

    render_command(
        out,
        "trace-events",
        "Read recent event trace rows from the daemon database.",
        "pacto-bot-admin trace-events echo-bot\npacto-bot-admin trace-events echo-bot --since 30 --limit 50",
        "`--since` is the number of minutes to look back (default: 10). `--limit` caps the number of rows (default: 100).",
    );

    render_command(
        out,
        "status",
        "Show daemon status, connectivity, and registered handlers.",
        "pacto-bot-admin status\npacto-bot-admin status --format json",
        "`--format` accepts `text` or `json`.",
    );
}

fn render_command(out: &mut String, name: &str, description: &str, examples: &str, notes: &str) {
    let _ = writeln!(out, "### `{name}`");
    out.push('\n');
    out.push_str(description);
    out.push_str("\n\nExamples:\n```bash\n");
    out.push_str(examples);
    out.push_str("\n```\n\nNotes:\n");
    out.push_str(notes);
    out.push_str("\n\n");
}

fn render_daemon_config(out: &mut String) {
    out.push_str("## Daemon configuration\n\n");
    out.push_str("The daemon reads bot identities from `pacto-bot-api.toml`. The file must be readable only by the owner (`0o600` or stricter).\n\n");
    out.push_str("Example config:\n\n");
    out.push_str("```toml\n");
    out.push_str("[[bots]]\n");
    out.push_str("id = \"echo-bot\"\n");
    out.push_str("npub = \"npub1...\"\n");
    out.push_str("signing = { backend = \"nsec\", nsec = \"<NSEC>\" }\n");
    out.push_str("relays = [\"ws://localhost:7000\"]\n");
    out.push_str("capabilities = [\"ReadMessages\", \"SendMessages\", \"SendGroupMessages\"]\n");
    out.push('\n');
    out.push_str("[[bots]]\n");
    out.push_str("id = \"secure-bot\"\n");
    out.push_str("npub = \"npub1...\"\n");
    out.push_str("signing = { backend = \"bunker_remote\", uri = \"<BUNKER_URI>\" }\n");
    out.push_str("relays = [\"wss://relay.example.com\"]\n");
    out.push_str("capabilities = [\"ReadMessages\", \"SendGroupMessages\"]\n");
    out.push_str("```\n\n");
    out.push_str("Signing backends:\n\n");
    out.push_str("- `nsec` — dev-only local test key. Use `PACTO_BOT_NSEC` environment variable or the config file.\n");
    out.push_str("- `bunker_local` — NIP-46 bunker on the same machine.\n");
    out.push_str("- `bunker_remote` — production NIP-46 bunker reachable over `wss://`.\n\n");
    out.push_str("Run the daemon with:\n\n");
    out.push_str("```bash\n");
    out.push_str("pacto-bot-api --config pacto-bot-api.toml\n");
    out.push_str("# Optional HTTP transport on 127.0.0.1:9800\n");
    out.push_str("pacto-bot-api --config pacto-bot-api.toml --enable-http\n");
    out.push_str("```\n\n");
}

fn render_handler_jsonrpc(out: &mut String) {
    out.push_str("## Handler JSON-RPC basics\n\n");
    out.push_str("Handlers connect to the daemon over the Unix socket at `$DATA_DIR/pacto-bot-api.sock` or the optional localhost HTTP transport at `127.0.0.1:9800`.\n");
    out.push_str("HTTP requests must include the `X-Pacto-Bot-Secret` header.\n\n");

    out.push_str("### Register a handler\n\n");
    out.push_str("```json\n");
    out.push_str(r#"{"jsonrpc":"2.0","id":1,"method":"handler.register","params":{"bot_ids":["echo-bot"],"event_types":["dm_received"],"capabilities":["ReadMessages","SendMessages"]}}"#);
    out.push_str("\n```\n\n");

    out.push_str("### Receive an event\n\n");
    out.push_str("The daemon forwards decrypted DMs as `agent.event` notifications:\n\n");
    out.push_str("```json\n");
    out.push_str(r#"{"jsonrpc":"2.0","method":"agent.event","params":{"bot_id":"echo-bot","type":"dm_received","content":"hello","author":"<npub>","rumor_id":"<id>","event_id":"<id>"}}"#);
    out.push_str("\n```\n\n");

    out.push_str("### Reply to a DM\n\n");
    out.push_str("```json\n");
    out.push_str(r#"{"jsonrpc":"2.0","id":2,"method":"agent.send_dm","params":{"bot_id":"echo-bot","recipient":"<npub>","content":"hello back"}}"#);
    out.push_str("\n```\n\n");

    out.push_str("Handlers must declare capabilities at registration. The daemon rejects calls that exceed those capabilities.\n\n");
}

fn render_when_to_use(out: &mut String) {
    out.push_str("## When to use which\n\n");
    out.push_str("- **Admin CLI (`pacto-bot-admin`)** — use for lifecycle and diagnostics: creating bot identities, publishing profiles, testing bunkers, exporting/importing state, validating config, rotating tokens, checking status, running quick health checks (`doctor`), and tracing recent events (`trace-events`).\n");
    out.push_str("- **Daemon (`pacto-bot-api`)** — use as the long-lived runtime: it owns relay connections, decrypts DMs, enforces capabilities, and persists cursors. Start it once and leave it running.\n");
    out.push_str("- **Handler JSON-RPC** — use when writing bot logic in any language: connect a handler to the daemon's Unix socket or HTTP transport, register for events, and respond with `agent.send_dm` or `agent.set_profile`.\n\n");
    out.push_str("Typical workflow:\n\n");
    out.push_str("1. Use `pacto-bot-admin new` to create a bot identity.\n");
    out.push_str("2. Add the generated config snippet to `pacto-bot-api.toml`.\n");
    out.push_str("3. Run `pacto-bot-admin validate-config` to verify the file.\n");
    out.push_str("4. Start `pacto-bot-api --config pacto-bot-api.toml`.\n");
    out.push_str("5. Connect a handler over JSON-RPC and register for events.\n");
    out.push_str("6. Use `pacto-bot-admin doctor` or `diagnose` to monitor health, and `trace-events` to inspect recent traffic.\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guide_includes_all_required_sections() {
        let guide = render_llm_guide();
        assert!(guide.contains("# Pacto Bot Operator's Guide"));
        assert!(guide.contains("## Admin CLI reference"));
        assert!(guide.contains("## Daemon configuration"));
        assert!(guide.contains("## Handler JSON-RPC basics"));
        assert!(guide.contains("## When to use which"));
    }

    #[test]
    fn guide_includes_examples_for_every_subcommand() {
        let guide = render_llm_guide();
        for sub in [
            "new",
            "scaffold",
            "publish-profile",
            "test-bunker",
            "export",
            "import",
            "validate-config",
            "rotate-http-token",
            "diagnose",
            "doctor",
            "logs",
            "send-test-dm",
            "trace-events",
            "status",
        ] {
            assert!(
                guide.contains(&format!("pacto-bot-admin {sub}")),
                "missing example for {sub}"
            );
        }
    }

    #[test]
    fn guide_contains_no_literal_secrets() {
        let guide = render_llm_guide();
        assert!(!guide.contains("nsec1"), "guide contains literal nsec");
        // Bunker URI placeholders are fine (e.g. bunker://<PUBKEY>); real key
        // material would appear after the scheme without angle brackets.
        assert!(
            !guide.contains("bunker://") || guide.contains("bunker://<"),
            "guide contains literal bunker URI without placeholder"
        );
        assert!(
            guide.contains("<NSEC>") && guide.contains("<BUNKER_URI>"),
            "guide should use placeholders"
        );
    }
}
