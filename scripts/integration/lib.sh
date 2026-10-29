#!/usr/bin/env bash
# Shared helpers for the integration scripts. Source it, or call one function directly:
#   bash scripts/integration/lib.sh reap_nodes
#
# Deliberately no `set -euo pipefail` here — it would leak into every sourcing caller,
# including justfile recipes. Every caller sets its own.
# shellcheck shell=bash

LIB_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# The defaults every runner shares. Previously copied into five places, which is how
# HOPRD_PUMP_MBPS ended up at 0.5 in four of them and 1.0 in the fifth.
it_env() {
  export REPO_ROOT="${LIB_ROOT}"
  export HOPRD_BIN="${HOPRD_BIN:-${LIB_ROOT}/result-hoprd/bin/hoprd}"
  export HOPRD_LOCALCLUSTER_BIN="${HOPRD_LOCALCLUSTER_BIN:-${LIB_ROOT}/result-localcluster/bin/hoprd-localcluster}"
  export HOPRD_CHAIN_URL="${HOPRD_CHAIN_URL:-http://localhost:8080}"
  export RUST_LOG="${RUST_LOG:-info,edgli=debug}"
  export RUST_MIN_STACK="${RUST_MIN_STACK:-33554432}"
  export HOPRD_PUMP_MBPS="${HOPRD_PUMP_MBPS:-0.5}"
  export BLOKLI_API_PORT="${BLOKLI_API_PORT:-8080}"
  export ANVIL_PORT="${ANVIL_PORT:-8545}"
  export CHAIN_DATA_DIR="${CHAIN_DATA_DIR:-/tmp/hopr-chain}"
  grep -qa 'frozen-identities' "${HOPRD_LOCALCLUSTER_BIN}" 2>/dev/null && export HOPRD_FROZEN_IDENTITIES=1
  return 0
}

# The PR whose `Requires:` lines name the rest of a breaking change-set; see README.
companion_source_pr() {
  [ -n "${GITHUB_EVENT_PATH:-}" ] || return 0
  python3 - "${GITHUB_EVENT_PATH}" <<'PY'
import json, re, sys

event = json.load(open(sys.argv[1]))
pr = ((event.get("client_payload") or {}).get("tests_pr")
      or (event.get("inputs") or {}).get("tests_pr")
      or (event.get("pull_request") or {}).get("number"))
if not pr:
    queued = re.search(r"/pr-([0-9]+)-", (event.get("merge_group") or {}).get("head_ref", ""))
    pr = queued and queued.group(1)
print(pr or "")
PY
}

# Sets EDGLI_REF / HOPRD_REF / BLOKLI_REF to the open companions on LINE. Read from the checkout,
# not the workflow: a dispatch runs main's workflow, but this script from the tests PR.
# shellcheck disable=SC2034  # read by run.sh
companion_refs() {
  local source_pr body companions repo num state base sha fork line
  source_pr="$(companion_source_pr)"
  [ -n "${source_pr}" ] || return 0
  body="$(gh pr view "${source_pr}" -R "${GITHUB_REPOSITORY}" --json body -q .body)"
  companions="$(sed -nE 's/^[[:space:]]*Requires: hoprnet\/(hoprd|edge-client|blokli)#([0-9]+).*/\1 \2/p' <<<"${body}")"
  if [ -z "${companions}" ] && grep -qE '^[[:space:]]*Requires:' <<<"${body}"; then
    echo "::error::#${source_pr} has a Requires: line that does not parse as 'Requires: hoprnet/<hoprd|edge-client|blokli>#<n>'" >&2
    return 1
  fi
  while read -r repo num; do
    [ -n "${repo}" ] || continue
    read -r state base sha fork < <(gh pr view "${num}" -R "hoprnet/${repo}" \
      --json state,baseRefName,headRefOid,isCrossRepository \
      -q '"\(.state) \(.baseRefName) \(.headRefOid) \(.isCrossRepository)"')
    if [ "${fork}" != false ]; then
      echo "::error::hoprnet/${repo}#${num} is a fork PR; its head must not run on the self-hosted box" >&2
      return 1
    fi
    case "${base}" in
    main) line=v5 ;;
    release/*) line=v4 ;;
    *)
      echo "::error::hoprnet/${repo}#${num} targets '${base}', which maps to no line" >&2
      return 1
      ;;
    esac
    case "${state}" in
    MERGED)
      echo "hoprnet/${repo}#${num} is merged: ${line} uses the ${base} head"
      continue
      ;;
    OPEN) ;;
    *)
      echo "::error::hoprnet/${repo}#${num} is ${state}" >&2
      return 1
      ;;
    esac
    if [ "${GITHUB_EVENT_NAME:-}" = merge_group ]; then
      echo "::error::merge hoprnet/${repo}#${num} first: main must not get ahead of ${repo}" >&2
      return 1
    fi
    [ "${line}" = "${LINE:-v4}" ] || continue
    case "${repo}" in
    edge-client) EDGLI_REF="${sha}" ;;
    hoprd) HOPRD_REF="${sha}" HOPRD_PR_BASE="${base}" ;;
    blokli) BLOKLI_REF="${sha}" ;;
    esac
    echo "companion: ${repo} at ${sha} (hoprnet/${repo}#${num}, from #${source_pr})" |
      tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
  done <<<"${companions}"
}

# Per-line defaults; an explicit env override or a companion still wins.
# shellcheck disable=SC2034  # read by run.sh
line_refs() {
  case "${LINE:-v4}" in
  v4)
    HOPRD_LINE="${HOPRD_LINE:-release/4.1}"
    EDGLI_REF="${EDGLI_REF:-release/4.1}"
    BLOKLI_DEFAULT="release/0.13"
    ;;
  v5)
    HOPRD_LINE="${HOPRD_LINE:-main}"
    EDGLI_REF="${EDGLI_REF:-main}"
    BLOKLI_DEFAULT="release/0.14"
    ;;
  *)
    echo "unknown LINE '${LINE}' (expected v4 or v5)" >&2
    return 2
    ;;
  esac
}

# Cargo git `rev` needs a commit, not a branch. `gh api` rather than `git ls-remote`: the dev
# shell's LD_LIBRARY_PATH makes the system `git-remote-https` die on `GLIBC_ABI_DT_X86_64_PLT`.
resolve_sha() { # owner/repo ref
  local ref="$2"
  if [[ $ref =~ ^[0-9a-f]{7,40}$ ]]; then
    echo "$ref"
    return
  fi
  gh api "repos/$1/commits/${ref}" --jq '.sha' 2>/dev/null
}

# Test the hoprnet edge-client locks, not the branch tip: a hoprnet merge must reach us through an
# edge-client lock bump, which its gate then tests.
locked_hoprlib_rev() { # repo ref
  gh api -H "Accept: application/vnd.github.raw" "repos/hoprnet/$1/contents/Cargo.lock?ref=$2" 2>/dev/null |
    sed -n '/^name = "hopr-lib"$/,/^source/ s/.*#\([0-9a-f]\{40\}\)"$/\1/p'
}

# Pin edgli in integration/Cargo.toml to a sha, and hopr-lib / hopr-strategy to what that edgli
# pins. Sets EDGLI_HOPRLIB_REV.
pin_edgli() {
  local sha="${1:?usage: pin_edgli <edge-client sha>}" crate="${LIB_ROOT}/integration" manifest
  echo "pinning edgli to ${sha} ..."
  manifest="$(gh api "repos/hoprnet/edge-client/contents/Cargo.toml?ref=${sha}" \
    --jq '.content' 2>/dev/null | base64 -d)" || true
  [ -n "${manifest}" ] || {
    echo "could not read edge-client's Cargo.toml at ${sha}" >&2
    return 1
  }
  EDGLI_MANIFEST="${manifest}" python3 - "${crate}/Cargo.toml" "${sha}" <<'PY' || return 1
import os, re, sys

path, rev = sys.argv[1], sys.argv[2]
src = open(path).read()

# The committed manifest pins edgli by BRANCH, so this replaces whichever key the stanza carries.
stanza = re.search(r'^edgli\s*=\s*\{.*?\}', src, re.S | re.M)
if not stanza:
    sys.exit(f"pin_edgli: no `edgli = {{ ... }}` dependency stanza in {path}")

pinned, n = re.subn(r'\b(?:branch|rev|tag)\s*=\s*"[^"]*"', f'rev = "{rev}"',
                    stanza.group(0), count=1)
if n == 0:
    sys.exit(f"pin_edgli: the edgli stanza in {path} carries no branch/rev/tag to pin")

src = src[: stanza.start()] + pinned + src[stanza.end() :]
print(f"  edgli pinned: {pinned.splitlines()[0]}")

# v5's direct `hopr-lib` / `hopr-strategy` MUST name what edgli resolves, else the lock carries two
# copies and metrics are registered by one and incremented by the other. No-op on v4.
KEY = r'\b(?:branch|rev|tag)\s*=\s*"[^"]*"'
for dep, keypat in (("hopr-lib", KEY), ("hopr-strategy", r'\bversion\s*=\s*"[^"]*"')):
    stanza_re = r'^' + dep + r'\s*=\s*\{.*?\}'
    ours = re.search(stanza_re, src, re.S | re.M)
    if not ours:
        continue
    theirs = re.search(stanza_re, os.environ['EDGLI_MANIFEST'], re.S | re.M)
    if not theirs:
        sys.exit(f"pin_edgli: edge-client's manifest has no `{dep}` stanza to mirror")
    key = re.search(keypat, theirs.group(0))
    if not key:
        sys.exit(f"pin_edgli: edge-client pins {dep} without a key this can mirror")
    mirrored, n = re.subn(keypat, key.group(0), ours.group(0), count=1)
    if n == 0:
        sys.exit(f"pin_edgli: our `{dep}` stanza carries no key to mirror onto")
    src = src[: ours.start()] + mirrored + src[ours.end() :]
    print(f"  {dep} mirrored from edge-client: {key.group(0)}")

open(path, 'w').write(src)
PY
  EDGLI_HOPRLIB_REV="$(locked_hoprlib_rev edge-client "${sha}")" || true
  [ -n "${EDGLI_HOPRLIB_REV}" ] || {
    echo "could not read edge-client's locked hopr-lib rev at ${sha}" >&2
    return 1
  }
  echo "  hopr-lib pinned to edge-client's lock: ${EDGLI_HOPRLIB_REV}"
  (cd "${crate}" &&
    cargo update -p edgli &&
    cargo update -p hopr-lib --precise "${EDGLI_HOPRLIB_REV}") || return 1

  # Two copies is invisible at runtime: readings come back all-zero rather than erroring.
  local dep n
  for dep in hopr-lib hopr-strategy; do
    n="$(grep -c "^name = \"${dep}\"$" "${crate}/Cargo.lock" || true)"
    if [ "${n}" -gt 1 ]; then
      echo "error: ${n} copies of ${dep} in the lock after pinning — the direct dep and" >&2
      echo "edge-client's do not name the same source. Reconcile them before running." >&2
      grep -n -A2 "^name = \"${dep}\"$" "${crate}/Cargo.lock" >&2
      return 1
    fi
  done
}

# The PR checks' entry point: the edgli run.sh would test on LINE, without building anything.
pin_line_deps() {
  local sha
  companion_refs || return 1
  line_refs || return 1
  sha="$(resolve_sha hoprnet/edge-client "${EDGLI_REF}")"
  [ -n "${sha}" ] || {
    echo "could not resolve edge-client ref '${EDGLI_REF}'" >&2
    return 1
  }
  pin_edgli "${sha}"
}

# Bring up a docker-free local HOPR chain: anvil, the contract deploy, then bloklid
# serving GraphQL. Blocks until killed. Run it backgrounded via chain_start.
chain_up() {
  local anvil="${ANVIL_BIN:-${LIB_ROOT}/result-foundry/bin/anvil}"
  local bloklid="${BLOKLID_BIN:-${LIB_ROOT}/result-bloklid/bin/bloklid}"
  local deployer="${DEPLOYER_BIN:-${LIB_ROOT}/result-bloklid/bin/blokli-contract-deployer}"
  local dir="${CHAIN_DATA_DIR:-/tmp/hopr-chain}"
  local anvil_port="${ANVIL_PORT:-8545}"
  local api_port="${BLOKLI_API_PORT:-8080}"
  local rpc_url="http://127.0.0.1:${anvil_port}"
  local config="${dir}/bloklid-config.toml"

  local bin
  for bin in "${anvil}" "${bloklid}" "${deployer}"; do
    [ -x "${bin}" ] || {
      echo "chain_up: missing binary '${bin}'" >&2
      return 1
    }
  done

  rm -rf "${dir}"
  mkdir -p "${dir}"

  local anvil_pid="" bloklid_pid=""
  # Both children are killed here. The old chain-up.sh `exec`d bloklid, which replaced the
  # shell and its trap, so anvil was orphaned on every teardown and the callers' `pkill -f
  # anvil` was doing the actual work.
  # shellcheck disable=SC2329  # invoked indirectly via the trap below
  chain_up_cleanup() {
    [ -n "${bloklid_pid}" ] && kill "${bloklid_pid}" 2>/dev/null || true
    [ -n "${anvil_pid}" ] && kill "${anvil_pid}" 2>/dev/null || true
  }
  trap chain_up_cleanup EXIT INT TERM

  # anvil to a file, not the console. At --block-time 1 it narrates every block and every RPC
  # call: 4354 of 6973 lines in a two-scenario run came from anvil alone.
  echo "chain_up: starting anvil on ${rpc_url} (log: ${dir}/anvil.log)"
  "${anvil}" --host 127.0.0.1 --port "${anvil_port}" --block-time 1 --mixed-mining --accounts 10 --balance 10000 \
    >"${dir}/anvil.log" 2>&1 &
  anvil_pid=$!

  for _ in $(seq 1 60); do
    curl -sf -X POST "${rpc_url}" -H 'content-type: application/json' \
      --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' >/dev/null && break
    sleep 0.5
  done

  echo "chain_up: deploying HOPR contracts"
  if ! "${deployer}" --rpc-url "${rpc_url}" --output "${dir}/contracts-deploy.toml" \
    >"${dir}/deployer.log" 2>&1; then
    echo "chain_up: contract deployment failed — last 40 lines:" >&2
    tail -40 "${dir}/deployer.log" >&2
    return 1
  fi

  cat >"${config}" <<EOF
data_directory = "${dir}"
network = "anvil-localhost"
rpc_url = "${rpc_url}"
max_rpc_requests_per_sec = 0

[database]
type = "sqlite"
index_path = "${dir}/bloklid-index.db"
logs_path = "${dir}/bloklid-logs.db"
max_connections = 10

[indexer]
fast_sync = false
enable_logs_snapshot = false

[indexer.subscription]
event_bus_capacity = 100
shutdown_signal_capacity = 10
batch_size = 50

[api]
bind_address = "0.0.0.0:${api_port}"
enabled = true
playground_enabled = true

[api.health]
max_indexer_lag = 10
timeout = "5s"
readiness_check_interval = "5s"
EOF
  cat "${dir}/contracts-deploy.toml" >>"${config}"
  rm -f "${dir}/contracts-deploy.toml"

  # bloklid also to a file: its indexer logs every block it ingests. Readiness is polled over
  # GraphQL (chain_start), never parsed out of this.
  echo "chain_up: starting bloklid on 0.0.0.0:${api_port} (log: ${dir}/bloklid.log, Ctrl-C to stop)"
  "${bloklid}" -c "${config}" >"${dir}/bloklid.log" 2>&1 &
  bloklid_pid=$!
  wait "${bloklid_pid}"
}

# Tails of the chain logs, at the moment they are worth reading. chain_up keeps them in files,
# so a startup failure is otherwise a bare "chain died" with no cause.
chain_logs() {
  local dir="${CHAIN_DATA_DIR:-/tmp/hopr-chain}" f
  for f in bloklid.log anvil.log deployer.log; do
    [ -s "${dir}/${f}" ] || continue
    echo "── last 40 lines of ${dir}/${f} ──" >&2
    tail -40 "${dir}/${f}" >&2
  done
}

CHAIN_PID=""
chain_start() {
  local api_port="${BLOKLI_API_PORT:-8080}"
  bash "${LIB_ROOT}/scripts/integration/lib.sh" chain_up &
  CHAIN_PID=$!
  for _ in $(seq 1 60); do
    curl -sf -X POST "http://localhost:${api_port}/graphql" \
      -H 'content-type: application/json' --data '{"query":"{__typename}"}' >/dev/null 2>&1 && return 0
    kill -0 "${CHAIN_PID}" 2>/dev/null || {
      echo "chain died during startup" >&2
      chain_logs
      return 1
    }
    sleep 2
  done
  echo "chain did not become ready in time" >&2
  chain_logs
  return 1
}

# Every line is `|| true`: a caller runs under `set -e`, and killing an already-dead chain or
# pkill matching nothing is the normal case, not a failure.
chain_stop() {
  [ -n "${CHAIN_PID}" ] && kill "${CHAIN_PID}" 2>/dev/null || true
  # Safety net for a chain whose wrapper died without running its trap.
  pkill -f "result-bloklid/bin/bloklid" 2>/dev/null || true
  pkill -f "result-foundry/bin/anvil" 2>/dev/null || true
  wait "${CHAIN_PID}" 2>/dev/null || true
  CHAIN_PID=""
}

# The cargo test process tears its own cluster down, but localcluster's SIGINT→hoprd reaping is
# async and can lag. Stray nodes steal CPU from the next scenario — on the crypto-heavy 1-hop
# path that alone tanks arrival — so force-reap and let the machine idle first.
#
# Matched against ${HOPRD_BIN} rather than a hardcoded path: `just pix` points it at a cargo
# target dir. `pkill -f`/`pgrep -f` read an ERE, so a `+`, `(` or `[` in that path would either
# stop the pattern matching or make pgrep error — escaped once here.
reap_nodes() {
  local bin_re
  bin_re="$(printf '%s' "${HOPRD_BIN:-result-hoprd/bin/hoprd}" | sed 's/[][\.^$*+?(){}|]/\\&/g')"
  pkill -f "hoprd-localcluster" 2>/dev/null || true
  pkill -f "${bin_re}" 2>/dev/null || true
  for _ in $(seq 1 30); do
    pgrep -f "${bin_re}|hoprd-localcluster" >/dev/null 2>&1 || break
    sleep 1
  done
  sleep 5
}

# Start a standalone cluster against an already-running chain; sets CLUSTER_PID.
CLUSTER_PID=""
cluster_up() {
  local data_dir="${1:?usage: cluster_up <data-dir>}"
  # --api-host must match what the Rust readiness poll uses (API_HOST in cluster.rs): the
  # default "localhost" binds [::1], which 127.0.0.1 never reaches.
  "${HOPRD_LOCALCLUSTER_BIN}" \
    --size "${HOPRD_CLUSTER_SIZE:-3}" --extra-identities 1 \
    --api-host 127.0.0.1 \
    --api-port-base 13000 --p2p-port-base 19000 \
    --api-token test-token-localcluster \
    --hoprd-bin "${HOPRD_BIN}" \
    --chain-url "${HOPRD_CHAIN_URL}" \
    --data-dir "${data_dir}" &
  CLUSTER_PID=$!
}

cluster_wait() {
  local data_dir="${1:?usage: cluster_wait <data-dir> [tries]}" tries="${2:-120}"
  for _ in $(seq 1 "${tries}"); do
    kill -0 "${CLUSTER_PID}" 2>/dev/null || {
      echo "localcluster exited before becoming ready" >&2
      return 1
    }
    [ "$("${HOPRD_LOCALCLUSTER_BIN}" status --data-dir "${data_dir}" 2>/dev/null | jq -r '.state' 2>/dev/null)" = running ] && return 0
    sleep 5
  done
  echo "localcluster did not reach 'running' in time" >&2
  return 1
}

# The one cargo invocation. Skips the dev-shell wrap when already inside one — `just ci` and CI
# both enter it before calling the runner, which used to open a second shell per scenario.
# Extra args after the target are libtest filters; SKIP_SCENARIOS names tests to hold out.
cargo_it() {
  local target="${1:?usage: cargo_it <test-target> [filter...]}"
  shift
  local -a wrap=()
  [ -z "${IN_NIX_SHELL:-}" ] && [ "${HOPRNET_SHELL:-}" != none ] &&
    wrap=(nix develop "${HOPRNET_SHELL:-github:hoprnet/hoprnet}" -c)
  local -a test_args cargo_features skips=()
  read -r -a test_args <<<"${TEST_ARGS:-}"
  read -r -a cargo_features <<<"${CARGO_FEATURES:-}"
  local held
  for held in ${SKIP_SCENARIOS:-}; do skips+=(--skip "${held}"); done
  # Bash 3.2 (macOS /bin/bash) treats empty arrays as unset under `set -u`.
  ${wrap[@]+"${wrap[@]}"} cargo test --manifest-path "${LIB_ROOT}/integration/Cargo.toml" \
    ${cargo_features[@]+"${cargo_features[@]}"} --test "${target}" "$@" \
    --no-fail-fast -- --include-ignored --test-threads=1 ${skips[@]+"${skips[@]}"} ${test_args[@]+"${test_args[@]}"}
}

# Runs LINE/TIER's suites on the chain the caller started, then prints the summary; non-zero if any
# suite failed. Suites do not short-circuit, so a red run reports everything broken. v5 is tiered
# (#65, docs/v5-test-matrix.md): `pr` gates merges on the happy paths, `nightly` runs the rest; v4
# runs everything on either. PIX_BIN, a hoprd with the deposit pool, enables the PIX suites.
# `rotsee` needs a funded Gnosis identity and `profiling` emits traces, so neither is a suite here.
run_tier() {
  local suite_rc=0 nodes_bin="${HOPRD_BIN}" mixed scenarios scenario held_out
  export CHAIN_SHARED=1
  IT_RESULTS="$(mktemp)" IT_STEP_LOG="$(mktemp)"
  export IT_RESULTS IT_STEP_LOG

  in_tier() { [ "${LINE}" = v4 ] || [ "${TIER}" = "$1" ]; }
  run_suite() { # target, then any scenarios to HOLD OUT of it
    local target="$1"
    shift
    echo "═══════ suite: ${target} ═══════"
    if ! TEST_TARGETS="${target}" SCENARIOS="${SCENARIOS:-}" SCENARIOS_EXCEPT="$*" \
      bash "${LIB_ROOT}/scripts/integration/run-binchain.sh"; then
      echo "suite ${target} FAILED" >&2
      suite_rc=1
    fi
  }

  echo "running integration tests (binary chain, ${LINE}, tier ${TIER}) ..."
  in_tier pr && run_suite integration
  in_tier nightly && run_suite exit_origination
  # Gated: `upload_survival` reproduces the sustained-upload return-path collapse and therefore
  # FAILS against release/4.0 until the reply-opener LRU fix (hoprnet#8417) lands there.

  if [ -n "${PIX_BIN:-}" ]; then
    HOPRD_BIN="${PIX_BIN}"
    in_tier pr && run_suite pix
    # `pix_shapes` also needs a localcluster that takes `--pix-config`: the bare `--enable-pix` is
    # a 32-packet demo cycle no traffic shape fits inside, so it is probed rather than assumed.
    if pix_check_localcluster "${HOPRD_LOCALCLUSTER_BIN}" 2>/dev/null; then
      # The mixed shape also proves the production geometry completes a cycle, so it gates PRs.
      mixed=a_mixed_session_sustains_its_cycles
      in_tier pr && SCENARIOS="${mixed}" run_suite pix_shapes
      in_tier nightly && run_suite pix_shapes "${mixed}"
    else
      echo "::error::${HOPRD_LOCALCLUSTER_BIN} has no --pix-config, so the pix_shapes suite cannot" >&2
      echo "state its geometry. Use a newer hoprd ref." >&2
      suite_rc=1
    fi
    HOPRD_BIN="${nodes_bin}"
  fi

  # v4 only until the v5 stack fixes land. Last and on a fresh chain: its 5-node cluster leaves
  # nodes 3-4 channelled but offline. Scenarios kill nodes, so one cluster each. The 1-hop ones
  # only: v4 mitigates a dead relayer one hop deep (hoprnet#8498); `spread` asserts a ratio on a
  # random draw, so a red says nothing.
  if [ "${LINE}" = v4 ]; then
    chain_stop
    chain_start
    scenarios="$(list_scenarios return_path)"
    held_out="return_paths_should_spread_across_distinct_relayers"
    held_out="${held_out} a_two_hop_session_should_survive_exit_losing_its_return_relayer"
    held_out="${held_out} a_two_hop_session_should_survive_relayer_loss"
    for scenario in ${scenarios}; do
      case " ${held_out} " in *" ${scenario} "*) continue ;; esac
      SCENARIOS="${scenario}" run_suite return_path
    done
  fi

  print_summary
  return "${suite_rc}"
}

# Appends `target<TAB>test<TAB>ok|FAILED<TAB>error` to IT_RESULTS from one cargo run's output. With
# one test thread libtest prints `test NAME ... `, then the test's own output, then `ok`/`FAILED`
# (on the same line only when the test printed nothing). The error is the test's `Error:` line or
# panic message, inline under --nocapture or in the `---- NAME stdout ----` block otherwise.
record_results() { # target, captured output, cargo rc
  local target="$1" out="$2" rc="$3" before
  [ -n "${IT_RESULTS:-}" ] || return 0
  before="$(wc -l <"${IT_RESULTS}")"
  awk -v target="${target}" '
    {
      line = $0
      if (match(line, /^test [^ ]+ \.\.\. /)) {
        split(line, f, " "); cur = f[2]; line = substr(line, RLENGTH + 1)
      }
      if (line == "ok" || line == "FAILED") {
        if (cur != "" && !(cur in status)) { order[++n] = cur; status[cur] = line }
        if (!(cur in err) && pending != "") err[cur] = pending
        pending = ""
        next
      }
      if (match(line, /^---- [^ ]+ stdout ----$/)) { split(line, f, " "); cur = f[2]; failures = 1; next }
      msg = ""
      if ((i = index(line, "Error: ")) > 0) msg = substr(line, i + 7)
      else if (index(line, "panicked at") > 0 && (getline nxt) > 0) msg = nxt
      # Printed before its `test NAME` line when stdout and stderr reach the log separately.
      if (msg == "") next
      if (failures || (cur != "" && !(cur in status))) err[cur] = msg
      else pending = msg
    }
    END {
      for (i = 1; i <= n; i++) {
        t = order[i]
        printf "%s\t%s\t%s\t%s\n", target, t, status[t], (status[t] == "FAILED" ? err[t] : "")
      }
    }
  ' "${out}" >>"${IT_RESULTS}"
  if [ "${rc}" != 0 ] && [ "$(wc -l <"${IT_RESULTS}")" = "${before}" ]; then
    printf '%s\t*\tFAILED\tno test ran (build or harness error)\n' "${target}" >>"${IT_RESULTS}"
  fi
}

# pytest-style end-of-run report from IT_RESULTS, naming the failing step from IT_STEP_LOG. Mirrored
# into the job summary, and its failures into IT_SUMMARY for the failure notification.
print_summary() {
  [ -s "${IT_RESULTS:-}" ] || return 0
  local report
  report="$(awk -F'\t' -v steps="${IT_STEP_LOG:-/dev/null}" -v secs="${SECONDS}" '
    BEGIN {
      while ((getline l < steps) > 0) {
        split(l, s, "\t")
        if (s[3] == "failed") step[s[1]] = "step `" s[2] "`: " s[4]
      }
    }
    $3 == "ok" { print "PASSED " $1 "::" $2; p++; next }
    {
      print "FAILED " $1 "::" $2; f++
      why = ($2 in step) ? step[$2] : $4
      if (why != "") print "       " why
    }
    END { printf "%d passed, %d failed in %dm%02ds\n", p, f, secs / 60, secs % 60 }
  ' "${IT_RESULTS}")"
  printf '═══════ summary ═══════\n%s\n' "${report}"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    local fence='```'
    printf '### Integration (%s)\n%s\n%s\n%s\n' "${LINE:-}${TIER:+, ${TIER}}" "${fence}" "${report}" "${fence}" >>"${GITHUB_STEP_SUMMARY}"
  fi
  if [ -n "${GITHUB_ENV:-}" ]; then
    {
      echo "IT_SUMMARY<<IT_SUMMARY_EOF"
      grep -vE '^PASSED ' <<<"${report}"
      echo "IT_SUMMARY_EOF"
    } >>"${GITHUB_ENV}"
  fi
}

# Ask the binary what it carries, so a new scenario runs the moment it is written.
list_scenarios() {
  local target="${1:?usage: list_scenarios <test-target>}"
  local -a wrap=() cargo_features
  [ -z "${IN_NIX_SHELL:-}" ] && [ "${HOPRNET_SHELL:-}" != none ] &&
    wrap=(nix develop "${HOPRNET_SHELL:-github:hoprnet/hoprnet}" -c)
  read -r -a cargo_features <<<"${CARGO_FEATURES:-}"
  ${wrap[@]+"${wrap[@]}"} cargo test --manifest-path "${LIB_ROOT}/integration/Cargo.toml" \
    ${cargo_features[@]+"${cargo_features[@]}"} --test "${target}" -- --list |
    sed -n 's/: test$//p' | tr '\n' ' '
}

# The deposit pool is a build-time choice, and a binary carrying the other one bootstraps
# normally and then simply never deposits — minutes into a run. `POOL` in hoprd::strategy is a
# &str compiled in for exactly this check.
pix_check_hoprd() {
  local bin="${1:?usage: pix_check_hoprd <hoprd>}"
  grep -qa 'non-anonymous-secp256k1' "${bin}" || {
    echo "${bin} was not built with the secp256k1 deposit pool. Rebuild it:" >&2
    echo "    cargo build --release -p hoprd --features strategy-pix-test" >&2
    return 1
  }
}

# A localcluster without --pix-config ignores the geometry and silently runs the demo one,
# which every shape assertion would then be measuring.
pix_check_localcluster() {
  local bin="${1:?usage: pix_check_localcluster <hoprd-localcluster>}"
  grep -qa 'pix-config' "${bin}" || {
    echo "${bin} has no --pix-config; HOPRD_SRC is behind the geometry seam." >&2
    return 1
  }
}

# Build the PIX binaries from a hoprd checkout; the flake has no PIX output for darwin.
# Release rather than debug: debug builds distort the SSA cycle the scenarios rest on.
pix_build() {
  local src
  src="$(cd "${1:?usage: pix_build <hoprd-src>}" 2>/dev/null && pwd)" || {
    echo "HOPRD_SRC '${1}' is not a directory — point it at a v5 hoprd checkout." >&2
    return 2
  }
  echo "building PIX-enabled hoprd + hoprd-localcluster from ${src}"
  (cd "${src}" && nix develop -c cargo build --release -p hoprd --features strategy-pix-test) || return 1
  (cd "${src}" && nix develop -c cargo build --release -p hoprd-localcluster) || return 1
  export HOPRD_BIN="${src}/target/release/hoprd"
  export HOPRD_LOCALCLUSTER_BIN="${src}/target/release/hoprd-localcluster"
}

# What "the state" is, in one place. Was four divergent lists.
clean_tmp() {
  rm -rf /tmp/hopr-chain /tmp/hopr-nodes /tmp/hopr-it /tmp/hoprd-it-* 2>/dev/null || true
  find /tmp -maxdepth 3 -type d -name 'hoprd-it-*' -exec rm -rf {} + 2>/dev/null || true
}

# Called as a script rather than sourced: dispatch to the named function. An `if` rather than
# `[ ... ] && ...`, whose false branch would make `source` return non-zero and abort a caller
# running under `set -e`.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  "$@"
fi
