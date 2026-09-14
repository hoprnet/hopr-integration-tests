//! Does a sustained upload keep its return path alive?
//!
//! # The incident this reproduces
//!
//! A `gnosis_vpn-client` session (VPN traffic over a HOPR session, an unreliable UDP-like socket)
//! reproducibly loses its return path a few minutes into a sustained **upload** — ~+405 s at
//! 3 Mbit/s, ~+250 s at 6 Mbit/s — while downloads and short transfers are unaffected. Data keeps
//! flowing up; only the exit's replies stop arriving, until the tunnel watchdog tears it down.
//!
//! # Why uploads and not downloads
//!
//! The initiator (edgli) mints SURBs so the exit can reply, and keeps one **reply opener** per
//! SURB. With `always_max_out_surbs` (the production WG session mirrored in [`env`]) every uplink
//! data packet carries SURBs regardless of the balancer's target — organic production the PID
//! controller does not throttle. On an upload the exit barely replies, so it consumes almost none
//! of them: the opener cache fills and overflows. The overflow evicts the **newest** openers (the
//! per-pseudonym opener cache is a moka cache at the default TinyLFU policy, and never-read openers
//! all tie at frequency zero, so incumbents win and fresh entries are dropped). The exit's SURB
//! ring, meanwhile, always holds the newest SURBs — exactly the openers just evicted — so every
//! reply it sends becomes undecryptable and the return stream dies. A download consumes SURBs at
//! the data rate, so the openers are used before they can pile up, and the cache never overflows.
//!
//! The exit-side pop order (`SurbPopOrder`, LIFO in production) does not save this: LIFO replies
//! with the freshest SURB, whose opener is the most likely to have just been evicted. That is a
//! different failure from the return-path *staleness* LIFO was introduced to fix (see
//! `return_path.rs`). The mechanism (and its fix) is proven deterministically, in isolation, by the
//! hoprnet `protocols/hopr/src/surb_store.rs` unit tests
//! (`a_sustained_upload_keeps_the_return_path_alive_at_the_deployed_config` and the retention probe
//! `a_sustained_upload_keeps_the_newest_reply_openers_and_sheds_the_stalest`); this test is the
//! end-to-end counterpart over real `hoprd` nodes.
//!
//! # Reading a result
//!
//! A **pass** means the return path survived the whole upload (the fix works). A **failure** —
//! a low arrival percentage and/or a long return-stream stall — is the bug reproduced: the opener
//! cache overflowed and the exit's replies stopped opening.
//!
//! The reply-opener cache is shrunk via [`request_edgli_max_openers`] so its overflow lands in
//! seconds rather than the ~291 s it takes at the production cap of 100 000; the failure mode is
//! the overflow itself, not the particular capacity.
//!
//! `#[ignore]` like the other cross-repo tests: needs the external binaries and a chain. Run with:
//!
//! ```bash
//! HOPRD_KEEP_ARTIFACTS=1 HOPRD_BIN=<hoprd> HOPRD_LOCALCLUSTER_BIN=<localcluster> \
//! TEST_TARGET=upload_survival TEST_ARGS=--nocapture \
//!   bash scripts/integration/run-binchain.sh > /tmp/run.log 2>&1
//! ```

use std::time::Duration;

use hoprd_integration_test::{
    IntegrationEnv,
    cluster::{EDGLI_MAX_OPENERS_FLOOR, request_edgli_max_openers},
    pump::{PumpOpts, pace_for_rate, pump_halves},
};
use rand::RngExt as _;

/// Reply-opener cap for the run: the smallest cache edgli will install, so its overflow lands
/// within about a second under load and the rest of the upload measures the return path with the
/// cache already overflowed — the state the incident hit after minutes at the production cap.
const SHRUNK_MAX_OPENERS: usize = EDGLI_MAX_OPENERS_FLOOR;

/// A sustained upload rate (MB/s), not a burst — the failure is about a stream held open long
/// enough to overflow the opener cache, not about peak throughput.
const TARGET_MBPS: f64 = 1.0;

/// How long to keep offering uplink. Comfortably longer than the ~1 s the shrunk cache takes to
/// overflow, so a collapsed return path has time to show.
const UPLOAD_DURATION: Duration = Duration::from_secs(30);

const PUMP_TIMEOUT: Duration = Duration::from_secs(600);

/// The exit echoes the upload back, so on a healthy return path almost everything returns (UDP
/// loss aside). The bug collapses it once the cache overflows, so this floor separates the two.
const MIN_ARRIVAL_PCT: f64 = 90.0;

/// A healthy echo stream never goes quiet for long; a return path killed by opener eviction stalls
/// for the rest of the run. Bounds the "went silent" failure that arrival percentage alone can
/// miss on a stream that stops early.
const MAX_STALL: Duration = Duration::from_secs(10);

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a bloklid-anvil container"]
async fn sustained_upload_keeps_the_return_path_alive() -> anyhow::Result<()> {
    // Must be requested before the cluster/edgli boot — it is read while edgli's config is built.
    request_edgli_max_openers(SHRUNK_MAX_OPENERS);

    let env = IntegrationEnv::setup().await?;
    // 0-hop forward and return: the opener-eviction mechanism is hop-independent, and a direct path
    // keeps the scenario about the SURB stores rather than relay selection.
    let (session, _exit) = env.open_unreliable_session_paths(0, 0).await?;

    let bytes = (TARGET_MBPS * 1_000_000.0 * UPLOAD_DURATION.as_secs_f64()) as usize;
    // Random bytes → unique packet ciphertexts per run (avoids replay-tag hits).
    let mut payload = vec![0u8; bytes];
    rand::rng().fill(&mut payload[..]);

    let (mut rx, mut tx) = tokio::io::split(session);
    let transfer = pump_halves(
        &mut rx,
        &mut tx,
        &payload,
        "upload",
        PUMP_TIMEOUT,
        PumpOpts {
            pace: pace_for_rate(TARGET_MBPS),
            // Long enough that a return path which merely slowed would not read as dead, but the
            // pump still ends within a reasonable window once the stream truly goes quiet.
            idle_budget: Some(Duration::from_secs(30)),
            tail_grace: Some(Duration::from_secs(30)),
            ..PumpOpts::default()
        },
    )
    .await?;

    assert!(
        transfer.arrival_pct() >= MIN_ARRIVAL_PCT,
        "return path collapsed under sustained upload: only {:.1}% echoed back (need ≥{MIN_ARRIVAL_PCT:.0}%); \
         outcome={:?}, longest stall {:.1}s — the exit's replies stopped opening once edgli's reply-opener \
         cache overflowed and shed the newest openers",
        transfer.arrival_pct(),
        transfer.outcome,
        transfer.longest_stall(),
    );
    assert!(
        transfer.longest_stall() <= MAX_STALL.as_secs_f64(),
        "return stream stalled {:.1}s (max {:?}): reply openers for the freshest SURBs were evicted",
        transfer.longest_stall(),
        MAX_STALL,
    );
    // Loss and corruption are distinct: UDP may drop bytes, but bytes that do return must be right.
    assert!(
        transfer.received_bytes < transfer.sent_bytes || transfer.sha_ok,
        "full payload returned but corrupted (SHA-256 mismatch)",
    );
    Ok(())
}
