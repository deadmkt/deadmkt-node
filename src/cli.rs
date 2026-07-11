// =========================================================================
// cli.rs: CLI subcommands (clap)
// =========================================================================

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "deadmkt-node", version = env!("CARGO_PKG_VERSION"), about = "deadmkt trading node")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Path to setup JSON config for non-interactive setup.
    ///   - MR1a (fresh):       no keystore + config has keystore_password
    ///   - MR1b (LLM-assisted): keystore present + config has no password
    ///                          (operator types password at prompt)
    ///   - MR1c (restore):     keystore present + no --config (run with no flags)
    /// MR1d gates: a config with keystore_password must be `chmod 600`;
    /// after a successful fresh setup the password is scrubbed from the file.
    #[arg(long)]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum Command {
    /// Run the node (default when no subcommand given)
    Run,
    /// Run first-boot setup wizard
    Setup,
    /// Show node state, batch, balance summary.
    /// With `--json`, emits the MR2 v1 status JSON to stdout (queries
    /// the running node on 127.0.0.1:9292 and falls back to a disk-only
    /// payload with `node_running:false` if the node is down).
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show escrow balances (confirmed + projected)
    Escrow,
    /// Print strategy auth token
    ShowToken,
    /// Print version
    Version,
    /// Deposit tokens into escrow
    Deposit {
        /// FA token metadata address (hex, 0x-prefixed)
        #[arg(long)]
        token: String,
        /// Amount in smallest units (e.g. 33000000 for 330 EMM at 5 decimals)
        #[arg(long)]
        amount: u64,
    },
    /// Reactivate an inactive node: try escrow::reactivate() first,
    /// fall back to claim_mint or request_mint if escrow is empty / past grace
    Reactivate,
    /// Force-apply pending param changes by calling auto_adjust_pools()
    ApplyParams,
    /// Update batch params on-chain (admin only)
    UpdateParams {
        #[arg(long, default_value_t = 20)]
        blocks_per_batch: u64,
        #[arg(long, default_value_t = 8)]
        commit_blocks: u64,
        #[arg(long, default_value_t = 6)]
        reveal_blocks: u64,
        #[arg(long, default_value_t = 2)]
        match_blocks: u64,
        #[arg(long, default_value_t = 4)]
        swap_blocks: u64,
        #[arg(long, default_value_t = 3)]
        commits_per_batch: u64,
        #[arg(long, default_value_t = 10)]
        settlement_max_age: u64,
        #[arg(long, default_value_t = 10)]
        rushed_grace_batches: u64,
    },
    /// MR3: SUPRA-only withdrawal flows. DMKT14: trustee-signed (the node's
    /// key IS the trustee); the off-chain payout address receives the SUPRA.
    Withdraw {
        #[command(subcommand)]
        action: WithdrawAction,
    },
    /// MR3: burn equal triples from escrow back to SUPRA.
    Burn {
        /// Burn target: SUPRA returns to escrow (top up gas) or to the
        /// payout address (profit takeout).
        #[arg(long, value_enum)]
        to: BurnTarget,
        /// Amount of each token (EMM/KAY/TEE) to burn, in raw units
        /// (5-decimal). The contract enforces equal triples.
        #[arg(long)]
        amount: u64,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        password_stdin: bool,
    },
    /// MR3: show or rotate the strategy bridge config so an external
    /// agent (Python bot, dashboard, LLM operator) can connect.
    AgentConfig {
        /// Generate a new strategy auth token and rewrite config.json.
        /// Requires node restart to take effect.
        #[arg(long)]
        rotate_token: bool,
        #[arg(long)]
        json: bool,
    },
    /// Burn-exit (DMKT14): individual NFT burn-exit, single trustee-signed
    /// call.
    ///
    /// Actions:
    ///   - execute: trustee destroys the NFT via exits::burn_trustee_nft;
    ///     sponsor gets the time-decayed refund, the payout address gets
    ///     leftover-token-burn-to-SUPRA
    ///   - preview: read-only view of what execute would pay now
    ///   - request: removed in DMKT14 (no separate request step); kept for
    ///     a clear error pointing operators at `execute`
    BurnPair {
        #[command(subcommand)]
        action: BurnPairAction,
    },
}

/// MR3: withdrawal sub-actions. SUPRA-only by design -- the per-token
/// wallet paths (`execute_rushed_withdrawal`, per-token `claim_all`)
/// are being removed in DMKT13 (contract item C-NO-PT-WD).
///
/// Each variant carries its own `--json` and `--password-stdin` flags
/// so the usual `cmd subcmd --flag` invocation works (clap parses
/// flags only against the current subcommand level).
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum WithdrawAction {
    /// Execute a pending rushed withdrawal as SUPRA. Requires a prior
    /// `request-rushed` + elapsed `rushed_grace_batches`.
    Rushed {
        #[arg(long)] json: bool,
        #[arg(long)] password_stdin: bool,
    },
    /// Start the rushed grace clock. After it elapses, run `rushed` to
    /// actually exit. Operator must have `rushed_withdrawal_enabled`.
    RequestRushed {
        #[arg(long)] json: bool,
        #[arg(long)] password_stdin: bool,
    },
    /// Cancel a pending rushed-withdrawal request.
    CancelRushed {
        #[arg(long)] json: bool,
        #[arg(long)] password_stdin: bool,
    },
    /// Execute `claim_all_as_supra` -- end-of-life exit. Requires a
    /// prior `start-holding` + elapsed `holding_period_days`.
    ClaimAll {
        #[arg(long)] json: bool,
        #[arg(long)] password_stdin: bool,
    },
    /// Begin the end-of-life holding period.
    StartHolding {
        #[arg(long)] json: bool,
        #[arg(long)] password_stdin: bool,
    },
    /// Cancel an active holding period and return to trading state.
    CancelHolding {
        #[arg(long)] json: bool,
        #[arg(long)] password_stdin: bool,
    },
}

impl WithdrawAction {
    pub fn json(&self) -> bool {
        match self {
            Self::Rushed { json, .. }
            | Self::RequestRushed { json, .. }
            | Self::CancelRushed { json, .. }
            | Self::ClaimAll { json, .. }
            | Self::StartHolding { json, .. }
            | Self::CancelHolding { json, .. } => *json,
        }
    }
    pub fn password_stdin(&self) -> bool {
        match self {
            Self::Rushed { password_stdin, .. }
            | Self::RequestRushed { password_stdin, .. }
            | Self::CancelRushed { password_stdin, .. }
            | Self::ClaimAll { password_stdin, .. }
            | Self::StartHolding { password_stdin, .. }
            | Self::CancelHolding { password_stdin, .. } => *password_stdin,
        }
    }
}

/// MR3: target for `burn` -- where the SUPRA produced by burning the
/// equal triple ends up.
#[derive(clap::ValueEnum, Debug, Clone, PartialEq)]
pub enum BurnTarget {
    /// SUPRA returns to the trustee's escrow. Use to top up gas without
    /// withdrawing.
    Escrow,
    /// SUPRA flows to the off-chain payout address. Profit takeout.
    /// DMKT14: was `beneficiary`; calls tokens::burn_for_profit.
    Payout,
}

/// Burn-exit (DMKT13) subcommands.
#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum BurnPairAction {
    /// DMKT14: removed. The burn-exit no longer has a separate request
    /// step; emits a clear error pointing operators at `execute`. Kept as a
    /// CLI variant for backwards-compatible invocation.
    Request {
        #[arg(long)] json: bool,
        #[arg(long)] password_stdin: bool,
    },
    /// Trustee-signed exits::burn_trustee_nft(nft_id, recipient). Sponsor
    /// receives the time-decayed refund; the payout address receives
    /// leftover-token-burn-to-SUPRA. NFT destroyed.
    Execute {
        #[arg(long)] json: bool,
        #[arg(long)] password_stdin: bool,
    },
    /// Read-only view of what `execute` would pay right now.
    /// Reflects bootstrap lockout (returns 0 if locked) + carve-out
    /// (returns treasury_balance if N==1) + time-decay + pro-rata cap.
    Preview {
        #[arg(long)] json: bool,
    },
}

impl BurnPairAction {
    pub fn json(&self) -> bool {
        match self {
            Self::Request { json, .. }
            | Self::Execute { json, .. }
            | Self::Preview { json, .. } => *json,
        }
    }
    pub fn password_stdin(&self) -> bool {
        match self {
            Self::Request { password_stdin, .. }
            | Self::Execute { password_stdin, .. } => *password_stdin,
            Self::Preview { .. } => false,
        }
    }
}

impl Cli {
    /// Parse CLI args. No subcommand → Run.
    pub fn resolve_command(&self) -> Command {
        self.command.clone().unwrap_or(Command::Run)
    }
}

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Command {
        let cli = Cli::parse_from(args);
        cli.resolve_command()
    }

    // T_CLI_01: status subcommand (no flag)
    #[test]
    fn t_cli_01_status() {
        assert_eq!(parse(&["deadmkt-node", "status"]), Command::Status { json: false });
    }

    // T_CLI_01b: status --json subcommand
    #[test]
    fn t_cli_01b_status_json() {
        assert_eq!(parse(&["deadmkt-node", "status", "--json"]), Command::Status { json: true });
    }

    // T_CLI_02: escrow subcommand
    #[test]
    fn t_cli_02_escrow() {
        assert_eq!(parse(&["deadmkt-node", "escrow"]), Command::Escrow);
    }

    // T_CLI_03: show-token subcommand
    #[test]
    fn t_cli_03_show_token() {
        assert_eq!(parse(&["deadmkt-node", "show-token"]), Command::ShowToken);
    }

    // T_CLI_04: no subcommand → Run
    #[test]
    fn t_cli_04_default_run() {
        assert_eq!(parse(&["deadmkt-node"]), Command::Run);
    }

    // T_CLI_05: version subcommand
    #[test]
    fn t_cli_05_version() {
        assert_eq!(parse(&["deadmkt-node", "version"]), Command::Version);
    }

    // Extra: setup subcommand
    #[test]
    fn test_setup() {
        assert_eq!(parse(&["deadmkt-node", "setup"]), Command::Setup);
    }

    // ---- MR3: action commands ----

    #[test]
    fn t_mr3_cli_01_burn_to_escrow() {
        let cmd = parse(&["deadmkt-node", "burn", "--to", "escrow", "--amount", "1000", "--json"]);
        match cmd {
            Command::Burn { to, amount, json, password_stdin } => {
                assert_eq!(to, BurnTarget::Escrow);
                assert_eq!(amount, 1000);
                assert!(json);
                assert!(!password_stdin);
            }
            other => panic!("expected Burn, got {:?}", other),
        }
    }

    #[test]
    fn t_mr3_cli_02_burn_to_payout_with_password_stdin() {
        let cmd = parse(&[
            "deadmkt-node", "burn",
            "--to", "payout",
            "--amount", "5000",
            "--password-stdin",
        ]);
        match cmd {
            Command::Burn { to, amount, json, password_stdin } => {
                assert_eq!(to, BurnTarget::Payout);
                assert_eq!(amount, 5000);
                assert!(!json);
                assert!(password_stdin);
            }
            other => panic!("expected Burn, got {:?}", other),
        }
    }

    #[test]
    fn t_mr3_cli_03_withdraw_rushed_with_json() {
        let cmd = parse(&["deadmkt-node", "withdraw", "rushed", "--json"]);
        match cmd {
            Command::Withdraw { action } => {
                assert!(matches!(action, WithdrawAction::Rushed { .. }));
                assert!(action.json());
                assert!(!action.password_stdin());
            }
            other => panic!("expected Withdraw, got {:?}", other),
        }
    }

    #[test]
    fn t_mr3_cli_04_withdraw_subcommand_variants() {
        // All six sub-actions parse with --json after the subcommand.
        for arg in ["rushed", "request-rushed", "cancel-rushed",
                    "claim-all", "start-holding", "cancel-holding"] {
            let cmd = parse(&["deadmkt-node", "withdraw", arg, "--json"]);
            match cmd {
                Command::Withdraw { action } => {
                    assert!(action.json(), "json missing for {}", arg);
                }
                other => panic!("expected Withdraw, got {:?} (arg={})", other, arg),
            }
        }
    }

    #[test]
    fn t_mr3_cli_07_withdraw_password_stdin() {
        let cmd = parse(&["deadmkt-node", "withdraw", "rushed", "--password-stdin"]);
        match cmd {
            Command::Withdraw { action } => {
                assert!(!action.json());
                assert!(action.password_stdin());
            }
            other => panic!("expected Withdraw, got {:?}", other),
        }
    }

    #[test]
    fn t_mr3_cli_05_agent_config_default() {
        let cmd = parse(&["deadmkt-node", "agent-config", "--json"]);
        match cmd {
            Command::AgentConfig { rotate_token, json } => {
                assert!(!rotate_token);
                assert!(json);
            }
            other => panic!("expected AgentConfig, got {:?}", other),
        }
    }

    #[test]
    fn t_mr3_cli_06_agent_config_rotate() {
        let cmd = parse(&["deadmkt-node", "agent-config", "--rotate-token"]);
        match cmd {
            Command::AgentConfig { rotate_token, json } => {
                assert!(rotate_token);
                assert!(!json);
            }
            other => panic!("expected AgentConfig, got {:?}", other),
        }
    }
}
