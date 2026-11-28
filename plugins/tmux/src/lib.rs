//! fleetlayer-tmux: the Fleetlayer adapter for tmux. It takes the session-manager role (`docs/protocol.md` section
//! 4): lists the Claude Code, Codex and other agents running in tmux panes as sessions, with their status (Claude
//! Code's own session registry, else the screen), opens and closes them in windows of their own, interrupts them,
//! types messages in when an agent has no channel of its own, answers prompts with keys, and streams panes for
//! `fleetlayer attach`. It never attaches a tmux client or changes a layout: the person's own tmux works as before.
//!
//! A session's address is its pane ID without the `%` (`p12`).
//!
//! Config (its section of the node's config, all optional): `program` (default `tmux`), `socket` (`tmux -L`) or
//! `socket_path` (`tmux -S`), `session` (where new agents open, default `fleetlayer`), `shell` (the login shell they
//! start through, default `$SHELL`), `claude_dir` (default `CLAUDE_CONFIG_DIR` or `~/.claude`).

pub mod plugin;
pub mod terminal;
pub mod tmux;
