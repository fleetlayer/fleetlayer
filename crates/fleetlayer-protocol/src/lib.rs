//! Fleetlayer's wire types. `docs/protocol.md` is the specification; these types follow it.
//!
//! - [`rpc`]: JSON-RPC 2.0 messages, one per line.
//! - [`acp`]: the part of the Agent Client Protocol (v1) Fleetlayer uses.
//! - [`ext`]: Fleetlayer's extensions: method names, `_meta` keys and their payloads.
//! - [`link`]: the node's link to the control plane.

pub mod acp;
pub mod ext;
pub mod link;
pub mod rpc;

/// The version of Fleetlayer's extensions this crate speaks (`protocol` in `fleetlayer.dev/node` and
/// `fleetlayer.dev/plugin`).
pub const PROTOCOL: u32 = 1;

/// The ACP major version Fleetlayer targets.
pub const ACP_PROTOCOL: u16 = 1;
