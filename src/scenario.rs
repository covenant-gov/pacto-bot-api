//! Declarative multi-participant scenario files that compile to existing bot
//! RPC verbs and replay against a live daemon (U12/R15/KTD8).
//!
//! A scenario names a cast of participants and an ordered list of steps.
//! Each step maps to an existing bot RPC verb (`admin.create_mls_group`,
//! `admin.invite_to_mls_group`, `agent.send_group_message`,
//! `admin.send_test_dm`) -- this module adds no wire-protocol capability.
//! Steps are gated on their predecessor's *observable signal* (a welcome
//! accepted, a message persisted) rather than the wall clock: the runner
//! registers a temporary handler for every participant, ACKs every event it
//! receives (which is also what causes the daemon to persist an
//! `event_trace` row for U12c), and waits on the specific event each step
//! needs before moving on. Parse-time validation rejects an unknown
//! participant, an unknown verb, or any field that would require
//! reproducing a wall-clock gap -- KD11/KTD8 make pacto-bot-api scenario
//! timelines ordering-only, since bot publish paths stamp `Timestamp::now()`
//! with no override.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use chrono::Utc;
use pacto_bot_api::config::BotConfig;
use pacto_bot_api::errors::DaemonError;
use pacto_bot_api::events::{AgentEvent, EventType};
use pacto_bot_api::transport::protocol::{JsonRpcMessage, parse_message, serialize_message};
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};

/// Highest scenario file version this build understands.
const SCENARIO_VERSION: u32 = 1;

/// Event types the scenario runner's listening handler subscribes to. Every
/// verb this module drives resolves to one of these.
const OBSERVED_EVENT_TYPES: &[&str] = &[
    "dm_received",
    "mls_welcome_received",
    "mls_group_message_received",
];

// ---------------------------------------------------------------------------
// Scenario file format (U12a)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawScenario {
    version: u32,
    #[serde(default)]
    participants: Vec<RawParticipant>,
    #[serde(default)]
    steps: Vec<RawStep>,
}

#[derive(Debug, Deserialize)]
struct RawParticipant {
    name: String,
    bot_id: String,
}

#[derive(Debug, Deserialize)]
struct RawStep {
    actor: String,
    action: String,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    invite: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    content: Option<String>,
    /// Wall-clock fields a scenario author might reach for to express a
    /// gap between steps. Recognized so the refusal in [`parse`] can name
    /// the exact field instead of a generic "unknown key" error.
    #[serde(default)]
    after: Option<String>,
    #[serde(default)]
    min_age: Option<String>,
    #[serde(default)]
    retention: Option<String>,
    #[serde(default)]
    cutoff: Option<String>,
    #[serde(default)]
    decay: Option<String>,
}

/// A parsed, fully validated scenario. Construction (see [`parse`]) is the
/// only place structural validity is checked; nothing downstream re-checks
/// participant or verb existence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub participants: Vec<Participant>,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub name: String,
    pub bot_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Create a new MLS group and invite one participant into it.
    CreateGroup {
        actor: String,
        group: String,
        invite: String,
    },
    /// Invite one more participant into an already-created group.
    Invite {
        actor: String,
        group: String,
        invite: String,
    },
    /// Send a message into a group. `recipients` is every other current
    /// member of the group at this point in the timeline, resolved at
    /// parse time from the preceding `create_group`/`invite` steps.
    SendGroupMessage {
        actor: String,
        group: String,
        content: String,
        recipients: Vec<String>,
    },
    /// Send a direct message to one participant.
    SendDm {
        actor: String,
        to: String,
        content: String,
    },
}

impl Step {
    pub fn actor(&self) -> &str {
        match self {
            Step::CreateGroup { actor, .. }
            | Step::Invite { actor, .. }
            | Step::SendGroupMessage { actor, .. }
            | Step::SendDm { actor, .. } => actor,
        }
    }

    pub fn action_name(&self) -> &'static str {
        match self {
            Step::CreateGroup { .. } => "create_group",
            Step::Invite { .. } => "invite",
            Step::SendGroupMessage { .. } => "send_group_message",
            Step::SendDm { .. } => "send_dm",
        }
    }
}

/// Parse-time validation failures (U12a). Every variant is produced by
/// [`parse`] alone -- never mid-run -- and names the step and field
/// involved so a scenario author does not have to guess.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ScenarioError {
    #[error("invalid scenario TOML: {0}")]
    Toml(String),

    #[error(
        "unsupported scenario version {found}; this build understands version {SCENARIO_VERSION}"
    )]
    UnsupportedVersion { found: u32 },

    #[error("participant {index} has an empty name")]
    EmptyParticipantName { index: usize },

    #[error("participant \"{name}\" is declared more than once")]
    DuplicateParticipant { name: String },

    #[error("step {step} ({actor} {action}): \"{actor}\" is not a declared participant")]
    UnknownParticipant {
        step: usize,
        actor: String,
        action: String,
    },

    #[error("step {step} ({actor} {action}): `{field}` names unknown participant \"{who}\"")]
    UnknownParticipantRef {
        step: usize,
        actor: String,
        action: String,
        field: &'static str,
        who: String,
    },

    #[error(
        "step {step}: unknown action \"{action}\"; known actions are create_group, invite, send_group_message, send_dm"
    )]
    UnknownVerb { step: usize, action: String },

    #[error("step {step} ({actor} {action}): missing required field `{field}` for this action")]
    MissingField {
        step: usize,
        actor: String,
        action: String,
        field: &'static str,
    },

    #[error(
        "step {step} ({actor} {action}): declares wall-clock field `{field}` = \"{value}\" -- \
         pacto-bot-api scenario timelines are ordering-only, not clock offsets (KD11/KTD8). Bot \
         publish paths stamp `Timestamp::now()` at send time with no override, so a declared \
         wall-clock gap cannot be reproduced and would otherwise be silently ignored. Remove \
         `{field}` and express the requirement as step order instead."
    )]
    WallClockGap {
        step: usize,
        actor: String,
        action: String,
        field: &'static str,
        value: String,
    },

    #[error(
        "step {step} ({actor} {action}): group \"{group}\" has not been created by an earlier step"
    )]
    UnknownGroup {
        step: usize,
        actor: String,
        action: String,
        group: String,
    },

    #[error("step {step} ({actor} {action}): group \"{group}\" was already created")]
    DuplicateGroup {
        step: usize,
        actor: String,
        action: String,
        group: String,
    },

    #[error("step {step} ({actor} {action}): \"{actor}\" is not a member of group \"{group}\"")]
    NotAMember {
        step: usize,
        actor: String,
        action: String,
        group: String,
    },
}

impl From<ScenarioError> for DaemonError {
    fn from(e: ScenarioError) -> Self {
        DaemonError::Config(e.to_string())
    }
}

/// Parse and fully validate a scenario file's TOML text. Pure and
/// I/O-free: every failure mode here is a parse-time refusal, never a
/// mid-run one (U12a bullet 3/4).
pub fn parse(text: &str) -> Result<Scenario, ScenarioError> {
    let raw: RawScenario = toml::from_str(text).map_err(|e| ScenarioError::Toml(e.to_string()))?;

    if raw.version != SCENARIO_VERSION {
        return Err(ScenarioError::UnsupportedVersion { found: raw.version });
    }

    let mut participants = Vec::with_capacity(raw.participants.len());
    let mut names: HashSet<String> = HashSet::new();
    for (index, p) in raw.participants.into_iter().enumerate() {
        if p.name.trim().is_empty() {
            return Err(ScenarioError::EmptyParticipantName { index });
        }
        if !names.insert(p.name.clone()) {
            return Err(ScenarioError::DuplicateParticipant { name: p.name });
        }
        participants.push(Participant {
            name: p.name,
            bot_id: p.bot_id,
        });
    }

    let mut steps = Vec::with_capacity(raw.steps.len());
    // group name -> members currently in it, in join order.
    let mut group_members: HashMap<String, Vec<String>> = HashMap::new();

    for (i, raw_step) in raw.steps.into_iter().enumerate() {
        let step_no = i + 1;
        let actor = raw_step.actor.clone();
        let action = raw_step.action.clone();

        for (field, value) in [
            ("after", &raw_step.after),
            ("min_age", &raw_step.min_age),
            ("retention", &raw_step.retention),
            ("cutoff", &raw_step.cutoff),
            ("decay", &raw_step.decay),
        ] {
            if let Some(v) = value {
                return Err(ScenarioError::WallClockGap {
                    step: step_no,
                    actor,
                    action,
                    field,
                    value: v.clone(),
                });
            }
        }

        if !names.contains(&actor) {
            return Err(ScenarioError::UnknownParticipant {
                step: step_no,
                actor,
                action,
            });
        }

        let step = match action.as_str() {
            "create_group" => {
                let group = raw_step
                    .group
                    .clone()
                    .ok_or_else(|| ScenarioError::MissingField {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        field: "group",
                    })?;
                let invite =
                    raw_step
                        .invite
                        .clone()
                        .ok_or_else(|| ScenarioError::MissingField {
                            step: step_no,
                            actor: actor.clone(),
                            action: action.clone(),
                            field: "invite",
                        })?;
                if !names.contains(&invite) {
                    return Err(ScenarioError::UnknownParticipantRef {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        field: "invite",
                        who: invite,
                    });
                }
                if group_members.contains_key(&group) {
                    return Err(ScenarioError::DuplicateGroup {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        group,
                    });
                }
                group_members.insert(group.clone(), vec![actor.clone(), invite.clone()]);
                Step::CreateGroup {
                    actor,
                    group,
                    invite,
                }
            }
            "invite" => {
                let group = raw_step
                    .group
                    .clone()
                    .ok_or_else(|| ScenarioError::MissingField {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        field: "group",
                    })?;
                let invite =
                    raw_step
                        .invite
                        .clone()
                        .ok_or_else(|| ScenarioError::MissingField {
                            step: step_no,
                            actor: actor.clone(),
                            action: action.clone(),
                            field: "invite",
                        })?;
                if !names.contains(&invite) {
                    return Err(ScenarioError::UnknownParticipantRef {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        field: "invite",
                        who: invite,
                    });
                }
                let members =
                    group_members
                        .get_mut(&group)
                        .ok_or_else(|| ScenarioError::UnknownGroup {
                            step: step_no,
                            actor: actor.clone(),
                            action: action.clone(),
                            group: group.clone(),
                        })?;
                if !members.contains(&actor) {
                    return Err(ScenarioError::NotAMember {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        group,
                    });
                }
                if !members.contains(&invite) {
                    members.push(invite.clone());
                }
                Step::Invite {
                    actor,
                    group,
                    invite,
                }
            }
            "send_group_message" => {
                let group = raw_step
                    .group
                    .clone()
                    .ok_or_else(|| ScenarioError::MissingField {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        field: "group",
                    })?;
                let content =
                    raw_step
                        .content
                        .clone()
                        .ok_or_else(|| ScenarioError::MissingField {
                            step: step_no,
                            actor: actor.clone(),
                            action: action.clone(),
                            field: "content",
                        })?;
                let members =
                    group_members
                        .get(&group)
                        .ok_or_else(|| ScenarioError::UnknownGroup {
                            step: step_no,
                            actor: actor.clone(),
                            action: action.clone(),
                            group: group.clone(),
                        })?;
                if !members.contains(&actor) {
                    return Err(ScenarioError::NotAMember {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        group,
                    });
                }
                let recipients = members
                    .iter()
                    .filter(|m| **m != actor)
                    .cloned()
                    .collect::<Vec<_>>();
                Step::SendGroupMessage {
                    actor,
                    group,
                    content,
                    recipients,
                }
            }
            "send_dm" => {
                let to = raw_step
                    .to
                    .clone()
                    .ok_or_else(|| ScenarioError::MissingField {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        field: "to",
                    })?;
                let content =
                    raw_step
                        .content
                        .clone()
                        .ok_or_else(|| ScenarioError::MissingField {
                            step: step_no,
                            actor: actor.clone(),
                            action: action.clone(),
                            field: "content",
                        })?;
                if !names.contains(&to) {
                    return Err(ScenarioError::UnknownParticipantRef {
                        step: step_no,
                        actor: actor.clone(),
                        action: action.clone(),
                        field: "to",
                        who: to,
                    });
                }
                Step::SendDm { actor, to, content }
            }
            other => {
                return Err(ScenarioError::UnknownVerb {
                    step: step_no,
                    action: other.to_string(),
                });
            }
        };
        steps.push(step);
    }

    Ok(Scenario {
        participants,
        steps,
    })
}

// ---------------------------------------------------------------------------
// CLI entry points
// ---------------------------------------------------------------------------

fn read_scenario_file(file: &Path) -> Result<Scenario, DaemonError> {
    let text = std::fs::read_to_string(file).map_err(|e| {
        DaemonError::Config(format!(
            "failed to read scenario file {}: {e}",
            file.display()
        ))
    })?;
    Ok(parse(&text)?)
}

/// `pacto-bot-admin scenario validate <FILE>`: parse and validate only.
pub fn cmd_scenario_validate(file: &Path) -> Result<(), DaemonError> {
    let scenario = read_scenario_file(file)?;
    println!(
        "scenario ok: {} participants, {} steps",
        scenario.participants.len(),
        scenario.steps.len()
    );
    Ok(())
}

/// `pacto-bot-admin scenario run <FILE>`: parse, validate, and replay
/// against the daemon at `config_path`/`data_dir_override` (U12b), then
/// print the resulting trace through the existing event-trace output
/// (U12c).
#[cfg(not(unix))]
pub async fn cmd_scenario_run(
    _config_path: &Path,
    _data_dir_override: Option<PathBuf>,
    file: &Path,
    _step_timeout_secs: u64,
) -> Result<(), DaemonError> {
    let _ = read_scenario_file(file)?;
    Err(DaemonError::Config(
        "scenario run is only available on Unix platforms".into(),
    ))
}

#[cfg(unix)]
pub async fn cmd_scenario_run(
    config_path: &Path,
    data_dir_override: Option<PathBuf>,
    file: &Path,
    step_timeout_secs: u64,
) -> Result<(), DaemonError> {
    let scenario = read_scenario_file(file)?;
    if scenario.participants.is_empty() {
        return Err(DaemonError::Config(
            "scenario declares no participants".into(),
        ));
    }

    let config = crate::load_admin_config(config_path)?;
    let mut bots: HashMap<String, BotConfig> = HashMap::new();
    for p in &scenario.participants {
        let bot = crate::find_bot(&config.bots, &p.bot_id)?.clone();
        bots.insert(p.name.clone(), bot);
    }

    let data_dir = crate::resolve_data_dir(&config, data_dir_override);
    // Unreachable: the empty-participants guard above already returned, and
    // every participant inserts a bot. Expressed as an error rather than a
    // panic because the crate denies `expect_used`.
    let any_bot = bots
        .values()
        .next()
        .ok_or_else(|| DaemonError::Config("scenario declares no participants".into()))?;
    let socket_path = crate::resolve_admin_socket_path(&config, any_bot, &data_dir)?;

    // A fresh bot has no live KeyPackage; publish one for every participant
    // that some step invites into a group, up front, so the create_group/
    // invite step targeting them can resolve it via `fetch_key_package`.
    // Only invitees need this: `agent.publish_key_package` requires
    // `SendGroupMessages`, which a participant that never sends may
    // legitimately lack (e.g. a creator-only role need not hold it).
    // `agent.publish_key_package` is an existing bot verb with no
    // dedicated CLI wrapper -- see `call_agent_publish_key_package`.
    let invitee_bot_ids: HashSet<&str> = scenario
        .steps
        .iter()
        .filter_map(|step| match step {
            Step::CreateGroup { invite, .. } | Step::Invite { invite, .. } => {
                bots.get(invite).map(|b| b.id.as_str())
            }
            _ => None,
        })
        .collect();
    for bot_id in invitee_bot_ids {
        crate::call_agent_publish_key_package(&socket_path, bot_id).await?;
    }

    let bot_ids: Vec<String> = scenario
        .participants
        .iter()
        .map(|p| p.bot_id.clone())
        .collect();
    let mut session = ScenarioSession::connect(&socket_path, &bot_ids).await?;
    let run_started = Utc::now();
    let step_timeout = StdDuration::from_secs(step_timeout_secs);

    let result = execute_steps(&scenario, &bots, &socket_path, &mut session, step_timeout).await;
    session.close().await;

    if let Err(e) = emit_trace(&data_dir, &bot_ids, run_started) {
        eprintln!("warning: failed to emit scenario trace: {e}");
    }

    result
}

// ---------------------------------------------------------------------------
// Executor (U12b)
// ---------------------------------------------------------------------------

/// A long-lived handler connection covering every participant bot in the
/// scenario. Reused for the whole run so events are observed in true
/// arrival order through one ordered queue -- no per-step resubscription
/// races.
#[cfg(unix)]
struct ScenarioSession {
    outgoing_tx: mpsc::UnboundedSender<JsonRpcMessage>,
    event_rx: mpsc::UnboundedReceiver<AgentEvent>,
    io_task: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl ScenarioSession {
    async fn connect(socket_path: &Path, bot_ids: &[String]) -> Result<Self, DaemonError> {
        let connect_timeout = StdDuration::from_secs(15);
        let stream = timeout(connect_timeout, UnixStream::connect(socket_path))
            .await
            .map_err(|_| DaemonError::Config("scenario: unix socket connect timed out".into()))??;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // Register synchronously so a registration failure (e.g. an
        // unknown bot_id in the daemon's own config) surfaces immediately
        // instead of hanging inside the background task. `ReceiveGroupMessages`
        // is required: `Dispatch::process_mls_group_message` filters
        // handlers to those authorized for it before fan-out, so every
        // participant a `send_group_message` step targets must grant it.
        let register_id: Value = 1.into();
        let register = JsonRpcMessage::request(
            register_id.clone(),
            "handler.register",
            Some(serde_json::json!({
                "bot_ids": bot_ids,
                "event_types": OBSERVED_EVENT_TYPES,
                "capabilities": ["ReceiveGroupMessages"],
            })),
        );
        let line = format!("{}\n", serialize_message(&register)?);
        write_half
            .write_all(line.as_bytes())
            .await
            .map_err(DaemonError::Io)?;

        let response = loop {
            let mut buf = Vec::new();
            let n = timeout(connect_timeout, reader.read_until(b'\n', &mut buf))
                .await
                .map_err(|_| {
                    DaemonError::Config("scenario: handler.register timed out".into())
                })??;
            if n == 0 {
                return Err(DaemonError::Config(
                    "scenario: daemon closed connection during registration".into(),
                ));
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
            }
            let line = String::from_utf8(buf).map_err(|_| {
                DaemonError::Config("scenario: daemon sent non-UTF-8 response".into())
            })?;
            let msg = parse_message(&line)?;
            if msg.id() == Some(&register_id) {
                break msg;
            }
        };
        match response {
            JsonRpcMessage::Error { error, .. } => {
                return Err(DaemonError::Config(format!(
                    "handler.register failed: {}",
                    error.message
                )));
            }
            JsonRpcMessage::Response { result: None, .. } => {
                return Err(DaemonError::Config(
                    "handler.register returned no result".into(),
                ));
            }
            JsonRpcMessage::Response { .. } => {}
            _ => {
                return Err(DaemonError::Config(
                    "scenario: unexpected handler.register response".into(),
                ));
            }
        }

        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<JsonRpcMessage>();
        let (event_tx, event_rx) = mpsc::unbounded_channel::<AgentEvent>();
        let ack_tx = outgoing_tx.clone();

        let io_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = outgoing_rx.recv() => {
                        let Ok(line) = serialize_message(&msg) else { continue };
                        if write_half.write_all(line.as_bytes()).await.is_err() { break; }
                        if write_half.write_all(b"\n").await.is_err() { break; }
                        if write_half.flush().await.is_err() { break; }
                    }
                    result = read_frame(&mut reader) => {
                        let Ok(line) = result else { break };
                        let trimmed = line.trim();
                        if trimmed.is_empty() { continue; }
                        let Ok(msg) = parse_message(trimmed) else { continue };
                        if let JsonRpcMessage::Notification { method, params, .. } = &msg
                            && method == "agent.event"
                            && let Some(params) = params
                            && let Ok(event) = serde_json::from_value::<AgentEvent>(params.clone())
                        {
                            let ack = JsonRpcMessage::notification(
                                "handler.response",
                                Some(serde_json::json!({
                                    "event_id": event.event_id,
                                    "action": "ack",
                                })),
                            );
                            let _ = ack_tx.send(ack);
                            let _ = event_tx.send(event);
                        }
                        // Other frames (agent.status, agent.metrics, the
                        // handler.unregister response sent from close())
                        // are not relevant to step gating and are dropped.
                    }
                }
            }
        });

        Ok(Self {
            outgoing_tx,
            event_rx,
            io_task,
        })
    }

    async fn close(self) {
        let _ = self.outgoing_tx.send(JsonRpcMessage::request(
            Value::from(999),
            "handler.unregister",
            Some(serde_json::json!({})),
        ));
        tokio::time::sleep(StdDuration::from_millis(150)).await;
        self.io_task.abort();
    }
}

#[cfg(unix)]
async fn read_frame(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
) -> Result<String, std::io::Error> {
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "connection closed",
        ));
    }
    Ok(line)
}

/// Execute every step in order, gating each on its predecessor's
/// observable signal rather than sleeping or checking elapsed time
/// (U12b). A step whose awaited signal never arrives fails naming both
/// the step and the signal.
#[cfg(unix)]
async fn execute_steps(
    scenario: &Scenario,
    bots: &HashMap<String, BotConfig>,
    socket_path: &Path,
    session: &mut ScenarioSession,
    step_timeout: StdDuration,
) -> Result<(), DaemonError> {
    let mut wire_ids: HashMap<String, String> = HashMap::new();

    for (i, step) in scenario.steps.iter().enumerate() {
        let step_no = i + 1;
        let label = format!("step {step_no} ({} {})", step.actor(), step.action_name());

        match step {
            Step::CreateGroup {
                actor,
                group,
                invite,
            } => {
                let actor_bot = &bots[actor];
                let invite_bot = &bots[invite];
                let recipient = crate::validate_mls_recipient(&invite_bot.npub)?;
                let response = crate::call_admin_create_mls_group(
                    socket_path,
                    &actor_bot.id,
                    group,
                    &recipient,
                    &[],
                )
                .await?;
                let signal = format!("welcome accepted by {invite} for group \"{group}\"");
                let wire_id = response.wire_id.clone();
                let mut targets = HashSet::new();
                targets.insert(invite_bot.id.clone());
                wait_for_all(
                    &mut session.event_rx,
                    targets,
                    |e| {
                        e.event_type == EventType::MlsWelcomeReceived
                            && e.chat_id.as_deref() == Some(wire_id.as_str())
                    },
                    &label,
                    &signal,
                    step_timeout,
                )
                .await?;
                println!("{label}: ok ({signal} observed)");
                wire_ids.insert(group.clone(), response.wire_id);
            }
            Step::Invite {
                actor,
                group,
                invite,
            } => {
                let actor_bot = &bots[actor];
                let invite_bot = &bots[invite];
                let recipient = crate::validate_mls_recipient(&invite_bot.npub)?;
                let response = crate::call_admin_invite_to_mls_group(
                    socket_path,
                    &actor_bot.id,
                    group,
                    &recipient,
                )
                .await?;
                let signal = format!("welcome accepted by {invite} for group \"{group}\"");
                let wire_id = response.wire_id.clone();
                let mut targets = HashSet::new();
                targets.insert(invite_bot.id.clone());
                wait_for_all(
                    &mut session.event_rx,
                    targets,
                    |e| {
                        e.event_type == EventType::MlsWelcomeReceived
                            && e.chat_id.as_deref() == Some(wire_id.as_str())
                    },
                    &label,
                    &signal,
                    step_timeout,
                )
                .await?;
                println!("{label}: ok ({signal} observed)");
                wire_ids.entry(group.clone()).or_insert(response.wire_id);
            }
            Step::SendGroupMessage {
                actor,
                group,
                content,
                recipients,
            } => {
                let actor_bot = &bots[actor];
                crate::require_bot_capability(actor_bot, "SendGroupMessages")?;
                let wire_id = wire_ids.get(group).cloned().ok_or_else(|| {
                    DaemonError::Config(format!(
                        "{label}: group \"{group}\" has no known wire id (its create_group step did not run)"
                    ))
                })?;
                crate::call_agent_send_group_message(socket_path, &actor_bot.id, &wire_id, content)
                    .await?;

                let mut targets = HashSet::new();
                for recipient in recipients {
                    targets.insert(bots[recipient].id.clone());
                }
                let signal = format!(
                    "message persisted for {} in group \"{group}\"",
                    recipients.join(", ")
                );
                let expected_content = content.clone();
                wait_for_all(
                    &mut session.event_rx,
                    targets,
                    |e| {
                        e.event_type == EventType::MlsGroupMessageReceived
                            && e.chat_id.as_deref() == Some(wire_id.as_str())
                            && e.content == expected_content
                    },
                    &label,
                    &signal,
                    step_timeout,
                )
                .await?;
                println!("{label}: ok ({signal} observed)");
            }
            Step::SendDm { actor, to, content } => {
                let actor_bot = &bots[actor];
                let to_bot = &bots[to];
                crate::call_admin_send_test_dm(socket_path, &actor_bot.id, &to_bot.npub, content)
                    .await?;

                let mut targets = HashSet::new();
                targets.insert(to_bot.id.clone());
                let signal = format!("DM persisted for {to}");
                let expected_content = content.clone();
                wait_for_all(
                    &mut session.event_rx,
                    targets,
                    |e| e.event_type == EventType::DmReceived && e.content == expected_content,
                    &label,
                    &signal,
                    step_timeout,
                )
                .await?;
                println!("{label}: ok ({signal} observed)");
            }
        }
    }

    Ok(())
}

/// Wait until every bot id in `targets` has produced an event matching
/// `matches`, draining `event_rx` in true arrival order so concurrent
/// recipients of the same step are never dropped by an earlier wait.
#[cfg(unix)]
async fn wait_for_all<F>(
    event_rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
    mut targets: HashSet<String>,
    mut matches: F,
    label: &str,
    signal: &str,
    wait: StdDuration,
) -> Result<(), DaemonError>
where
    F: FnMut(&AgentEvent) -> bool,
{
    let deadline = Instant::now() + wait;
    while !targets.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let mut still = targets.into_iter().collect::<Vec<_>>();
            still.sort();
            return Err(DaemonError::Config(format!(
                "{label}: timed out waiting for {signal} (still awaiting bot(s): {})",
                still.join(", ")
            )));
        }
        match timeout(remaining, event_rx.recv()).await {
            Ok(Some(event)) => {
                if targets.contains(&event.bot_id) && matches(&event) {
                    targets.remove(&event.bot_id);
                }
            }
            Ok(None) => {
                return Err(DaemonError::Config(format!(
                    "{label}: event channel closed while waiting for {signal}"
                )));
            }
            Err(_) => {
                let mut still = targets.into_iter().collect::<Vec<_>>();
                still.sort();
                return Err(DaemonError::Config(format!(
                    "{label}: timed out waiting for {signal} (still awaiting bot(s): {})",
                    still.join(", ")
                )));
            }
        }
    }
    Ok(())
}

/// Print every `event_trace` row recorded for this run's participants
/// since it started, merged in true processing order across bots (they
/// share one `agent.db`). Reuses [`crate::format_trace_line`] so the
/// scenario runner never invents a second trace format (U12c).
#[cfg(unix)]
fn emit_trace(
    data_dir: &Path,
    bot_ids: &[String],
    since: chrono::DateTime<Utc>,
) -> Result<(), DaemonError> {
    let db_path = data_dir.join(crate::AGENT_DB_FILE);
    if !db_path.exists() {
        return Err(DaemonError::Config(format!(
            "daemon database not found at {}",
            db_path.display()
        )));
    }
    let conn = crate::open_agent_db(&db_path)?;
    let placeholders = bot_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT bot_id, event_id, author, content_preview, action, reply_event_id, created_at
         FROM event_trace
         WHERE bot_id IN ({placeholders}) AND created_at >= ?
         ORDER BY created_at ASC, rowid ASC
         LIMIT 1000"
    );
    let mut stmt = conn.prepare(&sql)?;
    let since_ts = since.timestamp();
    let mut params: Vec<&dyn rusqlite::ToSql> =
        bot_ids.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
    params.push(&since_ts);
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, i64>(6)?,
        ))
    })?;

    println!("scenario trace (oldest first):");
    let mut any = false;
    for row in rows {
        let (bot_id, event_id, author, preview, action, reply_event_id, created_at) = row?;
        any = true;
        println!(
            "{bot_id} {}",
            crate::format_trace_line(
                created_at,
                &event_id,
                &author,
                &action,
                reply_event_id.as_deref(),
                &preview
            )
        );
    }
    if !any {
        println!("(no trace rows recorded for this scenario run)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_scenario_text() -> &'static str {
        r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[participants]]
name = "bob"
bot_id = "bob-bot"

[[steps]]
actor = "alice"
action = "create_group"
group = "squad-chat"
invite = "bob"

[[steps]]
actor = "alice"
action = "send_group_message"
group = "squad-chat"
content = "hello bob"

[[steps]]
actor = "bob"
action = "send_group_message"
group = "squad-chat"
content = "hi alice"

[[steps]]
actor = "alice"
action = "send_group_message"
group = "squad-chat"
content = "how's it going"
"#
    }

    #[test]
    fn parses_a_valid_scenario() {
        let scenario = parse(valid_scenario_text()).expect("valid scenario should parse");
        assert_eq!(scenario.participants.len(), 2);
        assert_eq!(scenario.steps.len(), 4);
        match &scenario.steps[1] {
            Step::SendGroupMessage { recipients, .. } => {
                assert_eq!(recipients, &vec!["bob".to_string()]);
            }
            other => panic!("expected SendGroupMessage, got {other:?}"),
        }
    }

    #[test]
    fn parsing_is_deterministic() {
        let a = parse(valid_scenario_text()).unwrap();
        let b = parse(valid_scenario_text()).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn unknown_participant_fails_at_parse_time() {
        let text = r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[steps]]
actor = "carol"
action = "send_dm"
to = "alice"
content = "hi"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ScenarioError::UnknownParticipant { .. }));
        assert!(err.to_string().contains("carol"));
    }

    #[test]
    fn unknown_verb_fails_at_parse_time() {
        let text = r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[participants]]
name = "bob"
bot_id = "bob-bot"

[[steps]]
actor = "alice"
action = "teleport"
to = "bob"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ScenarioError::UnknownVerb { .. }));
        assert!(err.to_string().contains("teleport"));
    }

    #[test]
    fn wall_clock_gap_is_rejected_and_names_the_ordering_only_contract() {
        let text = r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[participants]]
name = "bob"
bot_id = "bob-bot"

[[steps]]
actor = "alice"
action = "send_dm"
to = "bob"
content = "hi"
after = "30s"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ScenarioError::WallClockGap { .. }));
        let msg = err.to_string();
        assert!(msg.contains("ordering-only"));
        assert!(msg.contains("after"));
    }

    #[test]
    fn retention_field_is_also_rejected_as_a_wall_clock_gap() {
        let text = r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[participants]]
name = "bob"
bot_id = "bob-bot"

[[steps]]
actor = "alice"
action = "send_dm"
to = "bob"
content = "hi"
retention = "1h"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(
            err,
            ScenarioError::WallClockGap {
                field: "retention",
                ..
            }
        ));
    }

    #[test]
    fn sending_into_an_undeclared_group_fails_at_parse_time() {
        let text = r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[steps]]
actor = "alice"
action = "send_group_message"
group = "ghost-squad"
content = "hi"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ScenarioError::UnknownGroup { .. }));
    }

    #[test]
    fn sending_by_a_non_member_fails_at_parse_time() {
        let text = r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[participants]]
name = "bob"
bot_id = "bob-bot"

[[participants]]
name = "carol"
bot_id = "carol-bot"

[[steps]]
actor = "alice"
action = "create_group"
group = "squad-chat"
invite = "bob"

[[steps]]
actor = "carol"
action = "send_group_message"
group = "squad-chat"
content = "hi"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ScenarioError::NotAMember { .. }));
    }

    #[test]
    fn duplicate_group_creation_fails_at_parse_time() {
        let text = r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[participants]]
name = "bob"
bot_id = "bob-bot"

[[steps]]
actor = "alice"
action = "create_group"
group = "squad-chat"
invite = "bob"

[[steps]]
actor = "alice"
action = "create_group"
group = "squad-chat"
invite = "bob"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(err, ScenarioError::DuplicateGroup { .. }));
    }

    #[test]
    fn invite_step_grows_group_membership_for_later_sends() {
        let text = r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[participants]]
name = "bob"
bot_id = "bob-bot"

[[participants]]
name = "carol"
bot_id = "carol-bot"

[[steps]]
actor = "alice"
action = "create_group"
group = "squad-chat"
invite = "bob"

[[steps]]
actor = "alice"
action = "invite"
group = "squad-chat"
invite = "carol"

[[steps]]
actor = "alice"
action = "send_group_message"
group = "squad-chat"
content = "hi all"
"#;
        let scenario = parse(text).unwrap();
        match scenario.steps.last().unwrap() {
            Step::SendGroupMessage { recipients, .. } => {
                let mut sorted = recipients.clone();
                sorted.sort();
                assert_eq!(sorted, vec!["bob".to_string(), "carol".to_string()]);
            }
            other => panic!("expected SendGroupMessage, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_version_is_rejected() {
        let text = r#"
version = 2

[[participants]]
name = "alice"
bot_id = "alice-bot"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(
            err,
            ScenarioError::UnsupportedVersion { found: 2 }
        ));
    }

    #[test]
    fn unknown_invite_target_fails_at_parse_time() {
        let text = r#"
version = 1

[[participants]]
name = "alice"
bot_id = "alice-bot"

[[steps]]
actor = "alice"
action = "create_group"
group = "squad-chat"
invite = "ghost"
"#;
        let err = parse(text).unwrap_err();
        assert!(matches!(
            err,
            ScenarioError::UnknownParticipantRef {
                field: "invite",
                ..
            }
        ));
    }
}
