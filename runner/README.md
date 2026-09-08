# Runner

Jobs are split by whether they **execute a test** or only compile:

| Job                                | Runner                 | Why                                     |
| ---------------------------------- | ---------------------- | --------------------------------------- |
| `pr.yaml` → `validate-pr-title`    | `depot-ubuntu-24.04`   | metadata only                           |
| `pr.yaml` → `lint` (fmt + clippy)  | `depot-ubuntu-24.04-4` | compile-time checks, timing-insensitive |
| `pr.yaml` → `unit`                 | **`hetzner`**          | runs tests                              |
| `integration.yaml` → `integration` | **`hetzner`**          | the throughput gate                     |

Anything that runs a test runs on the **self-hosted Hetzner box** (label
`hetzner`), so a result here and a result there come from the same hardware. A
dedicated machine rather than a hosted VM because the onion encoder is CPU-bound
and the throughput thresholds only mean anything on a stable core count — on the
hosted runners the gate used to sit on, the 4-vCPU size already flooded with
packet-encode timeouts and 8 was the floor.

Format and clippy stay on depot deliberately: they saturate every core they are
given and would otherwise contend with a measurement, and they gain nothing from
stable hardware.

## Provisioning

The boxes are provisioned out of the **gitops** repo, not here:
`ansible/playbooks/install-github-hetzner-runner.yaml` (see `ansible/README.md`
there). Two servers, **one** runner instance each
(`github_hetzner_runner_instances: 1`, set in gitops commit `1d25673`), registered at
the **org** level with the single label `hetzner`.

```bash
# gitops repo
just install-github-hetzner-runner

# on the box
sudo journalctl -u 'github-hetzner-runner@*' -f
sudo systemctl restart 'github-hetzner-runner@*'
```

### Prerequisites the box must satisfy

Unlike a hosted VM, nothing is reinstalled per run:

- **Nix, multi-user, installed on the box.** This is the one that bit us: the
  first CI run on this runner died in 13s with

  ```
  sudo: a terminal is required to read the password
  sudo: a password is required
  ```

  `hopr-workflows/actions/setup-nix` only skips installation when `nix` is already
  on PATH; otherwise it falls through to `install-nix-action`, which needs root
  that the `runner` system user does not have. Giving `runner` passwordless sudo
  is _not_ a fix — the install would succeed once, then every later run would find
  `command -v nix` false again (`install-nix-action` exports PATH via
  `GITHUB_PATH`, which is per-run) and the installer would refuse because `/nix`
  already exists.

  So the hetzner jobs do not use `setup-nix` at all. They run a `Locate nix` step
  that finds nix on PATH, or at `/nix/var/nix/profiles/default/bin` or
  `~/.nix-profile/bin`, and fails with a pointer here if it is genuinely absent.
  The explicit profile path matters because a multi-user install exports nix
  through `/etc/profile.d/nix.sh`, which a **non-login** job shell never sources.
  `setup-nix` is still used by the depot `lint` job, where it works.

  The install itself belongs in the gitops role, roughly:

  ```yaml
  - name: Check whether nix is installed
    ansible.builtin.stat:
      path: /nix/var/nix/profiles/default/bin/nix
    register: nix_bin

  - name: Install nix (multi-user)
    ansible.builtin.shell:
      cmd: >-
        curl -L https://nixos.org/nix/install
        | sh -s -- --daemon --yes
    when: not nix_bin.stat.exists

  - name: Enable flakes and the hoprnet substituter
    ansible.builtin.copy:
      dest: /etc/nix/nix.conf
      mode: "0644"
      content: |
        experimental-features = nix-command flakes
        substituters = https://cache.nixos.org https://hoprnet.cachix.org
        trusted-public-keys = cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY= hoprnet.cachix.org-1:FzIaDwgsZOy42i2h0qyQ/k9kkggzoeTWmoo/2ehEr90=
        trusted-users = root runner
    notify: restart nix-daemon
  ```

  The `hoprnet` key above is the live one from
  `https://app.cachix.org/api/v1/cache/hoprnet` as of 2026-09-02; re-check it if
  the cache is ever rotated, because a wrong key silently disables the cache
  rather than erroring.

- **`git`, the actual binary.** Nix evaluation shells out to `git` to fetch git
  dependencies (hoprd's flake pulls hopr-lib from `github.com/hoprnet/hoprnet`),
  and the hoprnet dev shell does not provide one — it inherits the host's. A box
  without git fails deep inside a derivation eval with
  `executing "git": No such file or directory`. **`actions/checkout` does not
  reveal this**: it falls back to a tarball download when git is absent, so the
  checkout goes green and hides the gap. `integration.yaml` now takes git from
  nixpkgs when the host has none, but installing it on the box is the better fix —
  a CI runner without git is a trap for every future workflow.
- **The HOPR Cachix substituters in `/etc/nix/nix.conf`** (`hoprd` _and_
  `hoprnet` — `setup-nix` derives the cache name from the repo basename, so the
  two repos publish to two caches). The action's `cachix-action` step is skipped
  once nix is pre-installed, so these have to be on the box; they cannot be
  supplied from the workflow, because the nix daemon ignores substituters offered
  by an untrusted client. Both caches are public, so **no auth token is
  involved** — a Cachix token is a _push_ credential, and nothing here pushes.

  Do not expect this to speed up the hoprd build. Measured 2026-09-03 with
  `nix build --dry-run` against hoprd `release/4.1`
  `packages.x86_64-linux.binary-hoprd-x86_64-linux`: **1015 derivations built,
  201 fetched — identical with and without both HOPR caches.** Neither cache
  carries hoprd's x86_64-linux musl outputs (nor `main`'s, nor the
  `hoprd-deps` cargoArtifacts). So the caches help the shared nixpkgs and
  dev-shell closure only, and hoprd compiles from source either way. Worth
  configuring anyway — free, and it pays off the moment hoprd's CI publishes
  those paths, which is the real fix if these builds need to be fast.

- **Disk headroom for the nix store.** Each run adds a fresh hoprd + blokli
  closure. The workflow GCs (`nix-collect-garbage --delete-older-than 7d`) only
  when `/nix` drops below 50 GB free, so a warm store survives the common case.
- **Room under `~runner/.cache`** for the cargo target dirs. `actions/checkout`
  runs `git clean -ffdx`, which deletes the gitignored `integration/target` on
  every run — so both workflows redirect `CARGO_TARGET_DIR` to
  `$HOME/.cache/hopr-integration-tests/cargo-target-$RUNNER_NAME`, outside the workspace and
  keyed per runner instance (concurrent jobs would otherwise serialise on cargo's
  target-dir lock). Nothing prunes these; delete them by hand if the disk fills.

`harden-runner` still guards the depot `lint` job but is absent from the `unit`
job: it installs an eBPF egress monitor and needs sudo, and only supports
GitHub-hosted runners.

### Dedicating the box — two gitops changes still open

A throughput number is only comparable to another number from the same idle
machine, so the box has to be **dedicated to hopr-integration-tests** and **run one job at a
time**. Neither is achievable from this repo — both are gitops / org settings:

1. **Restrict the runners to this repository.** They are registered at the _org_
   level, so today any hoprnet repo can schedule onto them. The mechanism is a
   GitHub **runner group** scoped to `hopr-integration-tests` (Org → Settings → Actions →
   Runner groups: limit repository access to `hopr-integration-tests`, leave _Allow public
   repositories_ off), not a label — a label expresses a preference, it does not
   deny anyone. Renaming the label would also work but breaks every workflow
   referencing it, so prefer the group.
2. ~~One runner instance per box~~ — **already done.** The role sets
   `github_hetzner_runner_instances: 1`, so a box runs one job at a time and cannot
   contend with itself. Note the consequence: PR `unit` jobs queue behind a ~60
   minute integration run rather than running beside it.

So a slow measurement on this box is **not** explained by parallel jobs on the same
box — one instance rules that out. With two boxes and one instance each, a `unit` job
and an `integration` job land on different machines. What remains as an explanation is
the machine itself (per-core speed, core count) or something outside the box, not
self-contention.

Both boxes register the _same_ `hetzner` label, so a run lands on either one — a
baseline established on one box is only a baseline for that box. If the two ever
differ in spec, give this workflow its own label.

## Repo secrets (hopr-integration-tests)

Set under Settings → Secrets and variables → Actions:

| Secret                         | Used for                                   |
| ------------------------------ | ------------------------------------------ |
| `CACHIX_AUTH_TOKEN`            | hoprnet nix cache (avoid full compiles)    |
| `ZULIP_API_KEY`, `ZULIP_EMAIL` | red-run notification (HOPRd / integration) |

The `bloklid-anvil` image is in a **public** GCP Artifact Registry repo
(`hoprassociation/docker-images`, `allUsers` reader) — no registry credentials
needed to pull it. CI does not use it at all (binary chain); it is a local-only
alternative path.

The upstream repos do not use a PAT to reach this one. They mint a short-lived
**GitHub App token** instead — `vars.GH_APP_HOPRNET_BOT_CLIENT_ID` plus
`secrets.GH_APP_HOPRNET_BOT_PRIVATE_KEY`, scoped to `owner: hoprnet` /
`repositories: hopr-integration-tests` — the same pattern as the cross-repo
dispatches in blokli's and hoprd's `merge.yaml`. See the prerequisites below.

Optional repo _variables_:

| Variable     | Default        | Meaning                                                                |
| ------------ | -------------- | ---------------------------------------------------------------------- |
| `HOPRD_LINE` | `release/4.1`  | hoprd release line the binaries and any dispatched rev must belong to  |
| `HOPRD_REF`  | `$HOPRD_LINE`  | hoprd ref override                                                     |
| `EDGLI_REF`  | `release/4.1`  | edge-client ref override (the v4 line — `main` is v5 since #151)       |
| `BLOKLI_REF` | `release/0.13` | blokli ref override (default is a moving branch, not a release number) |

There are no gate variables — thresholds are hardcoded in
`integration/tests/integration.rs`.

## hoprd v4 / v5 split

Both lines are supported, one per run: `LINE=v4` (default) builds hoprd from
`release/4.1` against edge-client `release/4.1` and blokli `release/0.13`; `LINE=v5`
builds from `main` against edge-client `main` and blokli `v0.14.0`, and adds the PIX
suite. `release/4.1` is the only v4 branch hoprd has — `4.0` exists as a hoprnet branch
and as hoprd tags `v4.0.x`, not as a hoprd branch. The dependency sets live side by side
in the crate (`Cargo.toml` = v4, `Cargo.v5.toml` = v5) and `run.sh` swaps the v5 one in
for the run; the test bodies are shared source.

`run.sh` **rejects** a dispatched hoprd rev not contained in `HOPRD_LINE`, before
spending a build on it (bypass: `HOPRD_SKIP_LINE_CHECK=1`). Mixing the lines is not a
matter of taste: blokli `release/0.13` cannot bootstrap a v5 localcluster at all
(`service_registry` first appears in v0.14.0), and edge-client `main` repinned hopr-lib
to hoprnet `master` in #151.

**Upstream side:** each repo dispatches with the line matching the branch it merged —
`client_payload[line]=v5` from a `main` merge, omitted (v4) from a `release/*` one. A v4
gate that fires on a `main` merge fails fast with
`hoprd ref '<sha>' is not on the 'release/4.1' line` instead of running a mismatched
stack.

### How the upstream repos call in

| Trigger                               | Where                      | Behaviour                                                 |
| ------------------------------------- | -------------------------- | --------------------------------------------------------- |
| nightly 02:00 UTC                     | this repo, `main`          | The v4 lines' only automatic coverage — tips of all three |
| PR labelled `run-integration`         | hoprd, edge-client, blokli | Waits, so the verdict is a check on that PR               |
| merge queue                           | this repo, `main`          | Blocks this repo's own merges                             |
| `merge_group` on a `release/*` branch | hoprd, edge-client, blokli | **Cannot fire** — see below                               |

**A merge queue can only be attached to a repository's default branch.** All three
upstream repos develop v4 on `release/*`, so none of them can gate merges into those
branches. Their gate jobs keep the `merge_group` condition for the day that restriction
lifts, but today only the label path fires there — which is why the nightly exists.

The upstream gates use `repository_dispatch` to start the run and then **wait** on it,
which needs a `marker`: a caller cannot otherwise identify its own run, since
`repository_dispatch` returns nothing identifying, the API does not expose a run's
dispatch inputs, and "newest run" is a race because three repos dispatch here and the
`integration` concurrency group makes runs queue rather than start. The caller passes a
unique marker, `run-name` puts it in the run title, and the caller polls for it.

**Every dispatch names its line, and the default is v4.** A caller adds
`client_payload[line]=v5` (or `-f line=v5` for `workflow_dispatch`) when the rev it is
gating is on the v5 side; omitting it asks for v4. `run.sh` then rejects a rev that is
not contained in that line's hoprd branch, so a v5 sha dispatched without `line=v5`
fails in the first minute with the reason rather than after a 40-minute build.

**hoprd's existing gate is scoped to the v4 line on purpose** — it keys on
`vars.MAINTENANCE_RELEASE_BRANCH` rather than a literal branch, so a bump to
`release/4.2` needs no workflow edit, and it sends no `line`, which is v4. Gating v5 is
a merge queue on hoprd `main` (a queue can only attach to a repository's default
branch), and that gate has to send `line=v5`.

**A PR head is `ahead` of the line, not contained in it.** The containment check in
`run.sh` accepts `identical`/`behind`/`ahead` and rejects only `diverged` — without
`ahead` the label-triggered PR runs, which is exactly what the upstream repos fire,
would all be refused.

### This repo gates its own changes too

`integration.yaml` also runs on `merge_group` for hopr-integration-tests's own `main`. The
`run-integration` label is opt-in, so without a queue gate an unlabelled PR could
change the harness, the thresholds or the scripts and merge without the test ever
running — the one repo where that matters most. The job already admitted any
non-`pull_request` event, so only the trigger had to be added.

Merge-queue runs share the non-cancelling `integration` concurrency group with the
dispatch runs. That is deliberate: cancelling a queue run fails the queue entry, and
the runner executes one job at a time anyway. The cost is that a candidate can wait
behind an upstream gate run, so keep the queue's max wait comfortably above the
~30-40 minute runtime.

**hopr-integration-tests has no rulesets at all** as of 2026-09-04 — `main` is unprotected, so
this trigger fires but nothing enforces it. To make it a real gate, create a ruleset
for `main` with a merge queue rule and both `Integration throughput (v4)` and
`Integration throughput (v5)` among its required status checks. The line is part of the
job name because a queue candidate runs one job per line; `Integration throughput`
without a suffix matches neither. Same for `Format + clippy (v4|v5)` and
`Unit tests (v4|v5)`. Do **not** require `Validate title` — it is PR-only and cannot
report on a merge group.

### Prerequisites in the upstream repos

- **`run-integration` label** — present in hoprd, edge-client and blokli.
- **A GitHub App installed on `hoprnet` with access to `hopr-integration-tests`.**
  The gates mint a token per run via `actions/create-github-app-token`
  (`vars.GH_APP_HOPRNET_BOT_CLIENT_ID` + `secrets.GH_APP_HOPRNET_BOT_PRIVATE_KEY`).
  The installation needs **Actions: write** — write to dispatch, and read (implied
  by write) to list and watch runs. A dispatch-only grant makes a waiting gate fire
  the run and then fail with `never found a run tagged …`, which looks like a
  problem in this repo but is not.

  The older `HOPRD_TEST_DISPATCH_TOKEN` PAT is no longer used and its name predates
  the rename; there is nothing to migrate, but do not re-add it.

### Nightly is the v4 coverage

A **merge queue can only be attached to a repository's default branch.** hoprd,
edge-client and blokli all develop v4 on `release/*` branches, so none of them can
gate merges into those branches — the `merge_group` condition in their `pr.yaml`
gates can never fire for a release branch, and only their `run-integration` label
path works on demand.

The nightly `schedule` here fills that hole. It passes no inputs, so `run.sh` falls
through to its defaults, which are exactly the v4 lines:

| Project     | Nightly ref                   |
| ----------- | ----------------------------- |
| hoprd       | `release/4.1` (`HOPRD_LINE`)  |
| edge-client | `release/4.1` (`EDGLI_REF`)   |
| blokli      | `release/0.13` (`BLOKLI_REF`) |

Each is resolved to its current head per run, so the nightly always tests the tips of
the three v4 branches together. A red night posts to Zulip with the trigger shown as
`nightly v4 line` plus the three resolved revisions.

Scheduled runs only ever fire from the default branch, and they share the
non-cancelling `integration` concurrency group, so a nightly queues behind an
in-flight run rather than cancelling it.

### What the gate runs

Every scenario whose verdict is trustworthy, on every run — 3 on v4, 5 on v5, each with
its own fresh chain, ~17 min of test time on v4 (~35 min including build):

| Suite              | Scenarios                                  | Line |
| ------------------ | ------------------------------------------ | ---- |
| `integration`      | `zero_hop`, `one_hop`                      | both |
| `exit_origination` | the unresolvable-return-path repro         | both |
| `pix`              | deposits swept; session closes when unpaid  | v5   |

**`return_path` is held out entirely.** Every scenario in it asserts an arrival ratio
over a relayer draw the test does not force, so the verdict tracks the draw rather than
the code: `spread` asserts the histogram is spread, and the survival scenarios assert
≥83% arrival after killing one of three return relayers — reachable only if the victim
carried roughly its share. A 2026-09-08 v4 run failed at 67.9% with the victim holding
53% of replies (45/32/23), which is the mechanism rather than a slow machine. Fixing it
means scaling the gate by the victim's measured share, or forcing the spread before the
kill. Run them by hand with `just return-path <name>`.

`rotsee` is excluded (needs a funded Gnosis identity and a reachable public exit) and
so is `profiling` (emits traces, not a verdict, and needs `--features prof` +
`--profile tracer` + `tokio_unstable`).

Suites do not short-circuit: a failure in one still runs the rest, so a red run
reports everything that is broken rather than only the first thing.

### The failure notification

A red run posts to Zulip (stream **HOPRd**, topic **integration**) naming the
trigger and the three versions that actually ran, e.g.

```text
**Integration throughput test FAILED** (v4) — PR [#22](…) (`em/hookup-ci`)
* hoprd `release/4.1`
* edge-client `main (58564a22)`
* blokli `release/0.13`
[Run #48](…) · logs are attached to the run as an artifact.
```

The versions come from `run.sh`, which writes `RESOLVED_*` to `$GITHUB_ENV` once it
has resolved them — **not** from the dispatch inputs. Those inputs only ever carry
the triggering project's rev, so a manual or PR-label run had nothing to report and
the message used to read `for manual run (rev: )`. `unresolved` in a bullet means the
job died before `run.sh` got that far, which points at the runner rather than the
stack.

## Triggering / validating

- **On a hopr-integration-tests PR:** add the `run-integration` label → the test runs on
  **both** lines, one job each, back to back on the single runner.
- **Manual:** `gh workflow run integration.yaml -R hoprnet/hopr-integration-tests`
  then `gh run watch -R hoprnet/hopr-integration-tests --exit-status`.
- **Simulate a merge trigger:**
  ```bash
  gh api repos/hoprnet/hopr-integration-tests/dispatches \
    -f event_type=integration \
    -f 'client_payload[line]=v4' \
    -f 'client_payload[project]=edge-client' \
    -f 'client_payload[rev]=<edge-client release/4.1 sha>'
  ```

Concurrency: a new push to a PR cancels that PR's in-progress run; dispatch/manual
runs share a global group that never cancels, so they stack and run one at a time.
