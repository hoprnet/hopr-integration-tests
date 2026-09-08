//! Integration-test framework for the HOPR stack.
//!
//! Brings up a 3-node `hoprd-localcluster` (anvil + blokli + 3 `hoprd` processes,
//! full-mesh channels — contracts deployed by the chain container, see
//! [`cluster`]) and an `edgli` edge client into a shared
//! [`IntegrationEnv`](env::IntegrationEnv), then pumps a payload through a UDP
//! session to the exit node's built-in loopback and measures goodput + loss.
//!
//! Each hop count is its own `#[test]` (see `tests/integration.rs`) so 0-hop and
//! 1-hop are reported independently; every test owns its cluster (bring up →
//! run → tear down). There are no tuning knobs — thresholds are hardcoded in the
//! test.
//!
//! ## Modes
//! - **Managed** (default): set `HOPRD_LOCALCLUSTER_BIN`, `HOPRD_BIN`,
//!   `HOPRD_CHAIN_IMAGE` (a `bloklid-anvil` image), optional
//!   `HOPRD_CONTAINER_RUNTIME` (default `docker`).
//! - **External**: set `HOPRD_CLUSTER_DATA_DIR` (+ `HOPRD_LOCALCLUSTER_BIN`).

pub mod cluster;
pub mod env;
pub mod origination;
// Ungated on purpose, though only `tests/pix.rs` drives it: the balance and counter readers are
// plain parsers, and gating them would keep the subtlest logic in this crate — absent-vs-zero,
// whole-multiple reconciliation — out of the default `cargo test --lib` that CI runs. Only the
// parts naming edgli's PIX types are `#[cfg(feature = "pix")]`.
pub mod pix;
pub mod pump;
pub mod relayers;
pub mod session_metrics;

/// Payload size pumped through each session. Also sizes the strategy's expected
/// packet count for channel funding (see [`env`]).
pub const PAYLOAD_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

pub use env::IntegrationEnv;

/// On-chain address type, re-exported so submodules share one definition.
pub use edgli::hopr_lib::api::types::primitive::prelude::Address;

/// wxHOPR balance type, re-exported for the same reason as [`Address`].
///
/// This path rather than `hopr_lib::HoprBalance`: the latter is a private re-export, so naming it
/// compiles inside `hopr-lib` and not here.
pub use edgli::hopr_lib::api::types::primitive::prelude::HoprBalance;

/// The session type scenarios operate on, re-exported to spare callers the path through
/// Edgli's re-export chain.
pub use edgli::hopr_lib::exports::transport::HoprSession;
