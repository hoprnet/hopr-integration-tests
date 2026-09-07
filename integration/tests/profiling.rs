//! Executor-starvation profiling harness for the `edgli` data path.
//!
//! Captures tokio-console + Perfetto (Chrome) traces contrasting a **healthy paced pump**
//! with an **executor-starving continuous pump**, to diagnose the throughput collapse a
//! non-yielding `write_all` causes: the writer holds a tokio worker thread without
//! returning `Poll::Pending`, starving the SURB balancer and ack-processing tasks so the
//! session's return path stalls (see `pump::pump_continuous`).
//!
//! The whole file is gated behind `--features prof`; with the feature off it compiles to
//! an empty (zero-test) binary, so the default CI build never pulls in the profiling deps.
//!
//! # Requirements (all three must hold or tokio-console sees nothing)
//!
//! 1. `RUSTFLAGS="--cfg tokio_unstable --check-cfg cfg(tokio_unstable)"` — enables tokio's
//!    task instrumentation at compile time. (hopr-integration-tests has no base rustflags to clobber,
//!    so setting `RUSTFLAGS` here is safe — unlike edge-client, which relies on a
//!    `.cargo/config.toml`.)
//! 2. `--profile tracer` — inherits `release` (no stack overflow) but re-enables
//!    `debug-assertions`, which lifts `tracing`'s `release_max_level_debug` cap so TRACE
//!    callsites (incl. tokio's task spans) stay compiled in.
//! 3. `--features prof` — pulls in `console-subscriber` + `tracing-chrome`.
//!
//! # Running
//!
//! Use the script (handles env, build, and trace collection):
//!
//! ```bash
//! ./scripts/profile-executor-yield.sh          # local cluster (paced + continuous)
//! ./scripts/profile-executor-yield.sh --rotsee-only   # Rotsee continuous (needs EDGLI_ROTSEE_*)
//! ```
//!
//! Each test writes a Chrome trace to `$EDGLI_TRACE_DIR/` — load at <https://ui.perfetto.dev>.

#![cfg(feature = "prof")]

use std::time::Duration;

use hoprd_integration_test::{IntegrationEnv, PAYLOAD_BYTES, pump};
use rand::RngExt as _;

/// Per-pump transfer timeout. A stall in the continuous case is logged, not failed.
const PUMP_TIMEOUT: Duration = Duration::from_secs(600);
/// Hop counts exercised for the local traces.
const HOPS: [usize; 2] = [0, 1];

/// Install tokio-console (live gRPC TUI) + a tracing-chrome layer writing to
/// `$EDGLI_TRACE_DIR/<filename>` (or `./<filename>`). Returns the chrome flush guard —
/// keep it alive for the duration of the test. Uses `try_init` so it is a no-op if a
/// subscriber is already set (multiple tests may share a process).
fn init_subscriber(filename: &str) -> tracing_chrome::FlushGuard {
    use tracing_subscriber::prelude::*;

    let dir = std::env::var("EDGLI_TRACE_DIR").unwrap_or_else(|_| ".".to_string());
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|e| panic!("cannot create EDGLI_TRACE_DIR {dir}: {e}"));
    let path = format!("{dir}/{filename}");

    let (chrome_layer, guard) = tracing_chrome::ChromeLayerBuilder::new()
        .file(&path)
        .include_args(true)
        .build();

    // If a global subscriber is already installed (e.g. all three tests share one process
    // under `cargo test`), this test's chrome layer is NOT installed and its trace file would
    // silently stay empty. Report that instead of hiding it — the profiling script sidesteps
    // it by running one test per process.
    if tracing_subscriber::registry()
        .with(console_subscriber::spawn())
        .with(chrome_layer)
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .is_err()
    {
        eprintln!(
            "WARNING: a global tracing subscriber is already set — {path} will be EMPTY. Run one \
             profiling test per process (see scripts/profile-executor-yield.sh)."
        );
    } else {
        eprintln!("Chrome trace → {path}");
    }
    guard
}

fn random_payload() -> Vec<u8> {
    let mut payload = vec![0u8; PAYLOAD_BYTES];
    rand::rng().fill(&mut payload[..]);
    payload
}

/// Paced baseline over the local cluster: healthy task interleaving.
///
/// Writes `edgli-trace-paced.json`. The pump paces itself when `HOPRD_PUMP_MBPS` is set
/// (the profiling script sets it); in Perfetto, tasks interleave freely between batches
/// and the SURB balancer wakes regularly. Compare against `continuous_pump`.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn paced_pump_baseline() -> anyhow::Result<()> {
    let _guard = init_subscriber("edgli-trace-paced.json");
    let env = IntegrationEnv::setup().await?;
    let payload = random_payload();
    for hops in HOPS {
        let session = env.open_unreliable_session(hops).await?;
        pump::pump_loopback(
            session,
            &payload,
            &format!("paced {hops}-hop"),
            PUMP_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}

/// Continuous (non-yielding) pump over the local cluster: the starvation case.
///
/// Writes `edgli-trace-continuous.json`. A single `write_all` holds a tokio worker
/// thread; in Perfetto the writer task shows one very long poll while the SURB balancer
/// sits idle, and goodput collapses relative to the paced baseline.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn continuous_pump() -> anyhow::Result<()> {
    let _guard = init_subscriber("edgli-trace-continuous.json");
    let env = IntegrationEnv::setup().await?;
    let payload = random_payload();
    for hops in HOPS {
        let session = env.open_profiling_session(hops).await?;
        pump::pump_continuous(
            session,
            &payload,
            &format!("continuous {hops}-hop"),
            PUMP_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}

/// Continuous-pump starvation case against the public Rotsee testnet.
///
/// Writes `edgli-trace-rotsee.json`. Requires the `EDGLI_ROTSEE_*` env vars (see
/// `tests/rotsee.rs`).
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn continuous_pump_rotsee() -> anyhow::Result<()> {
    let _guard = init_subscriber("edgli-trace-rotsee.json");
    let env = IntegrationEnv::setup_rotsee().await?;
    let payload = random_payload();
    for hops in HOPS {
        let session = env.open_profiling_session(hops).await?;
        pump::pump_continuous(
            session,
            &payload,
            &format!("continuous rotsee {hops}-hop"),
            PUMP_TIMEOUT,
        )
        .await?;
    }
    Ok(())
}
