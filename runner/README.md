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
there). Two servers, four runner instances each — `github-hetzner-runner@1..4` —
registered at the **org** level with the single label `hetzner`.

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
2. **One runner instance per box** — `github_hetzner_runner_instances: 1` in the
   gitops role, down from 4. Four instances means up to four concurrent jobs on
   one machine's cores. Per-workflow concurrency groups cannot fix this: they
   serialise runs _within_ one workflow, so `pr.yaml`'s `unit` job would still run
   alongside an `integration.yaml` measurement. One instance serialises the whole
   machine, which is what a measurement needs; PR unit tests then queue behind a
   60–90 minute run.

Until both land: if the numbers move without a code change, check what else was
scheduled on the box. Two concurrent chains would also collide on ports 8080/8545.

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

hoprd `main` is **v5**; this test targets **v4**. The integration crate pins
`hopr-lib` to hoprnet `release/4.0` (which edge-client `main` also resolves), so
a v5 hoprd binary would be paired with a v4 library set. `run.sh` therefore
builds from `HOPRD_LINE` (`release/4.1` — the only v4 branch hoprd has; `4.0`
exists as a hoprnet branch and as hoprd tags `v4.0.x`, not as a hoprd branch) and
**rejects** a dispatched hoprd rev not contained in that line, before spending a
build on it. Bypass with `HOPRD_SKIP_LINE_CHECK=1`.

**hoprd side:** its merge workflow must only dispatch from `release/4.1`. A
dispatch fired on a `main` merge now fails fast with
`hoprd ref '<sha>' is not contained in 'release/4.1'` instead of running a
mismatched stack. When v5 gets its own line, run this workflow twice with
different `HOPRD_LINE` values rather than loosening the check.

### How the upstream repos call in

| Repo                          | When                                                       | Behaviour                                                                               |
| ----------------------------- | ---------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| hoprd                         | merge to `vars.MAINTENANCE_RELEASE_BRANCH` (`release/4.1`) | **Blocks.** Runs before `build-docker`, so a red gate means no image, no tag, no deploy |
| edge-client                   | merge to `main`                                            | Fire-and-forget; failure reported by the Zulip message below                            |
| blokli                        | merge to `release/0.13`                                    | Fire-and-forget; same                                                                   |
| all three                     | PR labelled `run-integration`                              | Waits, so the verdict is a check on that PR                                             |
| hopr-integration-tests itself | merge queue to `main`                                      | Blocks its own merges — see below                                                       |

Two mechanisms, deliberately:

- **Fire-and-forget** uses `repository_dispatch`. Right where there is nothing to
  gate — edge-client has no image, and blokli's own image is not held back — since
  blocking would only park a runner for the 30–40 minutes the test takes.
- **Waiting** uses `workflow_dispatch` with a `marker` input. A caller cannot
  otherwise identify its own run: `gh workflow run` returns no run id, the API does
  not expose a run's dispatch inputs, and "newest run" is a race because three
  repos dispatch here and the `integration` concurrency group makes runs queue
  rather than start. So the caller passes a unique marker, `run-name` puts it in the
  run title, and the caller polls for the run whose title carries it.

**hoprd's gate is scoped to the v4 line on purpose.** hopr-integration-tests only supports v4
(its crate pins hoprnet `release/4.0`) and rejects a rev off that line, so pointing
the gate at `main` (v5) would fail every merge. Keying on
`vars.MAINTENANCE_RELEASE_BRANCH` rather than a literal branch means a bump to
`release/4.2` needs no workflow edit.

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
for `main` with a merge queue rule and `Integration throughput` among its required
status checks.

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

### What the gate runs

Every suite a local cluster can drive, on every run — 8 scenarios, each with its own
fresh chain, ~44 min of test time (~62 min including build):

| Suite              | Scenarios                                                                             |
| ------------------ | ------------------------------------------------------------------------------------- |
| `integration`      | `zero_hop`, `one_hop`                                                                 |
| `return_path`      | spread, return-relayer loss, forward-relayer loss, common-mode outage, symmetric loss |
| `exit_origination` | the unresolvable-return-path repro                                                    |

`rotsee` is excluded (needs a funded Gnosis identity and a reachable public exit) and
so is `profiling` (emits traces, not a verdict, and needs `--features prof` +
`--profile tracer` + `tokio_unstable`).

Suites do not short-circuit: a failure in one still runs the rest, so a red run
reports everything that is broken rather than only the first thing.

### The failure notification

A red run posts to Zulip (stream **HOPRd**, topic **integration**) naming the
trigger and the three versions that actually ran, e.g.

```text
**Integration throughput test FAILED** — PR [#22](…) (`em/hookup-ci`)
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

- **On a hopr-integration-tests PR:** add the `run-integration` label → the test runs against
  the hoprd v4 line, edge-client main, and blokli `release/0.13`.
- **Manual:** `gh workflow run integration.yaml -R hoprnet/hopr-integration-tests`
  then `gh run watch -R hoprnet/hopr-integration-tests --exit-status`.
- **Simulate a merge trigger:**
  ```bash
  gh api repos/hoprnet/hopr-integration-tests/dispatches \
    -f event_type=integration \
    -f 'client_payload[project]=edge-client' \
    -f 'client_payload[rev]=<edge-client main sha>'
  ```

Concurrency: a new push to a PR cancels that PR's in-progress run; dispatch/manual
runs share a global group that never cancels, so they stack and run one at a time.
