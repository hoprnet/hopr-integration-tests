//! hoprd#211: one of the node's transactions is accepted but never mined. The node must still get
//! its later transactions on chain, without a restart.

use std::time::{Duration, Instant};

use anyhow::Context;
use hoprd_integration_test::cluster::{self, ClusterSummary, NodeInfo};
use serde_json::{Value, json};

const FUND_AMOUNT: &str = "0.0001 wxHOPR";
const FUND_TIMEOUT: Duration = Duration::from_secs(60);
// Past bloklid's 300 s tracking timeout, so a recovery driven by that signal still counts.
const RECOVERY_WINDOW: Duration = Duration::from_secs(420);
const RECOVERY_PAUSE: Duration = Duration::from_secs(10);
const MINE_WAIT: Duration = Duration::from_secs(30);

#[test_log::test(tokio::test(flavor = "multi_thread"))]
#[ignore = "requires hoprd/hoprd-localcluster binaries + a chain"]
async fn node_should_recover_after_a_submitted_tx_is_never_mined() -> anyhow::Result<()> {
    anyhow::ensure!(
        cluster::request_cluster_size(4) >= 4,
        "need the victim plus three channel peers"
    );
    cluster::request_node_env([("RUST_LOG", "info,hopr_chain_connector=debug")]);
    let summary = &cluster::bring_up_shared().await?.summary;
    let [victim, baseline_peer, dropped_peer, recovery_peer, ..] = summary.nodes.as_slice() else {
        anyhow::bail!("cluster has {} nodes, need 4", summary.nodes.len());
    };

    // The REST status is no verdict: on anvil hoprd's tracking timeout (4 s) undercuts bloklid's
    // 5 s receipt poll, so a mined fund can still answer 422. The chain nonce is the ground truth.
    let before = chain_nonce(victim).await?;
    let baseline = fund(victim, baseline_peer, FUND_TIMEOUT).await;
    let mined_nonce = nonce_above(victim, before, MINE_WAIT)
        .await?
        .with_context(|| format!("baseline fund never mined: {baseline:?}"))?;
    tracing::info!(mined_nonce, ?baseline, "baseline fund mined");

    anvil("evm_setAutomine", json!([false])).await?;
    anvil("evm_setIntervalMining", json!([0])).await?;
    let in_flight = tokio::spawn({
        let (victim, peer) = (victim.clone(), dropped_peer.clone());
        async move { fund(&victim, &peer, RECOVERY_WINDOW * 2).await }
    });
    let dropped = drop_pool_txs_from(victim).await;
    anvil("evm_setIntervalMining", json!([1])).await?;
    let dropped = dropped?;
    tracing::info!(
        ?dropped,
        "dropped the victim's accepted tx from the mempool"
    );

    let started = Instant::now();
    let mut attempts = Vec::new();
    while started.elapsed() < RECOVERY_WINDOW {
        let outcome = fund(victim, recovery_peer, FUND_TIMEOUT).await;
        tracing::info!(?outcome, elapsed = ?started.elapsed(), "recovery fund");
        attempts.push(format!("{:>4}s {outcome:?}", started.elapsed().as_secs()));
        if nonce_above(victim, mined_nonce, RECOVERY_PAUSE)
            .await?
            .is_some()
        {
            break;
        }
    }

    let nonce_after = chain_nonce(victim).await?;
    let stuck_in_pool = pool_txs_from(victim)
        .await?
        .iter()
        .map(|tx| tx["nonce"].to_string())
        .collect::<Vec<_>>();
    let in_flight_outcome = if in_flight.is_finished() {
        format!("{:?}", in_flight.await?)
    } else {
        "still waiting".to_string()
    };
    let sequencer_log = victim_sequencer_log(summary);
    anyhow::ensure!(
        nonce_after > mined_nonce,
        "victim's chain nonce stuck at {mined_nonce} (now {nonce_after}) for {:?}\n\
         dropped: {dropped:?}\nstill in pool (nonces): {stuck_in_pool:?}\n\
         in-flight fund: {in_flight_outcome}\nattempts:\n  {}\nvictim sequencer log (last 20):\n  {}",
        started.elapsed(),
        attempts.join("\n  "),
        sequencer_log.join("\n  "),
    );
    Ok(())
}

async fn fund(
    from: &NodeInfo,
    to: &NodeInfo,
    timeout: Duration,
) -> anyhow::Result<(reqwest::StatusCode, String)> {
    let mut req = reqwest::Client::builder()
        .timeout(timeout)
        .build()?
        .post(format!(
            "{}/api/v4/channels/{}/fund",
            from.api_url, to.address
        ))
        .json(&json!({ "amount": FUND_AMOUNT }));
    if let Some(token) = from.api_token.as_ref() {
        req = req.header("Authorization", format!("Bearer {token}"));
    }
    let response = req.send().await.context("POST /channels/{address}/fund")?;
    Ok((response.status(), response.text().await?))
}

async fn anvil(method: &str, params: Value) -> anyhow::Result<Value> {
    let port = std::env::var("ANVIL_PORT").unwrap_or_else(|_| "8545".to_string());
    let response: Value = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}"))
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .await
        .with_context(|| format!("anvil {method}"))?
        .json()
        .await?;
    if let Some(error) = response.get("error") {
        anyhow::bail!("anvil {method}: {error}");
    }
    Ok(response["result"].clone())
}

async fn chain_nonce(node: &NodeInfo) -> anyhow::Result<u64> {
    let count = anvil(
        "eth_getTransactionCount",
        json!([node.address.to_string(), "latest"]),
    )
    .await?;
    let count = count.as_str().context("tx count is not a hex string")?;
    Ok(u64::from_str_radix(count.trim_start_matches("0x"), 16)?)
}

async fn nonce_above(node: &NodeInfo, floor: u64, within: Duration) -> anyhow::Result<Option<u64>> {
    let deadline = Instant::now() + within;
    loop {
        let nonce = chain_nonce(node).await?;
        if nonce > floor {
            return Ok(Some(nonce));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn pool_txs_from(node: &NodeInfo) -> anyhow::Result<Vec<Value>> {
    let pool = anvil("txpool_content", json!([])).await?;
    let sender = node.address.to_string().to_lowercase();
    Ok(["pending", "queued"]
        .iter()
        .filter_map(|section| pool[section].as_object())
        .flat_map(|by_sender| by_sender.iter())
        .filter(|(address, _)| address.to_lowercase() == sender)
        .filter_map(|(_, by_nonce)| by_nonce.as_object())
        .flat_map(|by_nonce| by_nonce.values().cloned())
        .collect())
}

async fn drop_pool_txs_from(node: &NodeInfo) -> anyhow::Result<Vec<(String, String)>> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let txs = pool_txs_from(node).await?;
        if !txs.is_empty() {
            let mut dropped = Vec::new();
            for tx in txs {
                anvil("anvil_dropTransaction", json!([tx["hash"]])).await?;
                dropped.push((tx["nonce"].to_string(), tx["hash"].to_string()));
            }
            return Ok(dropped);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "victim's fund tx never reached the mempool"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn victim_sequencer_log(summary: &ClusterSummary) -> Vec<String> {
    let Some(log_dir) = summary.log_dir.as_ref() else {
        return vec!["<no log dir>".to_string()];
    };
    let log = std::fs::read_to_string(log_dir.join("hoprd_0.log")).unwrap_or_default();
    let lines = log
        .lines()
        .filter(|l| l.contains("transaction count retrieved") || l.contains("nonce"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    lines[lines.len().saturating_sub(20)..].to_vec()
}
