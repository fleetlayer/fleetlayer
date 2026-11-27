//! The parts of Fleetlayer's secrets layer that the plugins delivering secrets, the node and the `fleetlayer` command
//! share (`docs/secrets.md`):
//!
//! - [`registry`]: each plugin that writes delivered secrets on a node (fleetlayer-1password, fleetlayer-cookies,
//!   claude-auth-manager) also lists them, value and reference, in one private folder. The node reads it to redact
//!   them from everything that leaves the machine; `fleetlayer secrets run` reads it to fill in `op://` references.
//! - [`scrub`]: exact-match redaction of those values, and of their encoded forms, as `[REDACTED <reference>]`.
//! - [`pin`]: which node a plugin takes a secret bundle from, so no other node can send one of the same name.
//! - [`write`]: files written mode 0600 in one rename, and never over a file a person made.

pub mod pin;
pub mod registry;
pub mod scrub;
pub mod write;

pub use registry::Secret;
pub use scrub::{Hit, Scrubber};
