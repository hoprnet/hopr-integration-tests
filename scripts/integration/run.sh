#!/usr/bin/env bash
# Resolve versions, build hoprd + hoprd-localcluster + the blokli binary chain, link edgli,
# and run every suite on one shared flake-built chain (no docker image).
#
# No stored state: the dispatching project supplies its rev, everything else resolves to the
# head of its `main` branch.
#
# Inputs (env):
#   PROJECT          hoprd | edge-client | blokli | "" (manual = all defaults)
#   OVERRIDE_REV     git rev for PROJECT
#   HOPRD_REF        default hoprd ref       (default: main)
#   EDGLI_REF        default edge-client ref (default: main)
#   BLOKLI_REF       blokli ref override     (default: main)
#   NIX_SYSTEM_SUFFIX    cross-build to this nix system (default: empty = build for this machine)
#   TIER             pr | nightly — which suites run (default: pr)
#   SHARD            a | b — half of the nightly tier, one runner each (default: empty = both)
set -euo pipefail

# shellcheck source=scripts/integration/lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"
REPO_ROOT="${LIB_ROOT}"
CRATE_CARGO="${REPO_ROOT}/integration/Cargo.toml"
CRATE_LOCK="${REPO_ROOT}/integration/Cargo.lock"
# Bare flake names resolve to this system, and `binary-hoprd-localcluster` has no per-system
# alias at all — only an x86_64-linux one. So suffix nothing by default; NIX_SYSTEM_SUFFIX is a
# cross-build override (CI sets it to keep building the musl outputs), not a default.
SUFFIX="${NIX_SYSTEM_SUFFIX:+-${NIX_SYSTEM_SUFFIX}}"
SYSTEM="${NIX_SYSTEM_SUFFIX:-$(nix eval --raw --impure --expr builtins.currentSystem)}"

TIER="${TIER:-pr}"
case "${TIER}" in
pr | nightly) ;;
*)
  echo "unknown TIER '${TIER}' (pr | nightly)" >&2
  exit 2
  ;;
esac
case "${SHARD:-}" in
"" | a | b) ;;
*)
  echo "unknown SHARD '${SHARD}' (a | b)" >&2
  exit 2
  ;;
esac

companion_refs
HOPRD_REF="${HOPRD_REF:-main}"
EDGLI_REF="${EDGLI_REF:-main}"
BLOKLI_REF="${BLOKLI_REF:-main}"

# The triggering project overrides its own rev; the other keeps its default above.
case "${PROJECT:-}" in
hoprd) HOPRD_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=hoprd}" ;;
edge-client) EDGLI_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=edge-client}" ;;
blokli) BLOKLI_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=blokli}" ;;
"" | manual) echo "no PROJECT override — hoprd at ${HOPRD_REF}, edge-client at ${EDGLI_REF}, blokli at ${BLOKLI_REF}" ;;
*)
  echo "unknown PROJECT '${PROJECT}'" >&2
  exit 2
  ;;
esac

EDGLI_SHA="$(resolve_sha hoprnet/edge-client "${EDGLI_REF}")"
[ -n "${EDGLI_SHA}" ] || {
  echo "could not resolve edge-client ref '${EDGLI_REF}'" >&2
  exit 1
}

# Built by sha, not branch, so the run tests exactly the stack its fingerprint below names.
HOPRD_SHA="$(resolve_sha hoprnet/hoprd "${HOPRD_REF}")"
BLOKLI_SHA="$(resolve_sha hoprnet/blokli "${BLOKLI_REF}")"
[ -n "${HOPRD_SHA}" ] && [ -n "${BLOKLI_SHA}" ] || {
  echo "could not resolve hoprd ref '${HOPRD_REF}' or blokli ref '${BLOKLI_REF}'" >&2
  exit 1
}

echo "resolved versions:"
echo "  hoprd        = ${HOPRD_REF} (${HOPRD_SHA})"
echo "  edge-client  = ${EDGLI_REF} (${EDGLI_SHA})"
echo "  blokli       = ${BLOKLI_REF} (${BLOKLI_SHA})"

# Hand the resolved versions to the workflow so a failure notification can report
# what actually ran. The dispatch inputs are no good for this: they only ever carry
# the triggering project's rev, so a manual or PR-label run has nothing to report and
# used to render an empty "(rev: )". These are always populated once resolution got
# this far, and they cover all three projects rather than just one.
if [ -n "${GITHUB_ENV:-}" ]; then
  # A merge dispatch makes HOPRD_REF a full sha; abbreviate it so the notification
  # does not carry 40 characters that the trigger line already shows.
  hoprd_display="${HOPRD_REF}"
  [[ ${HOPRD_REF} =~ ^[0-9a-f]{40}$ ]] && hoprd_display="${HOPRD_REF:0:8}"
  {
    echo "RESOLVED_HOPRD=${hoprd_display}"
    echo "RESOLVED_EDGLI=${EDGLI_REF} (${EDGLI_SHA:0:8})"
    echo "RESOLVED_BLOKLI=${BLOKLI_REF} (${BLOKLI_SHA:0:8})"
  } >>"${GITHUB_ENV}"
fi

# A gating run reuses the green of a run with the same fingerprint, usually the PR's
# `run-integration` run. Upstreams count by tree: their queue candidate is a new commit
# with the PR head's tree. The toolchain is left out: it is unpinned, so every run would miss.
tree_of() { gh api "repos/hoprnet/$1/commits/$2" --jq .commit.tree.sha 2>/dev/null || true; }
upstream_trees="$(tree_of hoprd "${HOPRD_SHA}") $(tree_of edge-client "${EDGLI_SHA}") $(tree_of blokli "${BLOKLI_SHA}")"
fingerprint_inputs="${TIER}${SHARD:+-${SHARD}} $(git -C "${REPO_ROOT}" rev-parse 'HEAD^{tree}') ${upstream_trees}"
IT_FINGERPRINT=""
if [[ ${upstream_trees} =~ ^[0-9a-f]{40}\ [0-9a-f]{40}\ [0-9a-f]{40}$ ]]; then
  IT_FINGERPRINT="$(sha256sum <<<"${fingerprint_inputs}" | cut -c1-16)"
fi
echo "  fingerprint  = ${IT_FINGERPRINT:-none} (${fingerprint_inputs})"
if [ -n "${IT_FINGERPRINT}" ] && [[ ${GITHUB_EVENT_NAME:-} =~ ^(merge_group|repository_dispatch)$ ]]; then
  # Same-repo heads only: a fork PR runs its own workflow and could upload the marker untested.
  repo_id="$(gh api "repos/${GITHUB_REPOSITORY}" --jq .id 2>/dev/null || true)"
  passed_run="$(gh api "repos/${GITHUB_REPOSITORY}/actions/artifacts?name=it-pass-${IT_FINGERPRINT}" \
    --jq "[.artifacts[] | select((.expired | not) and .workflow_run.head_repository_id == ${repo_id:-0})][0].workflow_run.id // empty" \
    2>/dev/null || true)"
  if [ -n "${passed_run}" ]; then
    echo "${TIER}: same stack already passed in [run ${passed_run}](https://github.com/${GITHUB_REPOSITORY}/actions/runs/${passed_run}), not rerunning" |
      tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
    exit 0
  fi
fi
if [ -n "${GITHUB_ENV:-}" ] && [ -n "${IT_FINGERPRINT}" ]; then
  echo "IT_FINGERPRINT=${IT_FINGERPRINT}" >>"${GITHUB_ENV}"
  echo "${fingerprint_inputs}" >"${REPO_ROOT}/it-pass.txt"
fi

# ── Restore the manifest on exit ──
# The edgli pin below rewrites the manifest and lock in place, and a run that left them dirty
# once got committed.
MANIFEST_BACKUP="$(mktemp -d)"
cp "${CRATE_CARGO}" "${MANIFEST_BACKUP}/Cargo.toml"
cp "${CRATE_LOCK}" "${MANIFEST_BACKUP}/Cargo.lock"
# shellcheck disable=SC2329  # invoked indirectly via the traps below
restore_manifest() {
  # Idempotent: the signal handler exits, firing the EXIT trap too.
  [ -d "${MANIFEST_BACKUP}" ] || return 0
  cp "${MANIFEST_BACKUP}/Cargo.toml" "${CRATE_CARGO}"
  cp "${MANIFEST_BACKUP}/Cargo.lock" "${CRATE_LOCK}"
  rm -rf "${MANIFEST_BACKUP}"
}
# Signals need their own trap, and the `exit` is load-bearing: a handler does not end
# the script, so without it a cancelled job restores the manifest then runs the suites anyway.
trap restore_manifest EXIT
trap 'restore_manifest; exit 143' HUP INT TERM

# ── Build everything, keeping the build chatter off the console ──
# `nix build -L` emits every derivation's build log: ~20k lines for one run, which
# buries the few dozen lines anyone actually reads. Keep `-L` (the detail is what
# makes a failed build diagnosable) but send it to a file, print one line per
# build, and dump the tail only when a build fails. BUILD_LOG is picked up by the
# workflow and uploaded as an artifact, so the full detail is still one click away.
BUILD_LOG="${BUILD_LOG:-${REPO_ROOT}/nix-build.log}"
: >"${BUILD_LOG}"

nix_build() { # description, then `nix build` arguments
  local what="$1"
  shift
  echo "  building ${what} ..."
  {
    echo
    echo "═══ ${what} ═══"
  } >>"${BUILD_LOG}"
  if ! nix build "$@" >>"${BUILD_LOG}" 2>&1; then
    echo "nix build failed: ${what} — last 80 lines of ${BUILD_LOG}:" >&2
    tail -80 "${BUILD_LOG}" >&2
    return 1
  fi
}

echo "building hoprd binaries from ref ${HOPRD_REF} ..."
nix_build "hoprd" -L "github:hoprnet/hoprd/${HOPRD_SHA}#binary-hoprd${SUFFIX}" --out-link "${REPO_ROOT}/result-hoprd"
nix_build "hoprd-localcluster" -L "github:hoprnet/hoprd/${HOPRD_SHA}#binary-hoprd-localcluster${SUFFIX}" --out-link "${REPO_ROOT}/result-localcluster"

# ── Build the blokli binary chain from the branch (bloklid + deployer + anvil) ──
# By sha, so the 1h `tarball-ttl` cache of a branch's revision cannot serve a stale build.
echo "building blokli chain from ${BLOKLI_REF} ..."
nix_build "bloklid + deployer" -L "github:hoprnet/blokli/${BLOKLI_SHA}#bloklid" --out-link "${REPO_ROOT}/result-bloklid"
# anvil from the nixpkgs blokli locks, not the registry's: `nixpkgs#foundry` alone follows
# nixpkgs-unstable, and when that moved to a foundry the binary cache does not hold, the
# nightly had to build it from source and failed before any test ran (2026-11-12).
nix_build "anvil (foundry)" -L --inputs-from "github:hoprnet/blokli/${BLOKLI_SHA}" "nixpkgs#foundry" \
  --out-link "${REPO_ROOT}/result-foundry"

# ── The PIX exit binary ──
# The deposit pool is a build-time choice: a plain hoprd bootstraps fine and then never
# deposits. Built up front so a missing output fails in a minute, not after forty.
# x86_64-linux only — the flake exposes no other arch, so darwin goes via `just pix`.
PIX_BIN=""
if [ "${SYSTEM}" = "x86_64-linux" ]; then
  nix_build "hoprd (PIX pool)" -L "github:hoprnet/hoprd/${HOPRD_SHA}#binary-hoprd-pix-test-${SYSTEM}" \
    --out-link "${REPO_ROOT}/result-hoprd-pix"
  PIX_BIN="${REPO_ROOT}/result-hoprd-pix/bin/hoprd"
  pix_check_hoprd "${PIX_BIN}" || exit 1
else
  echo "skipping the PIX suite: no binary-hoprd-pix-test output for ${SYSTEM} (use \`just pix\`)"
fi

# ── Pin edgli to the resolved sha, and hopr-lib to whatever that edgli pins ──
pin_edgli "${EDGLI_SHA}"
# The cluster's nodes and the edgli entry must speak the same wire format (packet size, SURBs).
HOPRD_HOPRLIB_REV="$(locked_hoprlib_rev hoprd "${HOPRD_SHA}")" || true
if [ "${HOPRD_HOPRLIB_REV}" != "${EDGLI_HOPRLIB_REV}" ]; then
  echo "::warning::hoprd locks hoprnet ${HOPRD_HOPRLIB_REV:-unknown}, edge-client ${EDGLI_HOPRLIB_REV}; a wire change between them breaks every session" >&2
fi

# ── Run the tier's suites on one shared chain ──
# One chain for every suite: contract deploy + bloklid start happen once, and a localcluster with
# frozen identities finds its nodes already funded, announced and channelled on it.
it_env
chain_start
trap 'restore_manifest; chain_stop' EXIT
export PIX_BIN
[ -z "${GITHUB_OUTPUT:-}" ] || echo 'tests_started=true' >>"${GITHUB_OUTPUT}"
run_tier
