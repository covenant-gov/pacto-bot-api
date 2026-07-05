use crate::evm::snapshot::{
    CrewDeadline, CrewState, DeadlineKind, HatState, Mutiny, Proposal, SnapshotData, SquadInfo,
    TreasuryBalance,
};
use alloy::primitives::U256;
use chrono::DateTime;

/// Formats a governance snapshot into a Markdown summary suitable for a Squad channel.
///
/// The output covers active proposals, upcoming crew deadlines, treasury balances,
/// active mutinies, captain/crew state, and a short list of suggested discussion prompts.
/// Sections with no entries render a clear "No active ..." line rather than empty space.
pub fn format_snapshot(data: SnapshotData) -> String {
    let mut out = String::new();

    out.push_str("# Pacto Governance Snapshot\n\n");
    out.push_str(&format!(
        "- Generated at: {}\n",
        fmt_timestamp(data.generated_at)
    ));

    out.push_str("\n## Squad Info\n\n");
    out.push_str(&format_squad_info(&data.squad));

    out.push_str("\n## Active Proposals\n\n");
    if data.proposals.is_empty() {
        out.push_str("No active proposals.\n");
    } else {
        for proposal in &data.proposals {
            out.push_str(&format_proposal(proposal, data.generated_at));
        }
    }

    out.push_str("\n## Upcoming Deadlines\n\n");
    if data.crew_deadlines.is_empty() {
        out.push_str("No upcoming crew deadlines.\n");
    } else {
        for deadline in &data.crew_deadlines {
            out.push_str(&format_crew_deadline(deadline));
        }
    }

    out.push_str("\n## Treasury / Safe Balance\n\n");
    out.push_str(&format_treasury(&data.treasury));

    out.push_str("\n## Active Mutinies\n\n");
    if data.mutinies.is_empty() {
        out.push_str("No active mutinies.\n");
    } else {
        for mutiny in &data.mutinies {
            out.push_str(&format_mutiny(mutiny));
        }
    }

    out.push_str("\n## Captain & Crew\n\n");
    out.push_str(&format_crew_state(&data.crew_state));

    out.push_str("\n## Suggested Discussion Prompts\n\n");
    out.push_str(&format_prompts(&data));

    out
}

fn format_squad_info(squad: &SquadInfo) -> String {
    format!(
        "- Safe: {}\n\
         - Quartermaster: {}\n\
         - Mutiny module: {}\n\
         - Treasury authority: {}\n\
         - Squad admin proxy: {}\n\
         - Top hat: {}\n\
         - Captain hat: {}\n\
         - Crew hat: {}\n\
         - Squad admin hat: {}\n\
         - Mutiny role hat: {}\n\
         - Quartermaster role hat: {}\n\
         - Treasury authority role hat: {}\n\
         - Deployed at: {}\n\
         - Deployer: {}\n",
        squad.safe,
        squad.quartermaster,
        squad.mutiny_module,
        squad.treasury_authority,
        squad.squad_admin_proxy,
        squad.top_hat_id,
        squad.captain_hat_id,
        squad.crew_hat_id,
        squad.squad_admin_hat_id,
        squad.mutiny_role_hat_id,
        squad.quartermaster_role_hat_id,
        squad.treasury_authority_role_hat_id,
        fmt_timestamp(squad.deployed_at),
        squad.deployer
    )
}

fn format_proposal(proposal: &Proposal, generated_at: u64) -> String {
    let mut out = String::new();
    out.push_str(&format!("### Proposal #{}\n\n", proposal.id));
    out.push_str(&format!("- Proposer: {}\n", proposal.proposer));
    out.push_str(&format!("- Target: {}\n", proposal.to));
    out.push_str(&format!(
        "- Value: {} ETH\n",
        format_token_amount(proposal.value, 18)
    ));
    out.push_str(&format!("- Operation: {}\n", proposal.op));

    let deadline = fmt_timestamp(proposal.deadline);
    let remaining = fmt_duration_until(generated_at, proposal.deadline);
    out.push_str(&format!("- Deadline: {} ({})\n", deadline, remaining));

    out.push_str(&format!(
        "- Yeas: {} / Nays: {}\n",
        proposal.yeas, proposal.nays
    ));
    out.push_str(&format!(
        "- Captain approved: {}\n",
        if proposal.captain_approved {
            "Yes"
        } else {
            "No"
        }
    ));
    out.push_str(&format!("- Status: {}\n", proposal_status(proposal)));
    out.push('\n');
    out
}

fn proposal_status(proposal: &Proposal) -> &'static str {
    if proposal.executed {
        "Executed"
    } else if proposal.captain_defeated {
        "Defeated"
    } else {
        "Open"
    }
}

fn format_crew_deadline(deadline: &CrewDeadline) -> String {
    let action = match deadline.kind {
        DeadlineKind::Add => "add",
        DeadlineKind::Remove => "remove",
    };
    format!(
        "- Crew {} for {} executable at {}\n",
        action,
        deadline.target,
        fmt_timestamp(deadline.executable_at)
    )
}

fn format_treasury(treasury: &TreasuryBalance) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "- ETH: {}\n",
        format_token_amount(treasury.eth_balance, 18)
    ));

    if treasury.tokens.is_empty() {
        out.push_str("- No ERC-20 tokens tracked.\n");
    } else {
        out.push_str("- Tokens:\n");
        for token in &treasury.tokens {
            out.push_str(&format!(
                "  - {} ({}): {}\n",
                token.token,
                token.symbol,
                format_token_amount(token.balance, token.decimals)
            ));
        }
    }

    out
}

fn format_mutiny(mutiny: &Mutiny) -> String {
    let mut out = String::new();
    out.push_str(&format!("### Mutiny #{}\n\n", mutiny.id));
    out.push_str(&format!(
        "- Proposed captain: {}\n",
        mutiny.proposed_new_captain
    ));
    out.push_str(&format!(
        "- Started at: {}\n",
        fmt_timestamp(mutiny.started_at)
    ));
    out.push_str(&format!("- Yeas: {}\n", mutiny.yeas));
    out.push_str(&format!(
        "- Status: {}\n",
        if mutiny.executed {
            "Executed"
        } else {
            "Active"
        }
    ));
    out.push('\n');
    out
}

fn format_crew_state(state: &CrewState) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "- Captain: {} ({}: {})\n",
        state.captain.wearer,
        state.captain.hat_id,
        hat_status(&state.captain)
    ));

    if state.crew.is_empty() {
        out.push_str("- Crew: none\n");
    } else {
        out.push_str("- Crew:\n");
        for member in &state.crew {
            out.push_str(&format!(
                "  - {} ({}: {})\n",
                member.wearer,
                member.hat_id,
                hat_status(member)
            ));
        }
    }

    out
}

fn hat_status(hat: &HatState) -> &'static str {
    if hat.active { "active" } else { "inactive" }
}

fn format_prompts(data: &SnapshotData) -> String {
    let mut prompts: Vec<String> = Vec::new();

    for proposal in &data.proposals {
        let remaining = if proposal.deadline > data.generated_at {
            format!(
                "in {}",
                format_relative_duration(proposal.deadline - data.generated_at)
            )
        } else {
            "overdue".to_string()
        };
        prompts.push(format!(
            "Proposal #{} deadline is {} — discuss.",
            proposal.id, remaining
        ));
    }

    for deadline in &data.crew_deadlines {
        let action = match deadline.kind {
            DeadlineKind::Add => "add",
            DeadlineKind::Remove => "remove",
        };
        prompts.push(format!(
            "Crew {} for {} is executable at {} — review.",
            action,
            deadline.target,
            fmt_timestamp(deadline.executable_at)
        ));
    }

    for mutiny in &data.mutinies {
        prompts.push(format!(
            "Mutiny #{} proposes {} as captain — review.",
            mutiny.id, mutiny.proposed_new_captain
        ));
    }

    if !data.crew_state.captain.active {
        prompts.push(
            "Captain's hat is currently inactive — address leadership continuity.".to_string(),
        );
    }

    if prompts.is_empty() {
        prompts.push("No urgent governance items — check back later.".to_string());
    }

    prompts
        .into_iter()
        .enumerate()
        .map(|(i, prompt)| format!("{}. {}\n", i + 1, prompt))
        .collect()
}

fn format_token_amount(value: U256, decimals: u8) -> String {
    if value == U256::ZERO {
        return "0".to_string();
    }
    if decimals == 0 {
        return value.to_string();
    }

    let divisor = ten_pow(decimals);
    let integer = value / divisor;
    let remainder = value % divisor;

    if remainder == U256::ZERO {
        return integer.to_string();
    }

    let frac = remainder.to_string();
    let width = usize::from(decimals);
    let padded = format!("{:0>width$}", frac, width = width);
    let trimmed = padded.trim_end_matches('0');
    if trimmed.is_empty() {
        integer.to_string()
    } else {
        format!("{}.{}", integer, trimmed)
    }
}

fn ten_pow(exp: u8) -> U256 {
    let base = U256::from(10);
    let mut result = U256::from(1);
    for _ in 0..exp {
        result *= base;
    }
    result
}

fn fmt_timestamp(ts: u64) -> String {
    DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| ts.to_string())
}

fn fmt_duration_until(from: u64, until: u64) -> String {
    if until <= from {
        return "overdue".to_string();
    }
    format!("in {}", format_relative_duration(until - from))
}

fn format_relative_duration(seconds: u64) -> String {
    if seconds >= 86_400 {
        let days = seconds / 86_400;
        let hours = (seconds % 86_400) / 3_600;
        if hours > 0 {
            format!("{}d {}h", days, hours)
        } else {
            format!("{}d", days)
        }
    } else if seconds >= 3_600 {
        let hours = seconds / 3_600;
        let minutes = (seconds % 3_600) / 60;
        if minutes > 0 {
            format!("{}h {}m", hours, minutes)
        } else {
            format!("{}h", hours)
        }
    } else if seconds >= 60 {
        let minutes = seconds / 60;
        let secs = seconds % 60;
        if secs > 0 {
            format!("{}m {}s", minutes, secs)
        } else {
            format!("{}m", minutes)
        }
    } else {
        format!("{}s", seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evm::snapshot::{
        CrewDeadline, CrewState, DeadlineKind, HatState, Mutiny, Proposal, SnapshotData, SquadInfo,
        TokenBalance, TreasuryBalance,
    };
    use alloy::primitives::{Address, U256};

    fn sample_snapshot() -> SnapshotData {
        SnapshotData {
            squad: SquadInfo {
                safe: Address::repeat_byte(0x11),
                quartermaster: Address::repeat_byte(0x22),
                mutiny_module: Address::repeat_byte(0x33),
                treasury_authority: Address::repeat_byte(0x44),
                squad_admin_proxy: Address::repeat_byte(0x55),
                top_hat_id: U256::from(7u64),
                captain_hat_id: U256::from(8u64),
                crew_hat_id: U256::from(9u64),
                squad_admin_hat_id: U256::from(10u64),
                mutiny_role_hat_id: U256::from(11u64),
                quartermaster_role_hat_id: U256::from(12u64),
                treasury_authority_role_hat_id: U256::from(13u64),
                deployed_at: 1_700_000_000,
                deployer: Address::repeat_byte(0x66),
            },
            proposals: vec![Proposal {
                id: U256::from(1u64),
                proposer: Address::repeat_byte(0xaa),
                to: Address::repeat_byte(0xbb),
                value: U256::from(1_000_000_000_000_000_000u64), // 1 ETH
                op: 0,
                data: vec![0x01, 0x02, 0x03],
                deadline: 1_700_086_400, // 1 day after generated_at
                snapshot: U256::from(42u64),
                yeas: U256::from(3u64),
                nays: U256::from(1u64),
                captain_approved: true,
                captain_defeated: false,
                executed: false,
            }],
            mutinies: vec![Mutiny {
                id: U256::from(9u64),
                proposed_new_captain: Address::repeat_byte(0xcc),
                started_at: 1_700_010_000,
                snapshot: U256::from(100u64),
                yeas: U256::from(5u64),
                executed: false,
            }],
            crew_deadlines: vec![CrewDeadline {
                kind: DeadlineKind::Add,
                target: Address::repeat_byte(0xdd),
                executable_at: 1_700_172_800,
            }],
            treasury: TreasuryBalance {
                eth_balance: U256::from(5_000_000_000_000_000_000u64), // 5 ETH
                tokens: vec![TokenBalance {
                    token: Address::repeat_byte(0xee),
                    symbol: "TEST".to_string(),
                    decimals: 6,
                    balance: U256::from(1_000_000u64), // 1.0 TEST
                }],
            },
            crew_state: CrewState {
                captain: HatState {
                    wearer: Address::repeat_byte(0x00),
                    hat_id: U256::from(1u64),
                    active: true,
                },
                crew: vec![HatState {
                    wearer: Address::repeat_byte(0x11),
                    hat_id: U256::from(2u64),
                    active: true,
                }],
            },
            generated_at: 1_700_000_000,
        }
    }

    #[test]
    fn includes_all_sections() {
        let markdown = format_snapshot(sample_snapshot());
        assert!(markdown.contains("# Pacto Governance Snapshot"));
        assert!(markdown.contains("## Squad Info"));
        assert!(markdown.contains("## Active Proposals"));
        assert!(markdown.contains("## Upcoming Deadlines"));
        assert!(markdown.contains("## Treasury / Safe Balance"));
        assert!(markdown.contains("## Active Mutinies"));
        assert!(markdown.contains("## Captain & Crew"));
        assert!(markdown.contains("## Suggested Discussion Prompts"));
    }

    #[test]
    fn includes_proposal_details() {
        let markdown = format_snapshot(sample_snapshot());
        assert!(markdown.contains("### Proposal #1"));
        assert!(markdown.contains("Proposer: 0x"));
        assert!(markdown.contains("Value: 1 ETH"));
        assert!(markdown.contains("Yeas: 3 / Nays: 1"));
        assert!(markdown.contains("Captain approved: Yes"));
        assert!(markdown.contains("Status: Open"));
    }

    #[test]
    fn includes_treasury_and_token_values() {
        let markdown = format_snapshot(sample_snapshot());
        assert!(markdown.contains("ETH: 5"));
        assert!(markdown.contains("TEST"));
        assert!(markdown.contains("1"));
    }

    #[test]
    fn includes_mutiny_details() {
        let markdown = format_snapshot(sample_snapshot());
        assert!(markdown.contains("### Mutiny #9"));
        assert!(markdown.contains("Proposed captain: 0x"));
        assert!(markdown.contains("Status: Active"));
    }

    #[test]
    fn includes_crew_state() {
        let markdown = format_snapshot(sample_snapshot());
        assert!(markdown.contains("Captain:"));
        assert!(markdown.contains("active"));
        assert!(markdown.contains("Crew:"));
    }

    #[test]
    fn empty_proposals_show_no_active_line() {
        let mut data = sample_snapshot();
        data.proposals.clear();
        let markdown = format_snapshot(data);
        assert!(markdown.contains("No active proposals."));
        assert!(!markdown.contains("### Proposal #1"));
    }

    #[test]
    fn empty_mutinies_show_no_active_line() {
        let mut data = sample_snapshot();
        data.mutinies.clear();
        let markdown = format_snapshot(data);
        assert!(markdown.contains("No active mutinies."));
        assert!(!markdown.contains("### Mutiny #9"));
    }

    #[test]
    fn empty_proposals_and_mutinies_have_fallback_prompt() {
        let mut data = sample_snapshot();
        data.proposals.clear();
        data.mutinies.clear();
        data.crew_deadlines.clear();
        data.crew_state.captain.active = true;
        let markdown = format_snapshot(data);
        assert!(markdown.contains("No urgent governance items"));
    }

    #[test]
    fn inactive_captain_emits_prompt() {
        let mut data = sample_snapshot();
        data.crew_state.captain.active = false;
        let markdown = format_snapshot(data);
        assert!(markdown.contains("Captain's hat is currently inactive"));
    }

    #[test]
    fn zero_proposal_value_renders_zero() {
        let mut data = sample_snapshot();
        data.proposals[0].value = U256::ZERO;
        let markdown = format_snapshot(data);
        assert!(markdown.contains("Value: 0 ETH"));
    }

    #[test]
    fn format_token_amount_trims_trailing_zeros() {
        assert_eq!(format_token_amount(U256::from(1_000_000u64), 6), "1");
        assert_eq!(format_token_amount(U256::from(1_500_000u64), 6), "1.5");
        assert_eq!(format_token_amount(U256::from(1_234_567u64), 6), "1.234567");
    }

    #[test]
    fn format_relative_duration_picks_largest_unit() {
        assert_eq!(format_relative_duration(45), "45s");
        assert_eq!(format_relative_duration(90), "1m 30s");
        assert_eq!(format_relative_duration(3_660), "1h 1m");
        assert_eq!(format_relative_duration(90_000), "1d 1h");
    }
}
