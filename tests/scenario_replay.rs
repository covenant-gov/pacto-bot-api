#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! End-to-end replay tests for `pacto-bot-admin scenario run` (U12b/U12c)
//! against a real spawned daemon and an in-process mock relay -- the same
//! daemon-subprocess pattern `tests/mls_group.rs` already uses for MLS
//! admin-CLI coverage.

mod common;
mod support;

use std::error::Error;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use pacto_bot_api::config::{BotConfig, SigningConfig};
use pacto_bot_api::secrecy::ExposeSecret;
use support::mock_relay::MockRelay;

/// `common::make_config` does not emit the per-bot MLS fields
/// (`mls_db_path`, `mls_key_package_freshness_secs`); every scenario
/// participant here needs its own MLS store, so this builds the config
/// text directly instead of appending fields that would only land on the
/// last `[[bots]]` table.
fn write_scenario_config(dir: &Path, bots: &[BotConfig]) -> Result<PathBuf, Box<dyn Error>> {
    let data_dir = dir.to_string_lossy();
    let socket_path = dir.join("pacto-bot-api.sock");
    let mut content = format!(
        "[daemon]\ndata_dir = {:?}\nsocket_path = {:?}\n\
         group_message_rate = 100.0\ngroup_message_burst = 100.0\n\n",
        data_dir, socket_path
    );

    for bot in bots {
        content.push_str("[[bots]]\n");
        content.push_str(&format!("id = {:?}\n", bot.id));
        if let Some(display_name) = &bot.display_name {
            content.push_str(&format!("display_name = {:?}\n", display_name));
        }
        content.push_str(&format!("npub = {:?}\n", bot.npub));
        match &bot.signing {
            SigningConfig::Nsec { nsec } => {
                content.push_str(&format!(
                    "signing = {{ backend = \"nsec\", nsec = {:?} }}\n",
                    nsec.expose_secret()
                ));
            }
            other => return Err(format!("scenario tests only use nsec bots, got {other:?}").into()),
        }
        content.push_str(&format!("relays = {:?}\n", bot.relays));
        content.push_str(&format!("capabilities = {:?}\n", bot.capabilities));
        let mls_db_path = bot
            .mls_db_path
            .as_ref()
            .ok_or("participant bot is missing mls_db_path")?;
        content.push_str(&format!("mls_db_path = {:?}\n", mls_db_path));
        content.push_str("mls_key_package_freshness_secs = 300\n");
        content.push('\n');
    }

    let path = dir.join("pacto-bot-api.toml");
    std::fs::write(&path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }
    Ok(path)
}

/// Build a scenario participant bot: nsec-backed, MLS-capable, pointed at
/// `relay_url`, with every capability a scenario step might need as actor.
fn participant_bot(id: &str, relay_url: &str) -> Result<BotConfig, Box<dyn Error>> {
    let (mut bot, _nsec) = common::generate_nsec_bot(id)?;
    bot.relays = vec![relay_url.to_string()];
    bot.capabilities = vec![
        "Admin".to_string(),
        "SendGroupMessages".to_string(),
        "ReceiveGroupMessages".to_string(),
        "SendMessages".to_string(),
        "ReadMessages".to_string(),
    ];
    bot.mls_db_path = Some(PathBuf::from(format!("{id}-mls.db")));
    bot.mls_key_package_freshness_secs = Some(300);
    Ok(bot)
}

fn fixture(name: &str) -> String {
    format!("tests/fixtures/scenarios/{name}")
}

/// Run `pacto-bot-admin scenario run` against `config` and return the full
/// captured output (stdout+stderr combined via assert_cmd's output).
fn run_scenario(
    config: &Path,
    scenario_file: &str,
    step_timeout_secs: u64,
) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
    cmd.arg("--config")
        .arg(config)
        .arg("scenario")
        .arg("run")
        .arg(scenario_file)
        .arg("--step-timeout")
        .arg(step_timeout_secs.to_string());
    cmd.assert()
}

/// Extract every `<bot_id> <rfc3339> ...` trace line's leading `bot_id`
/// token, in the order printed (which is `created_at ASC, rowid ASC` --
/// true processing order across every participant's shared `agent.db`).
fn trace_bot_id_order(stdout: &str) -> Vec<String> {
    let mut in_trace = false;
    let mut ids = Vec::new();
    for line in stdout.lines() {
        if line.starts_with("scenario trace") {
            in_trace = true;
            continue;
        }
        if !in_trace {
            continue;
        }
        if line.starts_with('(') {
            continue;
        }
        if let Some(bot_id) = line.split_whitespace().next() {
            ids.push(bot_id.to_string());
        }
    }
    ids
}

/// Every `step N (...): ok (...)` progress line, in stdout order.
fn step_progress_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|l| l.starts_with("step ") && l.contains(": ok ("))
        .collect()
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn two_participant_three_message_scenario_replays_in_declared_order()
-> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let alice = participant_bot("alice-bot", &relay.url())?;
    let bob = participant_bot("bob-bot", &relay.url())?;
    let config = write_scenario_config(dir.path(), &[alice, bob])?;

    let log_path = dir.path().join("daemon.log");
    let daemon = common::spawn_daemon_until_ready_with_log(&config, Some(&log_path)).await?;

    let assert = run_scenario(&config, &fixture("two-participant-three-messages.toml"), 20);

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;

    let output = assert.get_output().clone();
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    let dlog = std::fs::read_to_string(&log_path).unwrap_or_default();
    eprintln!("DAEMON LOG:\n{dlog}");
    assert!(
        output.status.success(),
        "scenario run should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let steps = step_progress_lines(&stdout);
    assert_eq!(steps.len(), 4, "expected 4 successful steps\n{stdout}");
    assert!(steps[0].starts_with("step 1 (alice create_group)"));
    assert!(steps[1].starts_with("step 2 (alice send_group_message)"));
    assert!(steps[2].starts_with("step 3 (bob send_group_message)"));
    assert!(steps[3].starts_with("step 4 (alice send_group_message)"));

    // The trace shows the declared order too: bob's welcome-accept row
    // first, then message rows alternating recipients per the script.
    let ids = trace_bot_id_order(&stdout);
    assert_eq!(
        ids.len(),
        4,
        "expected 4 trace rows (1 welcome + 3 messages)\n{stdout}"
    );
    assert_eq!(ids[0], "bob-bot", "welcome accept should trace first");
    assert_eq!(ids[1], "bob-bot", "message one is received by bob");
    assert_eq!(ids[2], "alice-bot", "message two is received by alice");
    assert_eq!(ids[3], "bob-bot", "message three is received by bob");

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn two_participant_dm_scenario_replays_with_recipient_trace() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let alice = participant_bot("alice-bot", &relay.url())?;
    let bob = participant_bot("bob-bot", &relay.url())?;
    let config = write_scenario_config(dir.path(), &[alice, bob])?;

    let daemon = common::spawn_daemon_until_ready(&config).await?;

    let assert = run_scenario(&config, &fixture("two-participant-dm.toml"), 20);

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;

    let output = assert.get_output().clone();
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        output.status.success(),
        "dm scenario run should succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let steps = step_progress_lines(&stdout);
    assert_eq!(steps.len(), 1, "expected 1 successful step\n{stdout}");
    assert!(steps[0].starts_with("step 1 (alice send_dm)"));

    let ids = trace_bot_id_order(&stdout);
    assert_eq!(
        ids,
        vec!["bob-bot".to_string()],
        "recipient should record a trace row\n{stdout}"
    );
    // event_trace.action is the handler response (`ack`), not the inbound
    // event type; the wait-gate above already required DmReceived.
    assert!(
        stdout.lines().any(|line| {
            line.starts_with("bob-bot ")
                && line.contains(" ack ")
                && line.contains("hello bob, this is a dm")
        }),
        "expected a bob-bot ack trace row for the DM\n{stdout}"
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn unreachable_signal_fails_naming_the_step_and_the_signal() -> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let mut alice = participant_bot("alice-bot", &relay.url())?;
    // Alice only creates the group and never sends a group message in this
    // scenario, so she does not need `SendGroupMessages`; she still needs
    // `ReceiveGroupMessages` because the scenario runner's single observer
    // handler registers for every participant up front, before any step
    // runs, and `Dispatch::process_mls_group_message` requires it.
    alice.capabilities = vec!["Admin".to_string(), "ReceiveGroupMessages".to_string()];
    // bob's relay never resolves, so alice's welcome is published but bob's
    // BotState can never receive it -- the observable signal genuinely
    // never arrives.
    let mut bob = participant_bot("bob-bot", &relay.url())?;
    bob.relays = vec!["ws://127.0.0.1:1".to_string()];

    let config = write_scenario_config(dir.path(), &[alice, bob])?;
    let daemon = common::spawn_daemon_until_ready(&config).await?;

    let assert = run_scenario(&config, &fixture("two-participant-three-messages.toml"), 3);

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;

    let output = assert.get_output().clone();
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        !output.status.success(),
        "scenario run should fail when a signal never arrives"
    );
    assert!(
        stderr.contains("step 1"),
        "failure should name the step\n{stderr}"
    );
    assert!(
        stderr.contains("welcome accepted"),
        "failure should name the awaited signal\n{stderr}"
    );
    assert!(
        stderr.contains("bob"),
        "failure should name the participant still awaited\n{stderr}"
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn replaying_the_same_scenario_twice_produces_the_same_event_order()
-> Result<(), Box<dyn Error>> {
    async fn run_once() -> Result<Vec<String>, Box<dyn Error>> {
        let dir = common::tempdir()?;
        let relay = MockRelay::start().await?;

        let alice = participant_bot("alice-bot", &relay.url())?;
        let bob = participant_bot("bob-bot", &relay.url())?;
        let config = write_scenario_config(dir.path(), &[alice, bob])?;
        let daemon = common::spawn_daemon_until_ready(&config).await?;

        let assert = run_scenario(&config, &fixture("two-participant-three-messages.toml"), 20);

        common::shutdown_daemon(daemon).await?;
        relay.stop().await;

        let output = assert.get_output().clone();
        let stdout = String::from_utf8(output.stdout)?;
        assert!(
            output.status.success(),
            "scenario run should succeed\n{stdout}"
        );
        Ok(trace_bot_id_order(&stdout))
    }

    let first = run_once().await?;
    let second = run_once().await?;

    assert!(!first.is_empty(), "first run produced no trace rows");
    assert_eq!(
        first, second,
        "replaying the same scenario twice should produce the same event order"
    );

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn squad_conversation_replays_end_to_end_with_declared_order_in_trace()
-> Result<(), Box<dyn Error>> {
    let dir = common::tempdir()?;
    let relay = MockRelay::start().await?;

    let alice = participant_bot("alice-bot", &relay.url())?;
    let bob = participant_bot("bob-bot", &relay.url())?;
    let carol = participant_bot("carol-bot", &relay.url())?;
    let config = write_scenario_config(dir.path(), &[alice, bob, carol])?;

    let daemon = common::spawn_daemon_until_ready(&config).await?;

    let assert = run_scenario(&config, &fixture("squad-conversation.toml"), 20);

    common::shutdown_daemon(daemon).await?;
    relay.stop().await;

    let output = assert.get_output().clone();
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;
    assert!(
        output.status.success(),
        "squad conversation scenario should replay end to end\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    let steps = step_progress_lines(&stdout);
    assert_eq!(steps.len(), 5, "expected 5 successful steps\n{stdout}");
    assert!(steps[0].starts_with("step 1 (alice create_group)"));
    assert!(steps[1].starts_with("step 2 (alice invite)"));
    assert!(steps[2].starts_with("step 3 (alice send_group_message)"));
    assert!(steps[3].starts_with("step 4 (bob send_group_message)"));
    assert!(steps[4].starts_with("step 5 (carol send_group_message)"));

    // 2 welcomes (bob, carol) + 2 recipients per of the 3 messages = 8 rows.
    let ids = trace_bot_id_order(&stdout);
    assert_eq!(ids.len(), 8, "expected 8 trace rows\n{stdout}");
    assert_eq!(ids[0], "bob-bot", "bob's welcome accept traces first");
    assert_eq!(ids[1], "carol-bot", "carol's welcome accept traces second");
    // Alice never receives anything until bob's reply (step 4), so her
    // first trace row cannot appear before index 4 (2 welcomes + the two
    // recipients of alice's own step-3 message, bob and carol).
    let alice_first = ids
        .iter()
        .position(|id| id == "alice-bot")
        .expect("alice should eventually receive a reply");
    assert!(
        alice_first >= 4,
        "alice should not receive anything before step 4's replies; order was {ids:?}"
    );

    Ok(())
}
