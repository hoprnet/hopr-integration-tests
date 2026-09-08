#!/usr/bin/env bash
# Resolve versions, build hoprd + hoprd-localcluster + the blokli binary chain,
# link edgli, and run the integration throughput test against a fresh flake-built
# chain per scenario (no docker image).
#
# Version model — no stored state, no commits:
#   * the triggering project (PROJECT) uses the rev from the dispatch;
#   * everything else defaults to the head of its branch on the selected LINE;
#   * blokli is built from its flake — no docker image. A blokli dispatch overrides
#     it with the merged/PR rev, same as the other two.
# So a hoprd/edge-client merge is tested against the current tip of the other two.
#
# Branch model — every project is split into v4 and v5, and LINE picks the set:
#                   LINE=v4          LINE=v5
#   hoprd           release/4.1      main
#   hoprnet         release/4.0      master
#   edge-client     release/4.1      main       <- 4.1 cut 2026-09-07 from 1e211419
#   blokli          release/0.13     v0.14.0
#   PIX scenarios   no               yes
# The two sides are NOT interchangeable in either direction: edge-client #151 repinned
# `main` to hoprnet `master` on 2026-09-04, so a v5 edge client pairs with a v4 hoprd
# only by accident, and blokli release/0.13 cannot bootstrap a v5 localcluster at all
# (`service_registry` first appears in v0.14.0). HOPRD_LINE is the branch the hoprd
# binaries come from, and any dispatched hoprd rev must be contained in it.
#
# One crate serves both lines: the test bodies are shared source, and only the
# dependency set differs — `integration/Cargo.toml` is the v4 set, `Cargo.v5.toml` the
# v5 one, and LINE=v5 swaps the latter in for the run (restored on exit). PIX exists
# only on v5, since its deposit pool (`edgli/pix-test`) does not exist on v4.
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
#   NIX_SYSTEM_SUFFIX    nix output arch suffix (default: x86_64-linux)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CRATE_CARGO="${REPO_ROOT}/integration/Cargo.toml"
CRATE_LOCK="${REPO_ROOT}/integration/Cargo.lock"
ARCH="${NIX_SYSTEM_SUFFIX:-x86_64-linux}"

# Every per-line default in one place. An explicit env override still wins, so a
# cross-line experiment stays possible without editing this.
LINE="${LINE:-v4}"
case "${LINE}" in
v4)
  HOPRD_LINE="${HOPRD_LINE:-release/4.1}"
  EDGLI_REF="${EDGLI_REF:-release/4.1}"
  BLOKLI_DEFAULT="release/0.13"
  ;;
v5)
  HOPRD_LINE="${HOPRD_LINE:-main}"
  EDGLI_REF="${EDGLI_REF:-main}"
  BLOKLI_DEFAULT="v0.14.0"
  ;;
*)
  echo "unknown LINE '${LINE}' (expected v4 or v5)" >&2
  exit 2
  ;;
esac
HOPRD_REF="${HOPRD_REF:-${HOPRD_LINE}}"

# The triggering project overrides its own rev; the other keeps its default above.
case "${PROJECT:-}" in
hoprd) HOPRD_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=hoprd}" ;;
edge-client) EDGLI_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=edge-client}" ;;
# blokli merge-testing is back on: blokli now gates its own merges to release/0.13,
# so a dispatch from it must actually build the dispatched rev rather than the
# branch head. Set here and consumed by the BLOKLI_REF default further down.
blokli) BLOKLI_REF="${OVERRIDE_REV:?OVERRIDE_REV required for PROJECT=blokli}" ;;
"" | manual) echo "no PROJECT override — ${LINE}: hoprd at ${HOPRD_LINE}, edge-client at ${EDGLI_REF}, blokli at ${BLOKLI_REF:-${BLOKLI_DEFAULT}}" ;;
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
BLOKLI_REF="${BLOKLI_REF:-${BLOKLI_DEFAULT}}"

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
    echo "LINE=${LINE} builds hoprd from '${HOPRD_LINE}', and the crate's dependency set matches it." >&2
    echo "Set HOPRD_LINE to the intended line, or HOPRD_SKIP_LINE_CHECK=1 to run anyway." >&2
    exit 1
    ;;
  esac
fi

echo "resolved versions (LINE=${LINE}):"
echo "  hoprd        = ${HOPRD_REF} (line ${HOPRD_LINE})"
echo "  edge-client  = ${EDGLI_REF} (${EDGLI_SHA})"
echo "  blokli       = ${BLOKLI_REF}"

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
    echo "RESOLVED_LINE=${LINE}"
  } >>"${GITHUB_ENV}"
fi

# ── Put the selected line's dependency set in place ──
# Cargo insists the manifest be named `Cargo.toml`, so the v5 set cannot simply be
# passed with `--manifest-path`; it has to be copied over. Restored on exit so a
# local `just`/`cargo` afterwards is back on the committed (v4) set rather than
# silently building v5.
if [ "${LINE}" = "v5" ]; then
  echo "swapping in the v5 dependency set ..."
  MANIFEST_BACKUP="$(mktemp -d)"
  cp "${CRATE_CARGO}" "${MANIFEST_BACKUP}/Cargo.toml"
  cp "${CRATE_LOCK}" "${MANIFEST_BACKUP}/Cargo.lock"
  restore_manifest() {
    # Idempotent: the signal handler below restores and then exits, which fires the EXIT
    # trap as well.
    [ -d "${MANIFEST_BACKUP}" ] || return 0
    cp "${MANIFEST_BACKUP}/Cargo.toml" "${CRATE_CARGO}"
    cp "${MANIFEST_BACKUP}/Cargo.lock" "${CRATE_LOCK}"
    rm -rf "${MANIFEST_BACKUP}"
  }
  # Signals need their own trap: bash runs the EXIT trap on TERM/INT only if they are
  # trapped explicitly, and a signal handler does NOT end the script — without the
  # `exit` here a cancelled CI job would restore the v4 manifest and then carry on
  # running the v5 suites against it.
  trap restore_manifest EXIT
  trap 'restore_manifest; exit 143' HUP INT TERM
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

# ── The PIX exit binary (v5 only) ──
# The deposit pool is a BUILD-TIME choice and a plain hoprd carries none: it bootstraps
# normally and then simply never deposits, several minutes into a run. Built here rather
# than after the other suites so a missing output fails in one minute instead of forty.
# Only x86_64-linux: hoprd's flake exposes `binary-hoprd-pix-test` for that arch alone,
# so a local darwin run has to go through `just pix`, which builds hoprd from source.
PIX_SUITE=0
if [ "${LINE}" = "v5" ]; then
  if [ "${ARCH}" = "x86_64-linux" ]; then
    nix_build "hoprd (PIX pool)" -L "github:hoprnet/hoprd/${HOPRD_REF}#binary-hoprd-pix-test-${ARCH}" \
      --out-link "${REPO_ROOT}/result-hoprd-pix"
    PIX_BIN="${REPO_ROOT}/result-hoprd-pix/bin/hoprd"
    # `POOL` in hoprd::strategy is a &str compiled in for exactly this check — the two
    # pools are mutually exclusive and the binary carries exactly one.
    grep -qa 'non-anonymous-secp256k1' "${PIX_BIN}" || {
      echo "${PIX_BIN} carries no secp256k1 deposit pool — refusing to run the PIX suite" >&2
      exit 1
    }
    PIX_SUITE=1
  else
    echo "skipping the PIX suite: no binary-hoprd-pix-test output for ${ARCH} (use \`just pix\`)"
  fi
fi

# ── Pin edgli to the resolved sha, and hopr-lib to whatever that edgli pins ──
echo "pinning edgli to ${EDGLI_SHA} ..."
# Read through `gh api` for the reason resolve_sha gives: git-over-https is unusable in the dev
# shell. Needed because our `hopr-lib` must name the rev edgli resolves, and only edge-client's
# own manifest says which that is.
EDGLI_MANIFEST="$(gh api "repos/hoprnet/edge-client/contents/Cargo.toml?ref=${EDGLI_SHA}" \
  --jq '.content' 2>/dev/null | base64 -d)" || true
[ -n "${EDGLI_MANIFEST}" ] || {
  echo "could not read edge-client's Cargo.toml at ${EDGLI_SHA}" >&2
  exit 1
}
export EDGLI_MANIFEST
python3 - "$CRATE_CARGO" "$EDGLI_SHA" <<'PY'
import os, re, sys

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

src = src[: stanza.start()] + pinned + src[stanza.end() :]
print(f"  edgli pinned: {pinned.splitlines()[0]}")

# The v5 set carries a direct `hopr-lib` (to switch on `hopr-strategy/telemetry`'s
# sibling feature); it MUST name the same source and rev edgli resolves, or the lock
# carries two hopr-libs whose metrics are registered in one process and incremented by
# different copies. Mirror edge-client's own pin rather than trusting the committed one
# to have kept up. The v4 set has no such dep and this is a no-op there.
ours = re.search(r'^hopr-lib\s*=\s*\{.*?\}', src, re.S | re.M)
if ours:
    theirs = re.search(r'^hopr-lib\s*=\s*\{.*?\}', os.environ['EDGLI_MANIFEST'], re.S | re.M)
    if not theirs:
        sys.exit("run.sh: edge-client's manifest has no `hopr-lib` stanza to mirror")
    key = re.search(r'\b(?:branch|rev|tag)\s*=\s*"[^"]*"', theirs.group(0))
    if not key:
        sys.exit("run.sh: edge-client pins hopr-lib without a branch/rev/tag")
    mirrored, n = re.subn(r'\b(?:branch|rev|tag)\s*=\s*"[^"]*"', key.group(0),
                          ours.group(0), count=1)
    if n == 0:
        sys.exit("run.sh: our `hopr-lib` stanza carries no branch/rev/tag to mirror onto")
    src = src[: ours.start()] + mirrored + src[ours.end() :]
    print(f"  hopr-lib mirrored from edge-client: {key.group(0)}")

open(path, 'w').write(src)
PY
(cd "${REPO_ROOT}/integration" && cargo update -p edgli -p hopr-lib)

# Two copies of either crate is the failure mode the pins above exist to prevent, and
# it is invisible at runtime: every metric the tests read is registered by one copy and
# incremented by the other, so the reading is a full set of zeroes rather than an error
# — which is exactly the conclusion `tests/pix.rs` draws from a zero. Catch it here.
for crate in hopr-lib hopr-strategy; do
  n="$(grep -c "^name = \"${crate}\"$" "${CRATE_LOCK}" || true)"
  if [ "${n}" -gt 1 ]; then
    echo "error: ${n} copies of ${crate} in the lock after pinning — the direct dep and" >&2
    echo "edge-client's do not name the same source. Reconcile them before running." >&2
    grep -n -A2 "^name = \"${crate}\"$" "${CRATE_LOCK}" >&2
    exit 1
  fi
done

# ── Run every localcluster suite, fresh chain per scenario ──
# Everything that a local cluster can drive. `rotsee` is excluded because it needs a
# funded Gnosis identity and a reachable public exit; `profiling` because it emits
# traces rather than a verdict and needs its own build (--features prof, --profile
# tracer, tokio_unstable).
#
# One run-binchain.sh call per test binary; it starts/stops bloklid+anvil per
# scenario and reaps stray nodes in between. Suites are NOT short-circuited — a
# failure in one still runs the rest, so a red run reports everything broken rather
# than only the first thing.
BINCHAIN="$(dirname "${BASH_SOURCE[0]}")/run-binchain.sh"
suite_rc=0

run_suite() { # target, then scenario names
  local target="$1"
  shift
  echo "═══════ suite: ${target} ═══════"
  if ! SCENARIOS="$*" TEST_TARGET="${target}" bash "${BINCHAIN}"; then
    echo "suite ${target} FAILED" >&2
    suite_rc=1
  fi
}

echo "running integration tests (binary chain) ..."
run_suite integration zero_hop one_hop
# `return_path` is held out ENTIRELY: its scenarios assert an arrival ratio over an
# unforced random relayer draw, so a red says nothing. Locally: `just return-path`.
run_suite exit_origination exit_should_keep_originating_when_a_return_path_becomes_unresolvable

# Entry-side PIX: v5 only, both because `edgli/pix-test` exists nowhere on v4 and
# because the exit needs the PIX-pool binary built above. Named explicitly rather than
# left to the default filter — the two want different entry deposit budgets, and
# run-binchain.sh gives each scenario its own chain.
if [ "${PIX_SUITE}" = "1" ]; then
  export HOPRD_BIN="${PIX_BIN}" CARGO_FEATURES="--features pix"
  run_suite pix \
    edgli_entry_deposits_should_be_swept_into_the_exit_safe \
    a_session_should_close_when_the_entry_can_no_longer_deposit
fi

exit "${suite_rc}"
