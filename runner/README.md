# Runner

Jobs are split by whether they **execute a test** or only compile:

| Job                                | Runner                 | Why                                     |
| ---------------------------------- | ---------------------- | --------------------------------------- |
| `pr.yaml` → `validate-pr-title`    | `depot-ubuntu-24.04`   | metadata only                           |
| `pr.yaml` → `lint` (fmt + clippy)  | `depot-ubuntu-24.04-4` | compile-time checks, timing-insensitive |
| `pr.yaml` → `unit`                 | `depot-ubuntu-24.04-4` | no cluster, timing-insensitive          |
| `integration.yaml` → `integration` | **self-hosted**        | the throughput gate                     |

The integration suites run on **dedicated self-hosted boxes** (the label in
`integration.yaml`), so a result here and a result there come from the same hardware. A
dedicated machine rather than a hosted VM because the onion encoder is CPU-bound
and the throughput thresholds only mean anything on a stable core count — on the
hosted runners the gate used to sit on, the 4-vCPU size already flooded with
packet-encode timeouts and 8 was the floor.

Format, clippy and the unit tests stay on depot deliberately: they saturate every core they are
given and would otherwise contend with a measurement, and they gain nothing from
stable hardware.

## Trust boundary

Untrusted fork-PR code must never run on the persistent self-hosted runner. The box is
shared with the trusted merge-gate jobs, and anything a PR leaves behind (cargo target
dir, `~/.cargo/config.toml`, `~/.ssh`, a `git` or `nix` shim on `PATH`) would run inside
later gate jobs. Unit tests, lint and title checks therefore run on hosted depot runners.
Only `integration.yaml` → `integration` uses `[self-hosted, hetzner]`. That job also
runs on `pull_request`, but only for a PR a maintainer has labelled `run-integration*`.
**Never label a fork PR `run-integration*`**: the label routes its code to the persistent
runner, and reading the diff does not isolate that box. Push the branch to this repository
instead. Do not move a new job onto `[self-hosted, hetzner]` unless it is trusted (merge
queue, dispatch, schedule, or a labelled PR from a branch of this repository).
Rule from hoprnet/hopr-devops#355.

## Provisioning

The boxes are provisioned from the internal infrastructure repo, not here. Two boxes,
**one** runner instance each, registered at the **org** level with a single label and a
runner name ending in `-<box>-<instance>`. So up to 2 jobs run at once, one per box.
`max-parallel: 4` in `integration.yaml` is only a ceiling; the runner count is the limit.

One instance per box on purpose: with two, runs failed next to any
neighbour — `pix_shapes` evicted PIX shares from the Exit's SURB store and `2-hop` dropped
to ~0.17 MB/s (0.28 alone).

Should a box ever run two instances again, the integration job keeps them apart itself: the
"Claim this runner's ports and temp dir" step reads the instance number from `RUNNER_NAME`
and, for instance N, shifts every port by `(N-1) * 1000` and roots the chain and socket
folders at that runner's `RUNNER_TEMP`:

|                                            | instance 1                 | instance 2                 |
| ------------------------------------------ | -------------------------- | -------------------------- |
| anvil (`ANVIL_PORT`)                       | 8545                       | 9545                       |
| bloklid (`BLOKLI_API_PORT`)                | 8080                       | 9080                       |
| hoprd API (`HOPRD_API_PORT_BASE`)          | 13000+                     | 14000+                     |
| hoprd P2P (`HOPRD_P2P_PORT_BASE`)          | 19000+                     | 20000+                     |
| latency relays (`HOPRD_LATENCY_PORT_BASE`) | 9100+                      | 10100+                     |
| chain + control sockets (`IT_TMP_ROOT`)    | instance 1's `RUNNER_TEMP` | instance 2's `RUNNER_TEMP` |

`reap_nodes`, `chain_stop` and the hygiene steps match processes by their full path under
the job's own checkout, never by name, so neither job kills the other's nodes. Outside CI
none of these variables is set and the defaults are the instance-1 column with `/tmp`.

Installing, restarting and reading the runner service logs is documented with the
provisioning in the infrastructure repo.

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

  So the self-hosted jobs do not use `setup-nix` at all. They run a `Locate nix` step
  that finds nix on PATH, or at `/nix/var/nix/profiles/default/bin` or
  `~/.nix-profile/bin`, and fails with a pointer here if it is genuinely absent.
  The explicit profile path matters because a multi-user install exports nix
  through `/etc/profile.d/nix.sh`, which a **non-login** job shell never sources.
  `setup-nix` is still used by the depot `lint` job, where it works.

  The install itself belongs in the box's provisioning, roughly:

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
  `nix build --dry-run` against hoprd's
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

### Dedicating the runners — one infrastructure change still open

A throughput number is only comparable to another number from the same idle
machine, so the runners have to be **dedicated to hopr-integration-tests**, and ideally a box
runs one job at a time. Neither is achievable from this repo — both are provisioning / org settings:

1. **Restrict the runners to this repository.** They are registered at the _org_
   level, so today any hoprnet repo can schedule onto them. The mechanism is a
   GitHub **runner group** scoped to `hopr-integration-tests` (Org → Settings → Actions →
   Runner groups: limit repository access to `hopr-integration-tests`, leave _Allow public
   repositories_ off), not a label — a label expresses a preference, it does not
   deny anyone. Renaming the label would also work but breaks every workflow
   referencing it, so prefer the group.
2. **One job per box** — one runner instance each (above). The PR unit tests run on
   depot for the same reason: nothing else should take a box from an integration job.

All runners register the _same_ label, so a run lands on any of them — a
baseline established on one box is only a baseline for that box. If the boxes ever
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

| Variable     | Default | Meaning                                                                |
| ------------ | ------- | ---------------------------------------------------------------------- |
| `HOPRD_REF`  | `main`  | hoprd ref override                                                     |
| `EDGLI_REF`  | `main`  | edge-client ref override                                               |
| `BLOKLI_REF` | `main`  | blokli ref override (default is a moving branch, not a release number) |

There are no gate variables — thresholds are hardcoded in
`integration/tests/integration.rs`.

## How the upstream repos call in

| Trigger                       | Where                      | Behaviour                                   |
| ----------------------------- | -------------------------- | ------------------------------------------- |
| nightly 02:00 UTC             | this repo, `main`          | Both tiers against the tips of all three    |
| PR labelled `run-integration` | hoprd, edge-client, blokli | Waits, so the verdict is a check on that PR |
| merge queue on `main`         | hoprd, edge-client, blokli | Waits, and blocks the upstream merge        |
| merge queue                   | this repo, `main`          | Blocks this repo's own merges               |

The upstream gates use `repository_dispatch` to start the run and then **wait** on it,
which needs a `marker`: a caller cannot otherwise identify its own run, since
`repository_dispatch` returns nothing identifying, the API does not expose a run's
dispatch inputs, and "newest run" is a race because three repos dispatch here and a run
queues rather than starts while every runner is busy. The caller passes a
unique marker, `run-name` puts it in the run title, and the caller polls for it.

The dispatch payload carries `project`, `rev`, `marker` and optionally `tests_pr` and `tier`. Any other key
(an old `line`) is ignored.

### This repo gates its own changes too

`integration.yaml` also runs on `merge_group` for hopr-integration-tests's own `main`. The
`run-integration` label is opt-in, so without a queue gate an unlabelled PR could
change the harness, the thresholds or the scripts and merge without the test ever
running — the one repo where that matters most. The job already admitted any
non-`pull_request` event, so only the trigger had to be added.

Merge-queue runs compete with the dispatch runs for the same four runners. There is no
job-level concurrency group, so nothing cancels a queued run (which would fail the queue
entry); a job just waits for a free runner. With more than four jobs pending a candidate
can still wait behind others, so keep the queue's max wait comfortably above that. A queue candidate
itself runs the `pr` tier, about 45 minutes, against the ruleset's 320-minute check timeout.

**hopr-integration-tests has no rulesets at all** as of 2026-09-04 — `main` is unprotected, so
this trigger fires but nothing enforces it. To make it a real gate, create a ruleset
for `main` with a merge queue rule and the fan-in `Integration throughput` among its required
status checks. It passes only when every job of the run did. The same goes for `Format + clippy`
and `Merge gate`. Do **not** require `Validate title` — it is PR-only and cannot
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

### Nightly

The nightly `schedule` runs the `pr` tier and the `nightly` tier's two shards side by side, `a`
(exit_origination, wireguard, pix_relay_loss) and `b` (pix_shapes, return_path, pix_resume), against
run.sh's defaults: the current head of hoprd, edge-client and blokli `main`. A red night posts to
Zulip with the trigger shown as `nightly` plus the tier and shard, the three resolved revisions and
the failures from the end-of-run summary.

Scheduled runs only ever fire from the default branch, and wait for a free runner like
every other run, so a nightly never cancels an in-flight run or is cancelled by one.

### What the gate runs

The suites are split in two tiers (`TIER`, see [docs/test-matrix.md](../docs/test-matrix.md)),
each its own job. The merge queue and upstream dispatches run `pr`, the schedule runs both, and PR
labels pick a slice (table in the [README](../README.md#ci)).

| Suite              | Scenarios                                  | Tier    |
| ------------------ | ------------------------------------------ | ------- |
| `integration`      | `zero_one_two_hop`                         | pr      |
| `dead_last_hop`    | a relay that acks and delivers nothing     | pr      |
| `pix`              | deposits swept; session closes when unpaid | pr      |
| `pix_shapes`       | `a_mixed_session_sustains_its_cycles`      | pr      |
| `exit_origination` | the unresolvable-return-path repro         | nightly |
| `pix_shapes`       | browsing, download, idle, upload           | nightly |
| `return_path`      | every scenario but the tempering finding   | nightly |
| `pix_relay_loss`   | every scenario                             | nightly |
| `pix_resume`       | every scenario                             | nightly |

Every run ends with a pytest-style summary (console and job summary) listing each test,
and for a failure the step that failed and why.

`rotsee` is excluded (needs a funded Gnosis identity and a reachable public exit) and
so is `profiling` (emits traces, not a verdict, and needs `--features prof` +
`--profile tracer` + `tokio_unstable`).

Suites do not short-circuit: a failure in one still runs the rest, so a red run
reports everything that is broken rather than only the first thing.

### The failure notification

A red run posts to Zulip (stream **HOPRd**, topic **integration**) naming the
trigger and the three versions that actually ran, e.g.

```text
**Integration throughput test FAILED** (pr tier) — PR [#22](…) (`em/hookup-ci`)
* hoprd `main`
* edge-client `main (58564a22)`
* blokli `main (1a2b3c4d)`
[Run #48](…) · logs are attached to the run as an artifact.
```

The versions come from `run.sh`, which writes `RESOLVED_*` to `$GITHUB_ENV` once it
has resolved them — **not** from the dispatch inputs. Those inputs only ever carry
the triggering project's rev, so a manual or PR-label run had nothing to report and
the message used to read `for manual run (rev: )`. `unresolved` in a bullet means the
job died before `run.sh` got that far, which points at the runner rather than the
stack.

## Triggering / validating

- **On a hopr-integration-tests PR:** `run-integration` runs the `pr` tier,
  `run-integration-nightly` the `nightly` tier. Labels add up; the jobs run side by side, one per box.
- **Manual:** `gh workflow run integration.yaml -R hoprnet/hopr-integration-tests`
  then `gh run watch -R hoprnet/hopr-integration-tests --exit-status`.
- **Simulate a merge trigger:**
  ```bash
  gh api repos/hoprnet/hopr-integration-tests/dispatches \
    -f event_type=integration \
    -f 'client_payload[project]=edge-client' \
    -f 'client_payload[rev]=<edge-client main sha>'
  ```

Concurrency: a new push to a PR cancels that PR's in-progress run. Otherwise runs and
their matrix legs (`max-parallel: 4`) start as soon as a self-hosted runner is free — one
per box, two at once.
