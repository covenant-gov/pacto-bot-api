#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Black-box CLI-surface tests for `pacto-bot-admin scenario validate`
//! (U12a). These exercise the actual subcommand a scenario author runs,
//! not just the internal `parse` function (already covered by unit tests
//! in `src/scenario.rs`), and need no daemon or relay.

use assert_cmd::Command;
use predicates::prelude::*;

fn fixture(name: &str) -> String {
    format!("tests/fixtures/scenarios/{name}")
}

#[test]
fn validate_accepts_a_well_formed_scenario() {
    let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
    cmd.args(["scenario", "validate", &fixture("squad-conversation.toml")]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("3 participants, 5 steps"));
}

#[test]
fn validate_rejects_unknown_participant_at_parse_time() {
    let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
    cmd.args(["scenario", "validate", &fixture("unknown-participant.toml")]);
    cmd.assert().failure().stderr(
        predicate::str::contains("carol")
            .and(predicate::str::contains("not a declared participant")),
    );
}

#[test]
fn validate_rejects_unknown_verb_at_parse_time() {
    let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
    cmd.args(["scenario", "validate", &fixture("unknown-verb.toml")]);
    cmd.assert().failure().stderr(
        predicate::str::contains("unknown action").and(predicate::str::contains("teleport")),
    );
}

#[test]
fn validate_rejects_a_declared_wall_clock_gap_and_names_the_ordering_only_contract() {
    let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
    cmd.args(["scenario", "validate", &fixture("wall-clock-gap.toml")]);
    cmd.assert().failure().stderr(
        predicate::str::contains("after")
            .and(predicate::str::contains("ordering-only"))
            .and(predicate::str::contains("Timestamp::now()")),
    );
}

#[test]
fn validate_accepts_the_two_participant_three_message_fixture() {
    let mut cmd = Command::cargo_bin("pacto-bot-admin").unwrap();
    cmd.args([
        "scenario",
        "validate",
        &fixture("two-participant-three-messages.toml"),
    ]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("2 participants, 4 steps"));
}
