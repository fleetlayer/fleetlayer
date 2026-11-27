//! fleetlayer-claude-accounts: every Claude account the hub manages, with its usage, as claude-swap's
//! `--list --json` (schema v1), for CodexBar's "Read accounts from claude-swap".

fn main() {
    fleetlayer_accounts::main(&fleetlayer_accounts::CLAUDE);
}
