//! A PIX deposit that fails during an outage must be retried while its Session lives
//! (gnosis/gnosis_vpn#843).
//!
//! The outage: the Entry's Safe is emptied on anvil before the Session opens, so the pool refuses
//! the first deposit, and refilled [`OUTAGE`] later. The Exit runs the deployed `max_deposit_wait`
//! of 3 min, so the Session is still payable once the Safe is back. Passing means the deposit was
//! retried, landed, and the same Session went on to complete paid cycles, with no reconnect.
//!
//! ```bash
//! LINE=v5 HOPR_STRATEGY_SRC=../hopr-strategy just pix-deposit-outage
//! ```
#![cfg(feature = "v5")]

use std::time::Duration;

use anyhow::Context as _;
use hoprd_integration_test::{
    Address, HoprBalance, IntegrationEnv,
    cluster::{self, NodeInfo, request_cluster_size},
    pix::{self, PixCounters},
    pump::{PumpOpts, pump_halves},
};
use rand::RngExt as _;
use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const NODES: usize = 3;
const HOPS: usize = 1;

/// How long the Entry cannot pay; the issue's acceptance criterion.
const OUTAGE: Duration = Duration::from_secs(120);

/// The deployed Exit's deadline (`gnosis_vpn-infrastructure` piz-palu vars), plus the localcluster's
/// 20 s `max_ssa_delivery_time` the Exit adds to it. The Exit's pool watches the deposit for the
/// whole deadline, as deployed; the localcluster default of 30 s closes the Session first.
const EXIT_DEPOSIT_WAIT: Duration = Duration::from_secs(180);
const EXIT_DEADLINE: Duration = Duration::from_secs(200);

/// Paid cycles the Session must complete after the outage.
const TARGET_CYCLES: u64 = 2;
const BUDGETED_CYCLES: u64 = 20;

const SEND_CHUNK: usize = 512;
const SEND_INTERVAL: Duration = Duration::from_millis(400);
const REPLY_MARGIN: u64 = 2;
const EMISSIONS_PER_SSA: u64 =
    (pix::PIX_POLYS * (pix::PIX_SHARES + pix::PIX_ADDITIONAL_SHARES)) as u64;

/// Forward traffic during the outage, so the Exit never sees an idle Session.
const TRICKLE_INTERVAL: Duration = Duration::from_secs(5);

const PUMP_TIMEOUT: Duration = Duration::from_secs(600);
const SETTLE_TIMEOUT: Duration = Duration::from_secs(180);
const SETTLE_POLL: Duration = Duration::from_secs(3);
const INDEXING_TIMEOUT: Duration = Duration::from_secs(60);

/// Where the Safe's wxHOPR is parked during the outage.
const SINK: &str = "0x000000000000000000000000000000000000dEaD";

fn node_for(env: &IntegrationEnv, address: Address) -> anyhow::Result<NodeInfo> {
    env.cluster()?
        .nodes
        .iter()
        .find(|n| n.address == address)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no cluster node at {address}"))
}

async fn anvil_rpc(method: &str, params: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let port = std::env::var("ANVIL_PORT").unwrap_or_else(|_| "8545".to_owned());
    let body: serde_json::Value = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .await
        .with_context(|| format!("anvil {method}"))?
        .json()
        .await?;
    if let Some(error) = body.get("error") {
        anyhow::bail!("anvil {method}: {error}");
    }
    Ok(body["result"].clone())
}

/// The wxHOPR token, from the contract addresses `lib.sh chain_up` appends to bloklid's config.
fn token_address() -> anyhow::Result<String> {
    let dir = std::env::var("CHAIN_DATA_DIR").unwrap_or_else(|_| "/tmp/hopr-chain".to_owned());
    let config = std::fs::read_to_string(format!("{dir}/bloklid-config.toml"))
        .with_context(|| format!("reading {dir}/bloklid-config.toml"))?;
    config
        .lines()
        .find_map(|line| {
            let value = line
                .trim()
                .strip_prefix("token")?
                .trim_start()
                .strip_prefix('=')?;
            Some(value.trim().trim_matches('"').to_owned())
        })
        .context("no `token = ...` in bloklid's config")
}

/// Moves every wxHOPR `from` holds to `to`, signing as `from` through anvil's impersonation.
async fn move_all_wxhopr(token: &str, from: &str, to: &str) -> anyhow::Result<()> {
    let word = |address: &str| format!("{:0>64}", address.trim_start_matches("0x"));
    let balance = anvil_rpc(
        "eth_call",
        json!([{ "to": token, "data": format!("0x70a08231{}", word(from)) }, "latest"]),
    )
    .await?;
    let balance = balance
        .as_str()
        .context("balanceOf result")?
        .trim_start_matches("0x")
        .to_owned();

    anvil_rpc("anvil_setBalance", json!([from, "0x56bc75e2d63100000"])).await?;
    anvil_rpc("anvil_impersonateAccount", json!([from])).await?;
    let transfer = format!("0xa9059cbb{}{:0>64}", word(to), balance);
    let tx = anvil_rpc(
        "eth_sendTransaction",
        json!([{ "from": from, "to": token, "data": transfer }]),
    )
    .await;
    anvil_rpc("anvil_stopImpersonatingAccount", json!([from])).await?;
    let tx = tx?;

    for _ in 0..30 {
        let receipt = anvil_rpc("eth_getTransactionReceipt", json!([tx])).await?;
        if !receipt.is_null() {
            anyhow::ensure!(
                receipt["status"] == "0x1",
                "wxHOPR transfer reverted: {receipt}"
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    anyhow::bail!("wxHOPR transfer {tx} was never mined")
}

/// Waits until the Entry's own view of its Safe (through blokli) satisfies `done`.
async fn await_entry_safe(
    env: &IntegrationEnv,
    what: &str,
    done: impl Fn(HoprBalance) -> bool,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + INDEXING_TIMEOUT;
    loop {
        let balance = env.entry_safe_balance().await?;
        if done(balance) {
            tracing::info!(%balance, "entry Safe {what}");
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "entry Safe never {what} (still {balance})"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Writes one chunk, then reads and discards replies for [`TRICKLE_INTERVAL`].
async fn trickle(
    rx: &mut (impl tokio::io::AsyncRead + Unpin),
    tx: &mut (impl tokio::io::AsyncWrite + Unpin),
) -> anyhow::Result<()> {
    tx.write_all(&[0u8; SEND_CHUNK]).await?;
    drain(rx, TRICKLE_INTERVAL).await
}

async fn drain(
    rx: &mut (impl tokio::io::AsyncRead + Unpin),
    period: Duration,
) -> anyhow::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let pause = tokio::time::sleep(period);
    tokio::pin!(pause);
    loop {
        tokio::select! {
            _ = &mut pause => return Ok(()),
            read = rx.read(&mut buf) => { read?; }
        }
    }
}

async fn await_sweeps(
    exit: &NodeInfo,
    before: &PixCounters,
    target: u64,
) -> anyhow::Result<PixCounters> {
    let deadline = tokio::time::Instant::now() + SETTLE_TIMEOUT;
    loop {
        let delta = before.delta(&pix::sample_exit(exit).await?);
        tracing::info!("exit PIX counters: {}", delta.summary());
        if delta.sweeps().unwrap_or(0) >= target || tokio::time::Instant::now() >= deadline {
            return Ok(delta);
        }
        tokio::time::sleep(SETTLE_POLL).await;
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires PIX-enabled hoprd/hoprd-localcluster binaries and a chain"]
async fn a_deposit_failed_during_an_outage_should_be_retried_and_pay_the_same_session()
-> anyhow::Result<()> {
    request_cluster_size(NODES);
    cluster::request_pix_settings(format!(
        "max_deposit_wait: {}s\nmax_deposit_tracking_time: {}s\n",
        EXIT_DEPOSIT_WAIT.as_secs(),
        EXIT_DEADLINE.as_secs()
    ));
    let t0 = std::time::Instant::now();

    let per_cycle = pix::per_cycle()?;
    let env = IntegrationEnv::setup_pix(per_cycle * BUDGETED_CYCLES).await?;
    let token = token_address()?;
    let safe = env.cluster()?.extras[0].safe_address.to_string();

    move_all_wxhopr(&token, &safe, SINK).await?;
    let outage_start = tokio::time::Instant::now();
    await_entry_safe(&env, "emptied", |b| b < per_cycle).await?;

    let entry_before = pix::sample_entry();
    let (session, exit_addr) = env.open_unreliable_session_paths(HOPS, HOPS).await?;
    let session_opened = tokio::time::Instant::now();
    let exit = node_for(&env, exit_addr)?;
    let exit_before = pix::sample_exit(&exit).await?;
    tracing::info!(exit = %exit_addr, elapsed = ?t0.elapsed(), "PIX session open with the entry's Safe empty");

    let (mut rx, mut tx) = tokio::io::split(session);
    while outage_start.elapsed() < OUTAGE {
        trickle(&mut rx, &mut tx).await?;
    }
    let during = entry_before.delta(&pix::sample_entry());
    tracing::info!(
        "entry counters at the end of the outage: {}",
        during.summary()
    );
    // Without a failed attempt the outage never touched the deposit and this run proves nothing.
    assert!(
        during.deposits_failed().unwrap_or(0) >= 1 && during.deposits().unwrap_or(0) == 0,
        "the outage did not fail the Session's deposit ({})",
        during.summary()
    );

    move_all_wxhopr(&token, SINK, &safe).await?;
    await_entry_safe(&env, "refilled", |b| b >= per_cycle * BUDGETED_CYCLES).await?;

    let mut entry = entry_before.delta(&pix::sample_entry());
    while entry.deposits().unwrap_or(0) == 0
        && session_opened.elapsed() < EXIT_DEADLINE + Duration::from_secs(30)
    {
        trickle(&mut rx, &mut tx).await?;
        entry = entry_before.delta(&pix::sample_entry());
    }
    tracing::info!(elapsed = ?t0.elapsed(), "entry counters after the refill: {}", entry.summary());
    assert!(
        entry.deposits().unwrap_or(0) >= 1,
        "the deposit that failed during the {OUTAGE:?} outage was never retried once the Safe was \
         refilled, so the Session stays unpaid until the Exit closes it ({})",
        entry.summary()
    );

    drain(&mut rx, TRICKLE_INTERVAL).await?;
    let datagrams = TARGET_CYCLES * EMISSIONS_PER_SSA * REPLY_MARGIN;
    let mut payload = vec![0u8; datagrams as usize * SEND_CHUNK];
    rand::rng().fill(&mut payload[..]);
    let opts = PumpOpts {
        pace: Some(SEND_INTERVAL),
        chunk: Some(SEND_CHUNK),
        ..PumpOpts::default()
    };
    let transfer =
        pump_halves(&mut rx, &mut tx, &payload, "pix-outage", PUMP_TIMEOUT, opts).await?;
    let exit_counters = await_sweeps(&exit, &exit_before, TARGET_CYCLES).await?;
    let entry = entry_before.delta(&pix::sample_entry());
    tracing::info!(
        outcome = ?transfer.outcome, elapsed = ?t0.elapsed(),
        "entry counters: {} | exit counters: {}", entry.summary(), exit_counters.summary()
    );

    assert!(
        transfer.arrival_pct() > 0.0,
        "the Session carried no traffic after the outage (outcome {:?}; exit {})",
        transfer.outcome,
        exit_counters.summary()
    );
    assert_eq!(
        exit_counters.deposits_timed_out().unwrap_or(0),
        0,
        "the Exit gave up waiting for a deposit, so the retry landed too late ({})",
        exit_counters.summary()
    );
    assert!(
        exit_counters.sweeps().unwrap_or(0) >= TARGET_CYCLES,
        "the Exit swept {:?} cycles on the Session after the outage, expected at least {TARGET_CYCLES} \
         (entry {}; exit {})",
        exit_counters.sweeps(),
        entry.summary(),
        exit_counters.summary()
    );

    tracing::info!("PIX deposit outage test PASSED in {:?}", t0.elapsed());
    Ok(())
}
