// =========================================================================
// cli.rs: CLI subcommands (clap)
// =========================================================================

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "deadmkt-node", version = env!("CARGO_PKG_VERSION"), about = "deadmkt trading node")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone, PartialEq)]
pub enum Command {
    /// Run the node (default when no subcommand given)
    Run,
    /// Run first-boot setup wizard
    Setup,
    /// Show node state, batch, balance summary
    Status,
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
    /// Reactivate an inactive node by forcing a minimum mint
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

    // T_CLI_01: status subcommand
    #[test]
    fn t_cli_01_status() {
        assert_eq!(parse(&["deadmkt-node", "status"]), Command::Status);
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
}
