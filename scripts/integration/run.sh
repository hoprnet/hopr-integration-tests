#!/usr/bin/env bash
# Resolve versions, build hoprd + hoprd-localcluster + the blokli binary chain,
# link edgli, and run the integration throughput test against a fresh flake-built
# chain per scenario (no docker image).
#
# Version model — no stored state, no commits:
#   * the triggering project (PROJECT) uses the rev from the dispatch;
#   * hoprd defaults to the v4 release line (see below), edge-client to main HEAD;
#   * blokli defaults to the head of its `release/0.13` branch (the Jura/v4 line),
#     built from its flake — no docker image. A blokli dispatch overrides it with
#     the merged/PR rev, same as the other two.
# So a hoprd/edge-client merge is tested against the current tip of the other and
# the current Jura blokli.
#
# Branch model — every project is split into v4 and v5, and this test targets v4:
#   hoprd       v4 = release/4.1   (v5 = main)
#   hoprnet     v4 = release/4.0   (v5 = master)
#   edge-client v4 = release/4.1   (v5 = main)  <- cut 2026-09-07 from 1e211419
#   blokli      v4 = release/0.13  (the Jura line)
# edge-client `main` used to track hoprnet `release/4.0` and no longer does: #151
# repinned it to hoprnet `master` on 2026-09-04. Tracking `main` would therefore pair
# a v5 edge client with a v4 hoprd, so EDGLI_REF defaults to the v4 branch instead.
# HOPRD_LINE below is the branch the hoprd binaries come from, and any dispatched
# hoprd rev is required to be contained in it.
#
# Inputs (env):
#   PROJECT          hoprd | edge-client | blokli | "" (manual = all defaults)
#   OVERRIDE_REV     git rev for PROJECT when it is hoprd or edge-client
#   HOPRD_LINE       hoprd release line the rev must belong to (default: release/4.1)
#   HOPRD_REF        default hoprd ref       (default: ${HOPRD_LINE})
#   EDGLI_REF        default edge-client ref (default: release/4.1, the v4 line)
#   BLOKLI_REF       blokli ref override     (default: release/0.13, a moving branch)
#   HOPRD_SKIP_LINE_CHECK  set to 1 to run a hoprd rev outside HOPRD_LINE anyway
#   NIX_SYSTEM_SUFFIX    nix output arch suffix (default: x86_64-linux)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CRATE_CARGO="${REPO_ROOT}/integration/Cargo.toml"
ARCH="${NIX_SYSTEM_SUFFIX:-x86_64-linux}"

HOPRD_LINE="${HOPRD_LINE:-release/4.1}"
HOPRD_REF="${HOPRD_REF:-${HOPRD_LINE}}"
EDGLI_REF="${EDGLI_REF:-release/4.1}"

# The triggering project overrides its own rev; the other keeps its default above.
case "${PROJECT:-}" in
hoprd) HOPRD_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=hoprd}" ;;
edge-client) EDGLI_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=edge-client}" ;;
# blokli merge-testing is back on: blokli now gates its own merges to release/0.13,
# so a dispatch from it must actually build the dispatched rev rather than the
# branch head. Set here and consumed by the BLOKLI_REF default further down.
blokli) BLOKLI_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=blokli}" ;;
"" | manual) echo "no PROJECT override — hoprd at ${HOPRD_LINE}, edge-client at ${EDGLI_REF}, blokli at ${BLOKLI_REF:-release/0.13}" ;;
*)
  echo "unknown PROJECT '${PROJECT}'" >&2
  exit 2
  ;;
esac

# Resolve edge-client ref → concrete sha (cargo git `rev` needs a commit, not a branch).
resolve_sha() { # owner/repo ref
  local ref="$2"
  if [[ $ref =~ ^[0-9a-f]{7,40}$ ]]; then
    echo "$ref"
    return
  fi
  # Resolve via `gh api`, not `git ls-remote`: the dev shell's LD_LIBRARY_PATH points
  # at nix glibc, which the system `git-remote-https` helper loads over its older
  # system glibc, tripping `GLIBC_ABI_DT_X86_64_PLT not found` and aborting the fetch
  # on CI. `gh` is a self-contained nix binary on PATH (auth via GH_TOKEN in CI; the
  # repo is public, so this also works unauthenticated locally).
  gh api "repos/$1/commits/${ref}" --jq '.sha' 2>/dev/null
}
EDGLI_SHA="$(resolve_sha hoprnet/edge-client "${EDGLI_REF}")"
[ -n "${EDGLI_SHA}" ] || {
  echo "could not resolve edge-client ref '${EDGLI_REF}'" >&2
  exit 1
}

# blokli tracks the `release/0.13` BRANCH — the line the Jura (v4) network runs,
# agreed with the blokli team. Deliberately a moving branch and not a resolved
# release number, so patch releases land without an edit here; `--refresh` on its
# build below is what makes that actually take effect.
#
# Note there is no `latest-jura` (or any `latest-*`) git tag in blokli — those
# names only ever existed as bloklid-anvil DOCKER tags, and this builds a flake
# ref, which resolves against git. Note also that the branch can sit ahead of what
# Jura actually deploys (branch head was 0.13.2 while jura-dev/prod pinned 0.13.1
# on 2026-09-03), so a green gate here is evidence about the 0.13 line, not proof
# about the exact deployed build.
BLOKLI_REF="${BLOKLI_REF:-release/0.13}"

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
if [ "${HOPRD_SKIP_LINE_CHECK:-0}" != "1" ] && [ "${HOPRD_REF}" != "${HOPRD_LINE}" ]; then
  status="$(gh api "repos/hoprnet/hoprd/compare/${HOPRD_LINE}...${HOPRD_REF}" --jq '.status' 2>/dev/null || true)"
  case "${status}" in
  identical | behind | ahead) ;;
  "")
    echo "could not compare hoprd ref '${HOPRD_REF}' against '${HOPRD_LINE}'" >&2
    exit 1
    ;;
  *)
    echo "hoprd ref '${HOPRD_REF}' is not on the '${HOPRD_LINE}' line (compare: ${status})." >&2
    echo "This test targets the hoprd v4 line — the integration crate pins hoprnet release/4.0." >&2
    echo "Set HOPRD_LINE to the intended line, or HOPRD_SKIP_LINE_CHECK=1 to run anyway." >&2
    exit 1
    ;;
  esac
fi

echo "resolved versions:"
echo "  hoprd        = ${HOPRD_REF} (line ${HOPRD_LINE})"
echo "  edge-client  = ${EDGLI_REF} (${EDGLI_SHA})"
echo "  blokli       = ${BLOKLI_REF} (Jura/v4 line, moving branch)"

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
    echo "RESOLVED_BLOKLI=${BLOKLI_REF}"
  } >>"${GITHUB_ENV}"
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
nix_build "hoprd" -L "github:hoprnet/hoprd/${HOPRD_REF}#binary-hoprd-${ARCH}" --out-link "${REPO_ROOT}/result-hoprd"
nix_build "hoprd-localcluster" -L "github:hoprnet/hoprd/${HOPRD_REF}#binary-hoprd-localcluster-${ARCH}" --out-link "${REPO_ROOT}/result-localcluster"

# ── Build the blokli binary chain from the branch (bloklid + deployer + anvil) ──
# `--refresh` is load-bearing: nix caches a flake ref's resolved revision for
# `tarball-ttl` (1h by default), so without it a branch that moved inside that
# window silently rebuilds the previous revision — which defeats the point of
# tracking a moving ref at all.
echo "building blokli chain from ${BLOKLI_REF} ..."
nix_build "bloklid + deployer" -L --refresh "github:hoprnet/blokli/${BLOKLI_REF}#bloklid" --out-link "${REPO_ROOT}/result-bloklid"
nix_build "anvil (foundry)" -L "nixpkgs#foundry" --out-link "${REPO_ROOT}/result-foundry"

# ── Pin edgli to the resolved sha and refresh the lockfile ──
echo "pinning edgli to ${EDGLI_SHA} ..."
python3 - "$CRATE_CARGO" "$EDGLI_SHA" <<'PY'
import re, sys

path, rev = sys.argv[1], sys.argv[2]
src = open(path).read()

# The committed manifest pins edgli by BRANCH (`branch = "main"`) on purpose, so the
# default does not drift behind what CI tests. Pinning here therefore has to *replace
# the branch key with a rev*, not edit an existing rev — an earlier version of this
# only handled `rev = "<sha>"` and so could never match the committed state.
# Accept whichever key the stanza carries so a repeat run over an already-pinned
# manifest works too.
stanza = re.search(r'^edgli\s*=\s*\{.*?\}', src, re.S | re.M)
if not stanza:
    sys.exit(f"run.sh: no `edgli = {{ ... }}` dependency stanza in {path} — "
             "refusing to run against a stale pin")

pinned, n = re.subn(r'\b(?:branch|rev|tag)\s*=\s*"[^"]*"', f'rev = "{rev}"',
                    stanza.group(0), count=1)
if n == 0:
    sys.exit(f"run.sh: the edgli stanza in {path} carries no branch/rev/tag to "
             "pin — refusing to run against a stale pin")

open(path, 'w').write(src[: stanza.start()] + pinned + src[stanza.end() :])
print(f"  edgli pinned: {pinned.splitlines()[0]}")
PY
(cd "${REPO_ROOT}/integration" && cargo update -p edgli)

# ── Run the test against a fresh flake-built chain per scenario ──
# run-binchain.sh starts/stops bloklid+anvil per scenario (HOPRD_CHAIN_URL) and
# runs both hop counts (zero_hop, one_hop) as separate tests.
echo "running integration tests (binary chain) ..."
exec bash "$(dirname "${BASH_SOURCE[0]}")/run-binchain.sh"
