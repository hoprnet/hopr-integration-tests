//! Integration-test framework for the HOPR stack.
//!
//! Brings up a 3-node `hoprd-localcluster` (anvil + blokli + 3 `hoprd` processes,
//! full-mesh channels — contracts deployed by the chain container, see
//! [`cluster`]) and an `edgli` edge client into a shared
//! [`IntegrationEnv`](env::IntegrationEnv), then pumps a payload through a UDP
//! session to the exit node's built-in loopback and measures goodput + loss.
//!
//! One cluster serves every test in a binary (see [`cluster`]); a test that runs several
//! phases names each with [`step`], so a failure says which phase broke.
//!
//! ## Modes
//! - **Managed** (default): set `HOPRD_LOCALCLUSTER_BIN`, `HOPRD_BIN` and
//!   `HOPRD_CHAIN_URL` (a chain from `scripts/integration/lib.sh chain_up`).
//! - **External**: set `HOPRD_CLUSTER_DATA_DIR` (+ `HOPRD_LOCALCLUSTER_BIN`).

pub mod cluster;
pub mod env;
pub mod origination;
// Ungated on purpose, though only `tests/pix.rs` drives it: the balance and counter readers are
// plain parsers, and gating them would keep the subtlest logic in this crate — absent-vs-zero,
// whole-multiple reconciliation — out of the v4 `cargo test --lib` that CI runs. Only the parts
// naming edgli's PIX types are `#[cfg(feature = "v5")]`.
pub mod pix;
// Ungated for the same reason `pix` is, and with more at stake: this is the reader that tells a
// parked egress gate apart from a starved one, and its subtlety is all in the parsing — cumulative
// histogram buckets, a numeric `le`, and absent-versus-zero on a label that only exists once the
// gate has failed. Those tests belong in the default `cargo test --lib`.
pub mod pix_exit;
pub mod pump;
pub mod relayers;
pub mod session_metrics;
// Ungated for the same reason `pix` is: the profile is arithmetic over constants, and its
// compile-time guards and unit tests are what catch a geometry edited without re-deriving what
// depends on it. Those should run in the default `cargo test --lib`, on both lines.
pub mod shapes;
pub mod udp_service;

/// Payload size pumped through each session. Also sizes the strategy's expected
/// packet count for channel funding (see [`env`]).
pub const PAYLOAD_BYTES: usize = 10 * 1024 * 1024; // 10 MiB

pub use env::IntegrationEnv;

/// Runs one named phase of a test. A panic or error inside it fails the test with the step's
/// name, and with `IT_STEP_LOG` set the outcome is appended there for the end-of-run summary.
pub async fn step<T>(
    name: &str,
    body: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    use anyhow::Context as _;
    use futures::FutureExt as _;

    tracing::info!(step = name, "step started");
    let result = match std::panic::AssertUnwindSafe(body).catch_unwind().await {
        Ok(result) => result,
        Err(panic) => Err(anyhow::anyhow!(
            panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "panicked".into())
        )),
    };
    match &result {
        Ok(_) => tracing::info!(step = name, "step passed"),
        Err(e) => tracing::error!(step = name, "step failed: {e:#}"),
    }
    record_step(name, result.as_ref().err());
    result.with_context(|| format!("step `{name}` failed"))
}

fn record_step(name: &str, error: Option<&anyhow::Error>) {
    use std::io::Write as _;

    let Some(path) = std::env::var_os("IT_STEP_LOG") else {
        return;
    };
    let test = std::thread::current()
        .name()
        .unwrap_or("unknown")
        .to_string();
    let (status, error) = match error {
        None => ("passed", String::new()),
        Some(e) => (
            "failed",
            format!("{e:#}")
                .replace(['\t', '\n', '\r'], " ")
                .chars()
                .take(300)
                .collect(),
        ),
    };
    let line = format!("{test}\t{name}\t{status}\t{error}\n");
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = written {
        tracing::warn!(%e, "could not record the step outcome");
    }
}

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

/// What a Session connects to at the far end, re-exported for the same reason as [`HoprSession`].
///
/// `ExitNode(0)` is the Exit's built-in loopback and is what most scenarios want. The `UdpStream`
/// and `TcpStream` variants point at an ordinary socket on the Exit's host, which is how a shape
/// gets traffic that is *not* symmetric — see `env::IntegrationEnv::open_pix_session_with`.
pub use edgli::hopr_lib::exports::transport::SessionTarget;

#[cfg(test)]
mod tests {
    use super::step;

    #[tokio::test]
    async fn a_failing_step_names_itself() {
        let err = step("phase one", async {
            anyhow::bail!("broke") as anyhow::Result<()>
        })
        .await
        .unwrap_err();
        assert_eq!("step `phase one` failed: broke", format!("{err:#}"));
    }

    #[tokio::test]
    async fn a_panicking_step_becomes_an_error() {
        let err = step("phase two", async {
            let arrival = std::hint::black_box(40);
            assert!(arrival > 50, "only {arrival}% came back");
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(
            "step `phase two` failed: only 40% came back",
            format!("{err:#}")
        );
    }

    #[tokio::test]
    async fn a_passing_step_returns_its_value() {
        assert_eq!(7, step("phase three", async { Ok(7) }).await.unwrap());
    }

    #[test]
    fn the_step_log_carries_the_test_name() {
        let log = tempfile::NamedTempFile::new().unwrap();
        // SAFETY: nothing else in this crate reads IT_STEP_LOG; concurrent step tests may append
        // their own lines, so only this test's line is checked.
        unsafe { std::env::set_var("IT_STEP_LOG", log.path()) };
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(step("logged", async {
                anyhow::bail!("x") as anyhow::Result<()>
            }))
            .unwrap_err();
        unsafe { std::env::remove_var("IT_STEP_LOG") };
        let log = std::fs::read_to_string(log.path()).unwrap();
        assert!(
            log.lines()
                .any(|l| l == "tests::the_step_log_carries_the_test_name\tlogged\tfailed\tx"),
            "{log}"
        );
    }
}
