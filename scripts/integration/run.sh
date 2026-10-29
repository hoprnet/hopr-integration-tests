#!/usr/bin/env bash
# Resolve versions, build hoprd + hoprd-localcluster + the blokli binary chain, link edgli,
# and run every suite on one shared flake-built chain (no docker image).
#
# No stored state: the dispatching project supplies its rev, everything else resolves to the
# head of its branch on LINE. The lines are NOT mixable; README has the branch table.
#
# Inputs (env):
#   LINE             v4 | v5 — which release line to test (default: v4)
#   PROJECT          hoprd | edge-client | blokli | "" (manual = all defaults)
#   OVERRIDE_REV     git rev for PROJECT when it is hoprd or edge-client
#   HOPRD_LINE       hoprd release line the rev must belong to (default: per LINE)
#   HOPRD_REF        default hoprd ref       (default: ${HOPRD_LINE})
#   EDGLI_REF        default edge-client ref (default: per LINE)
#   BLOKLI_REF       blokli ref override     (default: per LINE)
#   HOPRD_SKIP_LINE_CHECK  set to 1 to run a hoprd rev outside HOPRD_LINE anyway
#   NIX_SYSTEM_SUFFIX    cross-build to this nix system (default: empty = build for this machine)
#   TIER             pr | nightly — which v5 suites run (default: pr); v4 runs everything on either
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

LINE="${LINE:-v4}"
TIER="${TIER:-pr}"
case "${TIER}" in
pr | nightly) ;;
*)
  echo "unknown TIER '${TIER}' (pr | nightly)" >&2
  exit 2
  ;;
esac

companion_refs
line_refs
HOPRD_REF="${HOPRD_REF:-${HOPRD_LINE}}"

# The triggering project overrides its own rev; the other keeps its default above.
case "${PROJECT:-}" in
hoprd) HOPRD_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=hoprd}" HOPRD_PR_BASE="" ;;
edge-client) EDGLI_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=edge-client}" ;;
# blokli merge-testing is back on: blokli now gates its own merges to release/0.13,
# so a dispatch from it must actually build the dispatched rev rather than the
# branch head. Set here and consumed by the BLOKLI_REF default further down.
blokli) BLOKLI_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=blokli}" ;;
"" | manual) echo "no PROJECT override — ${LINE}: hoprd at ${HOPRD_REF}, edge-client at ${EDGLI_REF}, blokli at ${BLOKLI_REF:-${BLOKLI_DEFAULT}}" ;;
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

# blokli tracks the `release/0.13` BRANCH — the line the Jura (v4) network runs,
# agreed with the blokli team — and `release/0.14` on v5. Deliberately a moving branch and not a resolved
# release number, so patch releases land without an edit here; resolving it to a sha
# below is what makes that actually take effect.
#
# Note there is no `latest-jura` (or any `latest-*`) git tag in blokli — those
# names only ever existed as bloklid-anvil DOCKER tags, and this builds a flake
# ref, which resolves against git. Note also that the branch can sit ahead of what
# Jura actually deploys (branch head was 0.13.2 while jura-dev/prod pinned 0.13.1
# on 2026-09-03), so a green gate here is evidence about the 0.13 line, not proof
# about the exact deployed build.
BLOKLI_REF="${BLOKLI_REF:-${BLOKLI_DEFAULT}}"

# Built by sha, not branch, so the run tests exactly the stack its fingerprint below names.
HOPRD_SHA="$(resolve_sha hoprnet/hoprd "${HOPRD_REF}")"
BLOKLI_SHA="$(resolve_sha hoprnet/blokli "${BLOKLI_REF}")"
[ -n "${HOPRD_SHA}" ] && [ -n "${BLOKLI_SHA}" ] || {
  echo "could not resolve hoprd ref '${HOPRD_REF}' or blokli ref '${BLOKLI_REF}'" >&2
  exit 1
}

# Reject a hoprd rev from the wrong side of the v4/v5 split. A merge dispatch from
# hoprd `main` carries a v5 sha, which pairs with a v4 hopr-lib only by accident;
# fail fast with the reason rather than after a 40-minute build + a red gate.
#
# `compare/<line>...<rev>` reports, from the line's point of view:
#   identical / behind — the rev is contained in the line
#   ahead             — the rev CONTAINS the line plus new commits, i.e. a branch or
#                       PR head based on it. Accepted: that is precisely the
#                       label-triggered PR case, which hoprd fires with its PR head.
#   diverged          — the rev is off the line entirely (a v5 `main` sha). Rejected.
# A companion PR that targets the line is skipped: once its base moves on, its head reads `diverged`.
if [ "${HOPRD_SKIP_LINE_CHECK:-0}" != "1" ] && [ "${HOPRD_REF}" != "${HOPRD_LINE}" ] &&
  [ "${HOPRD_PR_BASE:-}" != "${HOPRD_LINE}" ]; then
  status="$(gh api "repos/hoprnet/hoprd/compare/${HOPRD_LINE}...${HOPRD_REF}" --jq '.status' 2>/dev/null || true)"
  case "${status}" in
  identical | behind | ahead) ;;
  "")
    echo "could not compare hoprd ref '${HOPRD_REF}' against '${HOPRD_LINE}'" >&2
    exit 1
    ;;
  *)
    echo "hoprd ref '${HOPRD_REF}' is not on the '${HOPRD_LINE}' line (compare: ${status})." >&2
    echo "LINE=${LINE} builds hoprd from '${HOPRD_LINE}', and the crate's dependency set matches it." >&2
    echo "Set HOPRD_LINE to the intended line, or HOPRD_SKIP_LINE_CHECK=1 to run anyway." >&2
    exit 1
    ;;
  esac
fi

echo "resolved versions (LINE=${LINE}):"
echo "  hoprd        = ${HOPRD_REF} (${HOPRD_SHA}, line ${HOPRD_LINE})"
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
    echo "RESOLVED_LINE=${LINE}"
  } >>"${GITHUB_ENV}"
fi

# A gating run reuses the green of a run with the same fingerprint, usually the PR's
# `run-integration` run. Upstreams count by tree: their queue candidate is a new commit
# with the PR head's tree. The toolchain is left out: it is unpinned, so every run would miss.
tree_of() { gh api "repos/hoprnet/$1/commits/$2" --jq .commit.tree.sha 2>/dev/null || true; }
upstream_trees="$(tree_of hoprd "${HOPRD_SHA}") $(tree_of edge-client "${EDGLI_SHA}") $(tree_of blokli "${BLOKLI_SHA}")"
fingerprint_inputs="${LINE} ${TIER} $(git -C "${REPO_ROOT}" rev-parse 'HEAD^{tree}') ${upstream_trees}"
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
    echo "${LINE}: same stack already passed in [run ${passed_run}](https://github.com/${GITHUB_REPOSITORY}/actions/runs/${passed_run}), not rerunning" |
      tee -a "${GITHUB_STEP_SUMMARY:-/dev/null}"
    exit 0
  fi
fi
if [ -n "${GITHUB_ENV:-}" ] && [ -n "${IT_FINGERPRINT}" ]; then
  echo "IT_FINGERPRINT=${IT_FINGERPRINT}" >>"${GITHUB_ENV}"
  echo "${fingerprint_inputs}" >"${REPO_ROOT}/it-pass.txt"
fi

# ── Put the selected line's dependency set in place ──
# Copied rather than `--manifest-path`: cargo insists on the name `Cargo.toml`.
# Restored on exit on BOTH lines: the edgli pin below rewrites the manifest and lock in place,
# and a v4 run that left them dirty once got committed.
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
# the script, so without it a cancelled job restores v4 then runs v5 suites against it.
trap restore_manifest EXIT
trap 'restore_manifest; exit 143' HUP INT TERM
if [ "${LINE}" = "v5" ]; then
  echo "swapping in the v5 dependency set ..."
  cp "${REPO_ROOT}/integration/Cargo.v5.toml" "${CRATE_CARGO}"
  cp "${REPO_ROOT}/integration/Cargo.v5.lock" "${CRATE_LOCK}"
fi

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
nix_build "anvil (foundry)" -L "nixpkgs#foundry" --out-link "${REPO_ROOT}/result-foundry"

# ── The PIX exit binary (v5 only) ──
# The deposit pool is a build-time choice: a plain hoprd bootstraps fine and then never
# deposits. Built up front so a missing output fails in a minute, not after forty.
# x86_64-linux only — the flake exposes no other arch, so darwin goes via `just pix`.
PIX_BIN=""
if [ "${LINE}" = "v5" ]; then
  if [ "${SYSTEM}" = "x86_64-linux" ]; then
    nix_build "hoprd (PIX pool)" -L "github:hoprnet/hoprd/${HOPRD_SHA}#binary-hoprd-pix-test-${SYSTEM}" \
      --out-link "${REPO_ROOT}/result-hoprd-pix"
    PIX_BIN="${REPO_ROOT}/result-hoprd-pix/bin/hoprd"
    pix_check_hoprd "${PIX_BIN}" || exit 1
  else
    echo "skipping the PIX suite: no binary-hoprd-pix-test output for ${SYSTEM} (use \`just pix\`)"
  fi
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
run_tier
