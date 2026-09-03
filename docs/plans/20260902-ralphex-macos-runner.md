# ralphex-macos-runner: a native runner daemon for ralphex-farm

## Overview

`ralphex-macos-runner` is a second, independent implementation of the ralphex-farm runner protocol. It runs on a Mac as a LaunchAgent and executes ralphex **natively in an existing checkout** - the way the operator runs `ralphex docs/plans/x.md` by hand today - instead of inside a Docker task container. It exists for work the container cannot do (Xcode builds, SwiftUI tests against a simulator) while keeping everything the farm gives a run: a row in the runstore, live logs on the dashboard, a lease, cancel from the dashboard, and a pull request at the end.

Two binaries ship from one crate:

- `ralphex-macos-runner` - the daemon. Long-polls the farm for jobs, runs ralphex, streams output, heartbeats, reports completion, opens the pull request.
- `rxd` - the local client. `rxd <plan>` from a project directory opens a run on the farm **without a Linear ticket**, hands it to the daemon, and streams the output to the terminal; Ctrl-C detaches and the run keeps going; `rxd attach` reconnects. `rxd install` registers the daemon with launchd.

Both entry points - a Linear ticket with `runtime: native` and `rxd` - converge on the same execution path inside the daemon. The only difference is who opened the run and whether a terminal is attached.

This is plan 2 of 2. Plan 1 (`ralphex-farm`, `docs/plans/20260901-native-runtime-farm.md` there) teaches the farm to route by runtime class, to open ticketless runs via `POST /api/runner/runs`, and to refuse a foreign protocol version. **Every task in this plan, task 11 included, runs against a fake farm and needs nothing from plan 1.** The live smoke against the deployed farm is a Post-Completion item and is the only step that needs plan 1 merged and deployed.

### Non-goals

- **No cloning, fetching, resetting or cleaning of the checkout.** The daemon opens the directory the job names and runs ralphex there, in whatever state it is. Reproducibility from a pristine branch is the container runner's contract, not this one's.
- **No repository list, projects root or path mapping.** The checkout path arrives in every job (`Job.ctx`), or is the current directory for `rxd`.
- **One run at a time.** The slot count is fixed at 1 in this version: two ralphex processes in one checkout without worktrees would write over each other, and the operator's container runner on this host already runs with one slot. `ClaimRequest.slots` is always 1.
- **No worktree by default.** ralphex creates the feature branch itself; `--worktree` is passed through only when `rxd --worktree` asks for it.
- **No restoring the previous branch after a run.** A by-hand run leaves the checkout on the feature branch; so does this.
- **No pull request description generation.** The pull request gets a short fixed body; the farm's finalize-prompt machinery is not involved and `finalize_enabled` in the operator's personal ralphex config is never touched.
- **No plan progress.** `POST .../progress` is not called and the `ProgressRequest` family of types is not defined in this version. When progress ships, its `tasks` field must be modelled as `Option<Vec<_>>` because the farm reads `null` as "keep what you have" and `[]` as "the plan has no tasks".
- **No config-dir override for ralphex.** `RALPHEX_CONFIG_DIR` is never set: the run uses the operator's personal `~/.config/ralphex`, agents and skills included.
- **No `caffeinate`.** The host has sleep disabled; a locked screen does not stop a run.
- **No cancel or process listing in `rxd`.** Both exist on the dashboard; the client stays at run, attach, install, uninstall.
- **No Intel build.** The target is `aarch64-apple-darwin` only; there is no Intel host to run on.
- **No Linux support.** The container runner covers Linux.

### Rejected alternatives

- **Adding a host execution backend to the Go runner instead of a new daemon.** Rejected by the operator: the Mac's background services are Rust daemons with a shared launchd + Homebrew release pipeline, and this one joins that fleet.
- **A shared Go "workflow" helper the daemon shells out to for git, push and pull request.** Rejected: full parity in one language, no cross-language process boundary in the middle of a run.
- **Localhost HTTP for daemon-client IPC.** Rejected in favour of a Unix socket with `0600` permissions: no port, no auth question, and it is the shape the operator's other daemons use.
- **Letting ralphex's finalize step push and open the pull request.** Rejected: the stock finalize prompt rebases and squashes and does not push; enabling it would change the operator's by-hand runs too. The daemon pushes and calls `gh` itself.
- **Reserving the run slot for the whole of a claim long-poll.** Rejected: with one slot, an idle daemon spends 25 of every 25 seconds inside a poll, so nearly every `rxd` would be answered `Busy` by a daemon doing nothing.
- **Aborting an in-flight long-poll when `rxd` arrives.** Rejected: a job the farm dispatched in the instant before the abort is lost on the wire, holds a lease nobody heartbeats, and is finalised as `runner_lost` three minutes later. `rxd` waits for the poll to return instead (at most 25 s) - slower once in a while, never a lost job.
- **Aborting the run when a log chunk is answered `410`.** Rejected in favour of what the Go runner does: a log `410` only stops log delivery; the heartbeat's `410` is what aborts. One rule, one place.
- **`unsafe` calls to `setpgid` and `killpg` through `libc`.** Rejected: `std::os::unix::process::CommandExt::process_group` and the `nix` crate's `killpg` give the same behaviour with no `unsafe` blocks, so the crate carries no comments at all.
- **Copying the conformance vectors from the farm repository at task time.** Rejected: that repository is not reachable from a cold session, and at the time this plan runs its goldens do not yet carry plan 1's fields. The vectors are written into this plan verbatim and cross-checked against the farm after plan 1 merges.

## Skills to invoke

Load each skill below with the Skill tool and follow its conventions before implementing any task in this plan.

- `rust-style` - every file under `src/` and `tests/`: `for` loops over iterator chains, `let ... else` for early exits, newtypes over bare strings, enums over bools, exhaustive `match`, explicit destructuring, no comments.
- `rustdoc` - `///` on every public item (crate-level `//!` in `lib.rs` and each `bin`), RFC 1574 summary sentences, `# Errors` and `# Panics` sections where they apply.
- Use the rust-analyzer LSP (`goToDefinition`, `findReferences`, `documentSymbol`) for navigation, as `rust-style` requires.

## Context (from discovery)

**The farm side (already built, in `ralphex-farm`; pointers for orientation, not a spec to open):**

- Wire contract: `pkg/protocol/protocol.go`; golden JSON round-trips in `pkg/protocol/protocol_test.go`. The post-plan-1 form of those goldens is written out below under "Conformance vectors".
- Reference behaviour the daemon reproduces: `pkg/runner/client.go` (retry, status mapping), `pkg/runner/logstream.go` (buffering, chunking, `seq`, tail, the `410` latch), `pkg/runner/agent.go` (claim loop, slots, the immediate first heartbeat, cancel, drain), `pkg/runner/executor.go:616` (`buildRalphexCmd`), `pkg/github/pr.go:43` (the existing-pull-request check before `gh pr create`).
- Plan 1 adds: `runtime` on `ClaimRequest`/`HeartbeatRequest`/`Job`; `ctx`, `create_pr` on `Job`; `POST /api/runner/runs` taking `OpenRunRequest`; `409` on version mismatch with a `{"error": "..."}` body from the farm's single JSON error writer; `local-<unixmilli>` run ids.

**The operator's daemon pattern (from `~/Projects/turtle-harbor`, orientation only; every mechanism this plan relies on is written out in Technical Details):**

- One crate, `src/lib.rs` plus `src/bin/<daemon>.rs` and `src/bin/<cli>.rs`.
- IPC: length-prefixed JSON over a Unix socket - 4-byte little-endian length, then the JSON payload; `Command` and `Response` enums; a 10 MiB cap on a message.
- Process trees: the child is its own process group; stop is `SIGTERM` to the group, a grace period, then `SIGKILL` to the group.
- Shutdown: a task listens for `SIGTERM` and `SIGINT` and flips a `watch` channel every long-lived task selects on.
- `<cli> install`: copies the daemon binary to a stable path under `~/Library/Application Support/<app>/bin/`, writes `~/Library/LaunchAgents/<label>.plist` with `RunAtLoad`, `KeepAlive`, stdout/stderr under `~/Library/Logs/<app>/`, and **an `EnvironmentVariables.PATH` captured from the installing shell** - launchd agents do not inherit a login shell's PATH, and without it the daemon finds neither `ralphex`, `claude`, `codex`, `gh` nor `xcodebuild`. Then `launchctl bootout gui/<uid> <plist>` (ignored if not loaded) and `launchctl bootstrap gui/<uid> <plist>`.
- Toolchain: `mise.toml` pins tools; CI and the per-task gate run through `mise exec -- cargo` and `mise run check`.
- Release: a `v*` tag triggers a build on `macos-latest`, a check that the tag equals `Cargo.toml`'s version, Developer ID codesign, a `tar.gz`, a GitHub release, and a job that rewrites the formula in `pkarpovich/homebrew-apps` and pushes it with `HOMEBREW_TAP_TOKEN`.

**Host facts that shape defaults:** `ralphex` is `/opt/homebrew/bin/ralphex` (v1.6.1); `gh` is `/opt/homebrew/bin/gh` (2.98); `ruby` is the system 2.6 (enough for `ruby -c`); `actionlint` is not installed but is in mise's registry; the personal ralphex config is `~/.config/ralphex` with `finalize_enabled` at its default `false`; the farm URL and runner token live in the operator's environment repository, not in this one.

**Dependencies (add with `cargo add`, versions resolved at that moment and pinned by `Cargo.lock`; none are stated here from memory):** `tokio` (`rt-multi-thread`, `macros`, `process`, `signal`, `sync`, `time`, `net`, `io-util`), `reqwest` (`json`, `rustls`, default features off), `serde` (`derive`), `serde_json`, `toml`, `clap` (`derive`), `thiserror`, `tracing`, `tracing-subscriber` (`env-filter`), `dirs`, `nix` (`signal`, `process`). Dev: `axum` (the fake farm), `tempfile`.

## Development Approach

- **testing approach**: Regular - implement the task, then write its tests in the same task.
- complete each task fully before moving to the next
- make small, focused changes; no feature beyond what a task names
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
  - tests are not optional - they are a required part of the checklist
  - unit tests live in a `#[cfg(test)] mod tests` at the bottom of the module; integration tests in `tests/`
  - cover both success and error scenarios
- **CRITICAL: all tests must pass before starting next task** - no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- run `mise run check` after each change

## Code-Quality Rules (verify before marking each task complete)

### Rust (from the `rust-style` and `rustdoc` skills)

Non-negotiable; the gate for marking any task complete. If a rule is violated the task is not done - refactor, re-test, then mark complete.

**Control flow and shape:**
- `for` loops with mutable accumulators, not `iter().filter().map().collect()`, `sum()`, `find()` chains.
- `let ... else` for early exits; `if let` only for a short action with no `else`; `match` for several cases.
- Shadow through transformations (`let input = input.trim();`), no `raw_`/`parsed_`/`trimmed_` prefixes.
- `match` covers every variant explicitly - no `_` wildcard (ask before adding one), no `matches!`.
- Destructure structs and tuples explicitly to get compiler errors when fields change, rather than reaching through `value.field` at each use.

**Types:**
- Newtypes for strings with meaning: `RunId(String)`, `Branch(String)`, `Seq(u64)`, `RunnerName(String)`.
- Enums over bools in signatures: `CreatePr::{Yes, No}`, `Worktree::{Yes, No}`.

**Comments:**
- None. No inline explanations, no section dividers, no TODOs, no commented-out code. The crate has no `unsafe`, so no `SAFETY` lines either.
- `///` doc comments are required on every public item and follow `rustdoc`: a one-line summary in third person singular present indicative ending with a period; `# Errors` on every fallible public function; `# Panics` where a panic is possible; `//!` at the top of `lib.rs` and each `bin`. This plan narrows `rustdoc`'s examples rule on purpose: `# Examples` is required only on public functions a doctest can exercise without a farm, a socket or a process (framing, path resolution, plist generation, title and body builders); other public items document behaviour in prose.

**Per-task gate (before marking a checkbox `[x]`):**
1. `mise run check` green: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `actionlint`, and from task 10 on `ruby -c` on the formula template.
2. `grep -rn "^\s*//[^/!]" src/ tests/` returns nothing (no line comments); `grep -rn "\.iter()\.\(filter\|map\)\|\.collect::<\|matches!(" src/` returns nothing new; `grep -rn "=> _\|_ =>" src/` returns nothing new.
3. Every new `pub` item has a `///` block; every new `pub fn` returning `Result` has `# Errors`.
4. Only after 1-3 pass: mark complete.

## Testing Strategy

- **unit tests**: required for every task (see Development Approach above).
- **integration tests** (`tests/`): a hand-rolled fake farm built on `axum` as a dev-dependency (`tests/support/fake_farm.rs`) whose per-route behaviour a test scripts in advance - answer `204`, hand out a given `Job`, hold a claim open until the test releases it, answer `500` twice then `200`, answer `410`, answer `409` with the body from Technical Details, reject a log chunk whose `seq` is below 1 or not above the last accepted one with `400` exactly as the farm does - and which records every request's path, headers and body; a fake `ralphex` (`tests/support/fake-ralphex.sh`) that prints scripted lines to stdout and stderr, can print one unbroken 1 MiB line, sleeps when told, records its argv, cwd and environment, and exits with the code in `FAKE_RALPHEX_EXIT`; fake `git` and `gh` shims in `tests/support/bin/` that record their arguments and print canned answers. Nothing in the automated suite touches the real farm, Linear, Docker, launchd or the network.
- **conformance vectors** (`tests/protocol_vectors.rs`): the golden JSON strings from Technical Details, transcribed verbatim, deserialised into this crate's types and re-serialised to an equal JSON value. They are this plan's statement of the wire; the cross-check against the farm's own goldens is a Post-Completion step.
- **time**: every interval the daemon uses - retry backoff, the 2 s flush tick, the 30 s heartbeat, the 10 s stop grace, the drain timeout - is injected, so no test waits on wall-clock time.
- **live smoke**: Post-Completion only. Not a task, not a checkbox.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope
- keep plan in sync with actual work done

## Solution Overview

```
src/
  lib.rs                 crate docs, module tree
  protocol/
    mod.rs
    types.rs             wire types, serde, constants
    client.rs            FarmClient: the five calls, per-call retry, status mapping
  logstream.rs           buffer -> chunks -> farm; tail; history ring; subscribers
  job.rs                 spawn ralphex in ctx, tee output, stop, exit code
  pr.rs                  existing-pull-request check, git push, gh pr create
  agent.rs               config, the run slot, claim loop, heartbeat, drain, complete
  ipc.rs                 socket framing, Command/Response
  paths.rs               config, socket, log and daemon-binary paths (release/debug)
  config.rs              config.toml schema and loading
  service.rs             launchd plist, install/uninstall
  bin/
    ralphex-macos-runner.rs   daemon entry: agent + socket listener + signals
    rxd.rs                    client entry: run, attach, install, uninstall
tests/
  support/               fake farm, fake ralphex, git/gh shims
  protocol_vectors.rs
  client.rs, logstream.rs, job.rs, pr.rs
  agent_e2e.rs           daemon against fake farm and fake ralphex
  rxd_e2e.rs             client through the socket
```

Key decisions:

- **One execution path, one extra argument.** `agent::run_job(job: protocol::Job, local: LocalOptions)` serves both entry points. `LocalOptions { worktree: Worktree, env: Vec<(String, String)> }` carries what only `rxd` can ask for; the claim path passes `LocalOptions::default()` (`Worktree::No`, no extra env). The `rxd` path additionally subscribes to the run's log stream.
- **One slot, no reservation during a poll.** The agent owns a `RunSlot` that is `Free`, `Polling` or `Running(run_id)`. The claim loop polls only from `Free`, moving to `Polling`; a local request that arrives during `Polling` **waits for that poll to return**: a `204` hands the slot to the local request, a job hands it to the job and the local request is answered `Busy`. During `Running` a local request is answered `Busy` and the claim loop does not poll. Nothing is aborted, so no dispatched job can be lost.
- **Protocol drift is fatal, and the run is stopped first.** `409` on claim or on heartbeat raises a `Terminal::VersionMismatch`; if a job is running its process group is stopped through the normal stop sequence before the daemon exits with status 2. launchd's `KeepAlive` restarts it with its own throttle, and the log line names both versions.
- **`410` follows the Go runner exactly.** On a log chunk it latches the stream as gone and stops delivery; the run continues and still completes. On a heartbeat it stops the process group and the job is dropped without a `complete`. On `complete` itself there is nothing left to do.
- **Cancel is a signal sequence, not a request.** `SIGTERM` to the process group, a 10 s grace, `SIGKILL` to the group; the run then completes as `canceled`.
- **Terminal events travel on one channel.** Heartbeat (`cancel`, `410`, `409`), the drain deadline and the process exit all reach `run_job` through a single `Terminal` enum on a `tokio::sync::mpsc` channel it selects on, so every terminal condition has exactly one handling site.

## Technical Details

### Wire contract

Every call is `POST`, `Authorization: Bearer <token>`, `Content-Type: application/json` unless noted; the run id in a path is percent-encoded. Response bodies are read with a 64 KiB cap.

| Path | Body | Answer |
|---|---|---|
| `/api/runner/claim` | `ClaimRequest` | `200` + `Job`; `204` empty when the long-poll window expired; `409` version mismatch |
| `/api/runner/runs` | `OpenRunRequest` | `200` + `Job`; `400` invalid; `401` bad token |
| `/api/runner/jobs/{id}/log?seq=N` | raw bytes, `application/octet-stream`, at most 64 KiB | `204`; `400` when `seq` is below 1 or not above the last accepted value |
| `/api/runner/jobs/{id}/heartbeat` | `HeartbeatRequest` | `200` + `HeartbeatResponse`; `409` version mismatch |
| `/api/runner/jobs/{id}/complete` | `CompleteRequest` | `200`; a second call `410` |

`410 Gone` on any job call means the farm no longer knows the run. A `409` body is the farm's generic error shape, `{"error": "<message naming both versions>"}`; the daemon logs the message verbatim and does not parse versions out of it. `X-Repos-Generation` on claim and heartbeat answers is ignored (no repositories to resync).

Timeouts: 30 s per ordinary attempt; claim 25 s + 30 s.

### Types

JSON keys exactly as written. `repos` and `ready` are `Option<Vec<_>>` with `#[serde(default)]`: the farm's goldens pin `null` for the zero value (a Go nil slice), the farm's decoder accepts `null` and `[]` alike, and this crate always **sends** `Some(vec![])`, so both the goldens and the constructors round-trip.

```
ClaimRequest      { runner, version, runtime, repos: Option<Vec<RepoCapability>>, ready: Option<Vec<String>>, slots }
RepoCapability    { slug, default_branch }
HeartbeatRequest  { runner, image, version, runtime, repos: Option<Vec<RepoCapability>>, slots }
HeartbeatResponse { action: "none" | "cancel" }
OpenRunRequest    { runner, runtime, repo, ctx, plan, branch, create_pr }
Job               { run_id, issue_id, identifier, issue_url, title, repo_slug, plan_path, branch, mode, lease_ttl_seconds, runtime, ctx, create_pr }
CompleteRequest   { status: "done" | "error", pr_url, fail_reason, message, log_tail }
```

Constants, all `pub const` in `protocol::types`: `VERSION = "1"`, `RUNTIME = "native"`, `SLOTS = 1`, `LEASE_TTL = 180 s`, `HEARTBEAT_INTERVAL = 30 s`, `CLAIM_WINDOW = 25 s`, `LOG_FLUSH_INTERVAL = 2 s`, `MAX_LOG_CHUNK = 65536`, `LOG_TAIL_LINES = 100`, `LOG_TAIL_BYTES = 65536`, `LOG_BUFFER_BYTES = 4 MiB`, `HISTORY_LINES = 2000`, `HISTORY_BYTES = 4 MiB`, `RETRY_BASE_DELAY = 1 s`, `RETRY_MAX_DELAY = 30 s`, `RETRY_MAX_ATTEMPTS = 6`, `COMPLETE_BUDGET = LEASE_TTL`, `LOG_CLOSE_TIMEOUT = 30 s`, `STOP_GRACE = 10 s`.

### Conformance vectors

These strings are the plan's statement of the wire after plan 1 and are transcribed verbatim into `tests/protocol_vectors.rs`. Each is deserialised into the crate's type and re-serialised; the two JSON values must be equal (key order is irrelevant). They are the farm's existing goldens with plan 1's fields added; the Post-Completion cross-check confirms they match the farm's `protocol_test.go` after plan 1 merges.

```
RepoCapability
{"slug":"pkarpovich/ralphex-farm","default_branch":"master"}

ClaimRequest full
{"runner":"mac-1","version":"1","runtime":"native","repos":[{"slug":"owner/one","default_branch":"main"},{"slug":"owner/two","default_branch":"master"}],"ready":["owner/two"],"slots":2}

ClaimRequest empty
{"runner":"","version":"","runtime":"","repos":null,"ready":null,"slots":0}

Job full
{"run_id":"FARM-12-1753180800000","issue_id":"issue-uuid","identifier":"FARM-12","issue_url":"https://linear.app/example/issue/FARM-12","title":"split farm and runner","repo_slug":"owner/repo","plan_path":"/abs/checkout/docs/plans/20260722-farm-runner-architecture.md","branch":"farm-runner-architecture","mode":"review","lease_ttl_seconds":180,"runtime":"native","ctx":"/abs/checkout","create_pr":true}

Job empty
{"run_id":"","issue_id":"","identifier":"","issue_url":"","title":"","repo_slug":"","plan_path":"","branch":"","mode":"","lease_ttl_seconds":0,"runtime":"","ctx":"","create_pr":false}

HeartbeatRequest full
{"runner":"mac-1","image":"ghcr.io/pkarpovich/ralphex:latest","version":"1","runtime":"native","repos":[{"slug":"owner/one","default_branch":"main"}],"slots":2}

HeartbeatRequest empty
{"runner":"","image":"","version":"","runtime":"","repos":null,"slots":0}

HeartbeatResponse
{"action":"cancel"}
{"action":"none"}

OpenRunRequest full
{"runner":"mbp-native","runtime":"native","repo":"ralphex-farm","ctx":"/abs/checkout","plan":"/abs/checkout/docs/plans/x.md","branch":"x","create_pr":true}

OpenRunRequest empty
{"runner":"","runtime":"","repo":"","ctx":"","plan":"","branch":"","create_pr":false}

CompleteRequest done
{"status":"done","pr_url":"https://github.com/owner/repo/pull/7","fail_reason":"","message":"","log_tail":""}

CompleteRequest error
{"status":"error","pr_url":"","fail_reason":"nonzero_exit","message":"ralphex exited with code 2","log_tail":"line one\nline two"}
```

### Behavioural rules the farm depends on

1. **Retry is per operation.**

   | Call | Policy |
   |---|---|
   | `claim` | no retry; a failure is logged and the loop polls again after `RETRY_BASE_DELAY` |
   | `open_run` | **never retried**: it mints a run id and a lease on every call and has no idempotency key, so a retry after a lost response orphans a run. The error goes back to `rxd`. |
   | `append_log` | transport errors and `5xx` retry with backoff 1 s doubling to 30 s, at most 6 attempts; any `4xx` is final |
   | `heartbeat` | as `append_log` |
   | `complete` | transport errors and `5xx` retry with the same backoff **without an attempt limit**, until the farm accepts it, answers `410`, or `COMPLETE_BUDGET` (180 s) has elapsed since the first attempt. Giving up earlier lets the lease expire and the farm finalise a successful run as `runner_lost`. |

2. **`seq` is 1-based and is spent per attempt.** The first chunk of a run carries `seq = 1`; every send attempt, delivered or not, consumes the next value. Gaps are normal; `seq = 0` is rejected by the farm.
3. **Logs are best effort.** A chunk refused with a `4xx` other than `410` is dropped and the run continues. `410` latches the stream as gone: no further chunks are sent, the run continues, and the heartbeat's `410` is what ends it.
4. **Heartbeat beats once immediately, then every `HEARTBEAT_INTERVAL`.** The first beat is what fills the run's image column at the farm. Every beat repeats the full registration (`runner`, `version`, `runtime`, `slots`) - after a farm restart it is the only way a busy runner re-enters the registry.
5. **`repos` and `ready` are sent as `[]`, never omitted and never `null`**; see Types for why the type is still `Option<Vec<_>>`.
6. **Drain**: on `SIGTERM`/`SIGINT` stop claiming, let a running job finish up to `drain_timeout` (default 2 m), then stop it and complete it as `runner_shutdown` before exiting.
7. **`runtime` on an incoming job must be `native`**; anything else completes immediately with `fail_reason: runtime_mismatch`, nothing is spawned.

### Job lifecycle

```
Job received (claim or open_run)
  -> validate: ctx is an existing directory and `git rev-parse --git-dir` succeeds there  (else fail: ctx_invalid)
              plan_path exists and is under ctx                                            (else fail: plan_not_found)
  -> spawn ralphex (below); start the heartbeat task (immediate first beat); start the log flusher
  -> select on process exit and the Terminal channel:
     exit 0 and create_pr        -> existing-PR check / push / gh pr create -> complete { done, pr_url }
     exit 0 and not create_pr    -> complete { done }
     exit != 0                   -> complete { error, nonzero_exit, "ralphex exited with code N", log_tail }
     Terminal::Cancel            -> stop group -> complete { error, canceled }
     Terminal::Drain             -> stop group -> complete { error, runner_shutdown }
     Terminal::Gone (heartbeat)  -> stop group, no complete
     Terminal::VersionMismatch   -> stop group, no complete, agent exits 2
     spawn failure               -> complete { error, spawn_failed, message }
     push failure                -> complete { error, git_push, message }
     pr failure                  -> complete { error, pr_create, message }
```

Fail reasons reuse the farm's names where the meaning matches (`nonzero_exit`, `git_push`, `pr_create`, `canceled`, `runner_shutdown`) and add `runtime_mismatch`, `ctx_invalid`, `plan_not_found`, `spawn_failed`.

### Spawning ralphex

- Program: `config.ralphex_bin` (default `ralphex`, resolved through the daemon's PATH).
- Arguments: `--branch <branch>` always; `--worktree` when `LocalOptions.worktree` is `Yes`; `--review` when `Job.mode == "review"`; then the plan path as given.
- Working directory: `Job.ctx`. `stdin` is `/dev/null`: ralphex can prompt (`create initial commit? [y/N]`), and a prompt nobody sees must not hang a run.
- Environment: the daemon's own plus `LocalOptions.env` (`CLAUDE_CONFIG_DIR` when `rxd` forwarded one). `RALPHEX_CONFIG_DIR` is never set.
- `Command::process_group(0)` so the child leads its own group. stdout and stderr are piped; each pipe is read by its own task **in bounded chunks of at most 64 KiB** straight into the log stream's byte buffer, so an unbroken multi-megabyte line cannot grow memory. A separate line assembler, capped at 64 KiB per line (longer lines are split at the cap), feeds the history ring and subscribers.
- Stop: `nix::sys::signal::killpg(pgid, SIGTERM)`, wait up to `STOP_GRACE`, then `killpg(pgid, SIGKILL)`. Simulator and `xcodebuild` helper daemons that reparent out of the group survive; that is accepted.

### Log pipeline (`logstream.rs`)

- Writers append bytes to the outgoing buffer capped at `LOG_BUFFER_BYTES`; past the cap the oldest bytes are dropped.
- A flusher task wakes every `LOG_FLUSH_INTERVAL` and on close, takes up to `MAX_LOG_CHUNK`, sends it with the next `seq` (starting at 1), and repeats while the buffer is non-empty. A failed send still consumed its `seq`.
- Close: flush what is buffered with `LOG_CLOSE_TIMEOUT` as the total budget, then stop.
- Tail: the last `LOG_TAIL_LINES` lines, capped at `LOG_TAIL_BYTES`, kept separately for `CompleteRequest.log_tail`.
- **History ring**: the last `HISTORY_LINES` lines capped at `HISTORY_BYTES`, fed by the line assembler and **never consumed by the flusher**; this is what a late `rxd attach` replays.
- Subscribers: a `tokio::sync::broadcast` of lines for attached `rxd` clients; `subscribe()` returns the history ring's current contents plus a receiver, in that order, so nothing between replay and live is lost or duplicated.
- `gone()` returns a future that resolves when a `410` latched the stream, for callers that want to know; nothing in this version acts on it (see Key decisions).

### IPC (`ipc.rs`)

Unix socket at `paths::socket_path()`, created with mode `0600`, removed on daemon exit. Framing: 4-byte little-endian length, then JSON; messages over 10 MiB are refused.

```
Command  = Run { ctx, plan, branch, create_pr, worktree, env: Vec<(String, String)> }
         | Attach
Response = Started { run_id, dashboard_url }
         | Line { text }
         | Ended { status, pr_url, fail_reason }
         | Busy { run_id }
         | NoRun
         | Error { message }
```

`Run`: the daemon applies the slot rule from Key decisions (wait through a poll; `Busy` while running); when the slot is taken it calls `open_run` on the farm, answers `Started`, then streams `Line`s until `Ended`. `Attach`: `NoRun` when nothing is running; otherwise the history replay then live `Line`s. A client that disconnects is dropped from the broadcast; the run is unaffected. Several clients may attach at once.

### `rxd`

```
rxd <plan> [--branch <name>] [--no-pr] [--worktree]
rxd attach
rxd install
rxd uninstall
```

- `ctx` is the canonicalised current directory; `plan` is made absolute against it; `repo` is the directory's basename; `branch` defaults to the plan file's stem; `create_pr` defaults to yes.
- While the daemon is inside a claim poll, `rxd` prints `waiting for the daemon (up to 25 s)` and waits; on `Busy` it prints the running run id and exits 1.
- Prints `run <run_id>` and the dashboard URL (`<farm_url>/#/run/<run_id>`) before the first output line, so the operator can leave immediately.
- Ctrl-C closes the socket and exits 0; the run continues. When `rxd <plan>` stays attached to the end its exit status is 0 for `done` and 1 for `error`.
- Forwards `CLAUDE_CONFIG_DIR` from its own environment when set, so a run started from the operator's work shell (`rxw`) uses the work Claude profile.

### Pull request (`pr.rs`)

Runs in `ctx`, only after exit 0 and only when `create_pr` is yes:

1. `gh pr list --head <branch> --state open --json url --jq '.[0].url'`; a non-empty answer is an existing pull request: `git push origin <branch>` to update it and report that URL, skipping the steps below. Re-running the same plan reuses the same default branch name, so this is the ordinary second run, not an edge case.
2. `git push -u origin <branch>`.
3. Base branch: `git symbolic-ref --short refs/remotes/origin/HEAD` with the `origin/` prefix stripped; if that fails, `gh repo view --json defaultBranchRef --jq .defaultBranchRef.name`.
4. `gh pr create --head <branch> --base <base> --title <title> --body <body>`; the URL is the last line of stdout.

Title: `<identifier>: <title>` for a ticket job, the plan stem for a local run. Body: `Plan: <plan_path>`, `Run: <run_id>`, `Resolves <identifier>` (with the issue URL when present), `Automated by ralphex-macos-runner.` - each on its own line, blank lines between.

### Configuration and paths

`paths.rs` chooses by profile: the release profile uses `dirs::data_dir()` (`~/Library/Application Support`) and `~/Library/Logs`; the debug profile uses the same roots with a `-dev` suffix on the application directory, so a daemon under development never collides with the installed one.

```
<data>/ralphex-macos-runner/config.toml
<data>/ralphex-macos-runner/daemon.sock
<data>/ralphex-macos-runner/bin/ralphex-macos-runner     (stable copy launchd runs)
~/Library/Logs/ralphex-macos-runner/daemon.{out,err}.log
~/Library/LaunchAgents/dev.pkarpovich.ralphex-macos-runner.plist
```

`config.toml`:

```toml
farm_url      = "http://farm.example:7077"
token         = "..."
name          = "mbp-native"
drain_timeout = "2m"       # default
ralphex_bin   = "ralphex"  # default
```

The daemon refuses to start when `farm_url`, `token` or `name` is missing, and logs a warning when the file is readable by group or others. There is no `slots` key.

### launchd (`service.rs`)

`rxd install`: copies the daemon binary that sits next to the running `rxd` (same directory as `std::env::current_exe()`) to the stable path above, writes the plist with `Label`, `ProgramArguments` (the stable path), `RunAtLoad true`, `KeepAlive true`, `StandardOutPath`/`StandardErrorPath` under the log directory, and `EnvironmentVariables.PATH` set to the installing shell's `$PATH`; runs `launchctl bootout gui/<uid> <plist>` ignoring failure, then `launchctl bootstrap gui/<uid> <plist>` and fails loudly if that fails. `rxd uninstall` boots out and removes the plist. The daemon binary in the Homebrew prefix is only the source of the copy - the stable path never changes across `brew upgrade`, so a re-run of `rxd install` after an upgrade is the whole update procedure, and the formula's caveat says so.

### Release

`.github/workflows/release.yml`, on tag `v*`, one job on `macos-latest`:

1. checkout; `jdx/mise-action`; `Swatinem/rust-cache`.
2. the tag (`v` stripped) must equal `Cargo.toml`'s `package.version`, read with `cargo metadata --no-deps`; otherwise fail.
3. `mise run check`.
4. `cargo build --release --target aarch64-apple-darwin`.
5. certificate import: decode `MACOS_CERT_P12_BASE64` into `$RUNNER_TEMP/cert.p12`; `security create-keychain -p <random> $RUNNER_TEMP/build.keychain-db`; `security set-keychain-settings -lut 21600`; `security unlock-keychain`; `security import cert.p12 -k <keychain> -P $MACOS_CERT_PASSWORD -T /usr/bin/codesign`; `security list-keychains -d user -s <keychain> <existing>`; `security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k <password> <keychain>`; remove `cert.p12`.
6. `codesign --force --options runtime --timestamp --identifier dev.pkarpovich.ralphex-macos-runner --sign "$SIGN_IDENTITY"` on the daemon and the same with identifier `dev.pkarpovich.rxd` on `rxd`, where `SIGN_IDENTITY` is a workflow variable holding the operator's `Developer ID Application: ...` identity; `codesign --verify --strict --verbose=2` on both.
7. `tar.gz` of the two binaries named `ralphex-macos-runner-aarch64-apple-darwin.tar.gz`; GitHub release with `generate_release_notes`.
8. a second job: compute the tarball's sha256, check out `pkarpovich/homebrew-apps` with `HOMEBREW_TAP_TOKEN`, write `Formula/ralphex-macos-runner.rb` from `docs/formula-template.rb` with the version and sha256 substituted, commit and push.

`docs/formula-template.rb`: `class RalphexMacosRunner < Formula`, `on_macos { on_arm { url, sha256 } }`, `bin.install` both binaries, a `caveats` block naming `rxd install` as the post-upgrade step, a `test` block asserting `rxd --help` mentions `rxd`.

Secrets: `MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD`, `HOMEBREW_TAP_TOKEN`; variable: `SIGN_IDENTITY`.

`.github/workflows/ci.yml`, on push and pull request: `mise run check`.

## Implementation Steps

### Task 1: Crate scaffold, toolchain pin, paths and CI

**Files:**
- Create: `Cargo.toml`, `src/lib.rs`, `src/paths.rs`, `src/bin/ralphex-macos-runner.rs`, `src/bin/rxd.rs`
- Create: `mise.toml`, `.github/workflows/ci.yml`
- Modify: `.gitignore` (keep the existing `.revmux/` and `target/` lines)
- Create: `tests/smoke.rs`

- [x] `cargo init --lib` in the repository root; declare the two `[[bin]]` targets; add the dependencies listed in Context with `cargo add` (runtime and dev separately)
- [x] `mise.toml`: `[tools]` pinning `rust` to the current stable (`mise use rust@latest`) and `actionlint`; a `check` task running `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `actionlint`
- [x] `src/paths.rs` with the profile-dependent paths from Technical Details; `lib.rs` declares it
- [x] `clap` skeletons: the daemon takes `--config <path>` defaulting to `paths::config_path()`; `rxd` has the four subcommands as no-ops that print what they would do
- [x] `ci.yml`: checkout, `jdx/mise-action`, `Swatinem/rust-cache`, `mise run check`
- [x] write unit tests for `paths`: release and debug profiles give different application directories, every path is under the expected root
- [x] write `tests/smoke.rs`: both binaries answer `--help` with exit 0 and mention their own name
- [x] run the gate - must pass before task 2

➕ `rust` is pinned to `1.98.0` with the `rustfmt` and `clippy` components, `actionlint` to `1.7.12`.
⚠️ `reqwest` 0.13 renamed its TLS feature: the crate uses `features = ["json", "rustls"]` with default features off, not the `rustls-tls` named in Context. The Context line below is corrected to match.

### Task 2: Protocol types and conformance vectors

**Files:**
- Create: `src/protocol/mod.rs`, `src/protocol/types.rs`
- Modify: `src/lib.rs`
- Create: `tests/protocol_vectors.rs`

- [x] define every type and constant from Technical Details with `serde` derives; `repos`/`ready` as `Option<Vec<_>>` with `#[serde(default)]`; newtypes `RunId`, `Seq`, `RunnerName`, `Branch` with transparent serde
- [x] `ClaimRequest::native(name)` and `HeartbeatRequest::native(name)` constructors producing `Some(vec![])` for the slices, `image: ""`, `slots: SLOTS`, `runtime: RUNTIME`, `version: VERSION`
- [x] transcribe every vector from "Conformance vectors" into `tests/protocol_vectors.rs`; each test deserialises the vector, re-serialises, and asserts equality of the two `serde_json::Value`s
- [x] write unit tests: the native constructors serialise `repos` and `ready` as `[]`; a `null` in either deserialises to `None`; a missing key deserialises to `None`
- [x] run the gate - must pass before task 3

➕ `create_pr` is the `CreatePr::{Yes, No}` enum from the Code-Quality Rules, encoded as a JSON boolean through `#[serde(from = "bool", into = "bool")]`; `action` and `status` are the enums `HeartbeatAction` and `CompleteStatus` with lowercase serde renaming. Every other wire field stays a `String`, an integer or one of the four newtypes.

### Task 3: Farm client with per-call retry and status mapping

**Files:**
- Create: `src/protocol/client.rs`
- Modify: `src/protocol/mod.rs`
- Create: `tests/support/mod.rs`, `tests/support/fake_farm.rs`
- Create: `tests/client.rs`

- [x] `FarmClient::new(farm_url, token, sleeper)`; methods `claim`, `open_run`, `append_log(run_id, seq, bytes)`, `heartbeat`, `complete`, each returning `Result<_, FarmError>` where `FarmError` is `{ Gone, VersionMismatch { message: String }, BadRequest(String), Rejected(u16, String), Transport(String), Decode(String) }`
- [x] the per-operation retry table from Behavioural rule 1, with the sleeper injected (`Sleeper` trait, tokio-backed in production, instant-and-recording in tests) and a clock for `COMPLETE_BUDGET`
- [x] `claim` returns `Ok(None)` on `204`; `409` decodes the `error` field into `VersionMismatch` (raw body when the field is absent); `410` to `Gone`; other `4xx` to `Rejected` or `BadRequest` for `400`; `5xx` and transport errors follow the table
- [x] `tests/support/fake_farm.rs`: an `axum` server on an ephemeral port with a scripted response queue per route, a hold-until-released claim, the farm's `seq` rule on the log route, and a recorder of `(path, headers, body)`; a helper to start it and get its URL
- [x] write tests: bearer header on every call; `204` claim; job decode; `409` on claim and on heartbeat carries the fake's message verbatim; `410`; `append_log` on `500,500,200` succeeds on the third attempt with two recorded sleeps; `append_log` gives up after 6 attempts of `500`; `complete` is still retrying at attempt 10 and stops when the clock passes `COMPLETE_BUDGET`; `complete` stops on `410`; `open_run` on `500` fails at once with exactly one recorded request; a non-`410` `4xx` on log does not retry
- [x] run the gate - must pass before task 4

➕ The `Sleeper` trait carries both `sleep` and `now`, so one injected object serves the backoff and the `COMPLETE_BUDGET` clock; the test double advances its own clock by every delay it is asked for.
➕ The fake farm's per-route script is a queue plus an optional sticky reply the route falls back to once the queue is empty, which is how a test asks for "answer `500` until I stop asking".

### Task 4: Log pipeline

**Files:**
- Create: `src/logstream.rs`
- Modify: `src/lib.rs`
- Create: `tests/logstream.rs`

- [x] `LogStream::new(client, run_id, ticker)` with `write(&[u8])`, `push_line(String)`, `subscribe() -> (Vec<String>, broadcast::Receiver<String>)`, `tail() -> String`, `gone()`, `close().await`
- [x] flusher task per Technical Details: `seq` starting at 1 and incremented per attempt, `MAX_LOG_CHUNK` pieces, drop on `4xx`, retry per the client, latch `gone` on `410`
- [x] the outgoing ring with oldest-drop, the history ring bounded by `HISTORY_LINES` and `HISTORY_BYTES`, the tail bounded by `LOG_TAIL_LINES` and `LOG_TAIL_BYTES`
- [x] write tests against the fake farm: 200 KiB written arrives as four chunks with `seq` 1..4; a chunk the fake answers `400` is dropped and the next carries `seq` 3 not 2; the fake rejects a chunk sent with `seq` 0; `410` stops further posts and `gone()` resolves; tail keeps the last 100 lines; 50 lines pushed, **three flush ticks driven**, then `subscribe()` returns all 50 as replay followed by lines pushed afterwards; the history ring drops the oldest line past `HISTORY_LINES`; `close` flushes the remainder
- [x] run the gate - must pass before task 5

➕ The flush cadence is injected through a `Ticker` trait (`IntervalTicker` in production, a `ManualTicker` in `tests/support` whose `TickHandle::drive()` releases one tick and returns only once the flush it triggered is finished), so the tests drive flushes without waiting on a clock.
⚠️ The `gone` and `close` latches use `watch::Sender::send_replace`, not `send`: `send` fails and leaves the value unchanged when no receiver is alive, which silently lost the `410` latch.

### Task 5: Spawning and stopping ralphex

**Files:**
- Create: `src/job.rs`
- Modify: `src/lib.rs`
- Create: `tests/support/fake-ralphex.sh`
- Modify: `tests/support/mod.rs`
- Create: `tests/job.rs`

- [x] `JobSpec { ctx, plan, branch, review: Review, local: LocalOptions, ralphex_bin }` with `LocalOptions { worktree: Worktree, env: Vec<(String, String)> }` and `validate(&JobSpec) -> Result<(), JobError>` implementing the ctx and plan checks from the lifecycle table
- [x] `spawn(spec, log: &LogStream) -> Result<RunningJob, JobError>` building the argv and environment from Technical Details, `stdin` null, `process_group(0)`, one task per pipe reading 64 KiB chunks into `write` and a capped line assembler into `push_line`
- [x] `RunningJob::wait() -> ExitStatus` and `RunningJob::stop(grace).await` doing `SIGTERM` group, the grace, `SIGKILL` group
- [x] `fake-ralphex.sh`: prints `FAKE_RALPHEX_LINES` lines alternating stdout/stderr, prints one unbroken line of `FAKE_RALPHEX_LONG_LINE` bytes when set, records its argv, cwd and full environment to `FAKE_RALPHEX_RECORD`, spawns a sleeping child when `FAKE_RALPHEX_CHILD` is set, sleeps `FAKE_RALPHEX_SLEEP` seconds, exits `FAKE_RALPHEX_EXIT`
- [x] write tests: argv contains `--branch x` and the plan path and, when asked, `--worktree` and `--review`; cwd is `ctx`; an entry in `LocalOptions.env` reaches the recorded environment; stdout and stderr both reach the log stream; exit code propagates; a 1 MiB unbroken line reaches the farm buffer in chunks and the subscribers as lines no longer than 64 KiB; a sleeping fake with a child is stopped with both processes gone within the grace; a missing ctx and a plan outside ctx fail validation with the named error
- [x] run the gate - must pass before task 6

➕ `spawn` takes `Arc<LogStream>`, not `&LogStream`: the two pipe readers are `tokio::spawn`ed tasks and need an owned `'static` handle on the stream.
➕ `validate` is `async` because the ctx check runs `git rev-parse --git-dir` through `tokio::process`.
➕ `wait` and `stop` return `Result<ExitStatus, JobError>` with a `JobError::Wait` variant rather than a bare `ExitStatus`, so an unwaitable child is reported instead of panicking; `Wait` reuses the `spawn_failed` fail reason. Both drain the pipe readers before returning, so no output is lost between the exit and the completion.
➕ The line assembler emits a line whenever the pending bytes reach `MAX_LOG_CHUNK`, so an unbroken 1 MiB line becomes 16 capped lines plus the empty remainder its newline closes.
⚠️ The fake records `pwd -P`, not `pwd`: a child inherits the daemon's `PWD`, so only the physical path can be compared against a canonicalised `ctx`. Its background child redirects both pipes to `/dev/null`, otherwise a reparented grandchild would hold the pipe open and the reader drain would never see EOF.

### Task 6: Pull request

**Files:**
- Create: `src/pr.rs`
- Modify: `src/lib.rs`
- Create: `tests/support/bin/git`, `tests/support/bin/gh`
- Modify: `tests/support/mod.rs`
- Create: `tests/pr.rs`

- [x] `open_pull_request(ctx, PrSpec { branch, title, body }) -> Result<PrUrl, PrError>` running the four-step sequence from Technical Details with `tokio::process::Command`, `PrError::{Push(String), Base(String), Create(String), List(String)}`
- [x] title and body builders for a ticket job and for a local run, exactly as specified
- [x] the two shims: record `$0 $@` and cwd to the file named by `FAKE_RECORD`; `gh pr list` prints nothing unless `FAKE_EXISTING_PR` is set, in which case it prints that value; `git symbolic-ref` prints `origin/main`; `gh pr create` prints a canned URL; `gh repo view` prints `main`; each fails when `FAKE_FAIL` names its command
- [x] write tests: order list -> push -> symbolic-ref -> pr create with the expected flags when no pull request exists; with `FAKE_EXISTING_PR` set the result is that URL, `git push` ran without `-u`, and `gh pr create` did not run; base fallback to `gh repo view` when symbolic-ref fails; each failure maps to its variant; the URL is parsed from the last stdout line; title and body for both run kinds
- [x] run the gate - must pass before task 7

➕ `open_pull_request` takes a third argument, `PrTools { git, gh, env }`, whose `Default` is the `git` and `gh` on the daemon's `PATH` and no extra environment. The shims are addressed by path and configured through `env`, so no test mutates the test process's own environment (`std::env::set_var` is `unsafe` in edition 2024 and the crate carries no `unsafe`).
➕ `PrError::fail_reason()` maps `Push` to `git_push` and `List`, `Base` and `Create` to `pr_create`, the two names the lifecycle table gives task 7.
➕ `PrSpec::describe(branch, &RunOrigin, plan, run_id)` is the one builder for both run kinds; `RunOrigin::{Ticket { identifier, issue_url, title }, Local}` decides the title and whether a `Resolves` paragraph appears. A ticket without an issue URL resolves the bare identifier.
⚠️ `gh pr list --jq '.[0].url'` prints `null` for a branch with no open pull request, so `null` counts as absent alongside an empty answer; treating it as a URL would report `null` as the pull request of every first run.
⚠️ The shims append one block per invocation (`cmd:`, `cwd:`, one `arg:` per argument, newlines inside an argument turned into spaces) rather than a single `$0 $@` line, because the pull request body is multi-line; `support::invocations` parses the blocks back.

### Task 7: Agent - the slot, claim, heartbeat, drain, complete

**Files:**
- Create: `src/config.rs`, `src/agent.rs`
- Modify: `src/lib.rs`, `src/paths.rs`, `src/bin/ralphex-macos-runner.rs`
- Create: `tests/agent_e2e.rs`

- [x] `config.rs`: the schema from Technical Details, defaults, the three required fields, the permission warning
- [x] `Agent::new(config, client, AgentOptions { heartbeat_interval, drain_timeout, stop_grace, ticker })` so every interval is injectable; `run(shutdown: watch::Receiver<bool>) -> AgentExit`
- [x] the `RunSlot` state machine from Key decisions and `run_job(job, local)` selecting on process exit and the `Terminal` channel, with every row of the lifecycle table; the heartbeat task beating once immediately then every interval, translating `cancel`, `410` and `409` into `Terminal` events; `runtime_mismatch` refusal before spawn; drain on shutdown
- [x] the daemon `main`: load config, build the client, spawn the signal task flipping the `watch`, run the agent, exit 2 on `AgentExit::VersionMismatch`
- [x] write `tests/agent_e2e.rs` against the fake farm and fake ralphex, with millisecond intervals: a claimed job runs to `done` with the fake's output on the farm and a `complete` recorded; the very first heartbeat arrives before the first flush; exit 3 completes as `nonzero_exit` with a tail; `cancel` on heartbeat stops the process and completes `canceled`; a job with `runtime: container` completes `runtime_mismatch` without spawning; `409` on claim ends the agent with `VersionMismatch`; `409` on heartbeat with a running fake stops its process group and ends the agent with `VersionMismatch`; `410` on heartbeat kills the job and posts no `complete`; `410` on a log chunk leaves the job running and it still completes `done`; a second job is not claimed while one runs; shutdown during a run completes `runner_shutdown` after the drain timeout
- [x] run the gate - must pass before task 8

➕ `AgentOptions` carries two injectables the plan did not name: `claim_retry_delay`, so no test waits out the second the claim loop sleeps after a failed poll, and `pr_tools`, so the two pull-request rows of the lifecycle table run against the shims from task 6. The remaining rows (spawn failure, `ctx_invalid`, `plan_not_found`) have tests of their own, so `tests/agent_e2e.rs` covers the whole table and task 11's second checkbox is already satisfied.
➕ The process exit stays a `select!` branch of its own instead of travelling on the `Terminal` channel: waiting needs `&mut RunningJob`, which the stop path needs back the instant a terminal event wins the select. Cancel, drain, `410` and `409` do share the one channel, so every terminal condition still has exactly one handling site.
➕ `RunSlot` moves `Free -> Polling -> Running(run_id) -> Free` along the claim path and is readable through `Agent::slot()`; the local request's wait-through-a-poll belongs to task 8's `start_local`.
➕ `config.toml` is parsed with `deny_unknown_fields`, so the absent `slots` key is refused loudly rather than ignored silently.
➕ `tests/support` grew three doubles: `always_claim` on the fake farm (a sticky `Hold` parks the claim loop once the scripted jobs are gone), `fake_ralphex_with` (a wrapper script that sets the fake's environment, because a claimed job is spawned with no environment of its own) and `fixed_ticker` (a millisecond flush cadence).
⚠️ A `done` completion carries no `log_tail`: the conformance vector pins it empty and only a failure carries the tail. `src/paths.rs` needed no change after all.

### Task 8: Socket IPC and the `rxd` run and attach commands

**Files:**
- Create: `src/ipc.rs`
- Modify: `src/lib.rs`, `src/agent.rs`, `src/bin/ralphex-macos-runner.rs`, `src/bin/rxd.rs`
- Create: `tests/rxd_e2e.rs`

- [x] `ipc.rs`: `Command` and `Response` from Technical Details, `send`/`receive` with the length-prefixed framing and the 10 MiB cap
- [x] daemon listener: bind the socket at `paths::socket_path()` with mode `0600`, remove a stale file first, remove on exit; per connection, `Run` -> `agent.start_local(request)` (which applies the slot rule, waiting through a poll) -> `client.open_run` -> `Started` -> subscribe -> stream `Line`s -> `Ended`; `Attach` -> `NoRun` or replay plus live
- [x] `rxd <plan>` and `rxd attach` per Technical Details, including the `CLAUDE_CONFIG_DIR` forward, the waiting message, the `run <id>` and URL header lines, Ctrl-C handling and the exit status
- [x] write `tests/rxd_e2e.rs`: framing round-trip including the cap; `rxd <plan>` against a running daemon prints the run id and URL first, streams the fake's lines, exits 0 on `done` and 1 on `error`; `rxd --worktree` puts `--worktree` in the fake's recorded argv; a `CLAUDE_CONFIG_DIR` in `rxd`'s environment reaches the fake's recorded environment; `rxd` during a held claim poll starts once the fake releases the poll with `204`; `rxd` during a held poll that the fake releases with a job gets `Busy` and the job runs; `Busy` while a job runs; `attach` replays history then streams; two attached clients both receive lines; the socket file has mode `0600`
- [x] run the gate - must pass before task 9

➕ The slot is a `tokio::sync::Semaphore` with one permit next to the `RunSlot` state, because the handoff the Key decisions ask for needs a fair queue: on a `204` the claim loop drops the permit and asks for it again, and a waiting `rxd` is served first because tokio's semaphore serves its waiters in order. The state alone could not do it - a compare-and-set on a `watch` would let the claim loop re-take the slot before the waiter woke.
➕ `RunSlot` grew an `Opening` variant for the moment between taking the slot and the farm minting the run id. A second local request treats it as `Polling` and waits, so a `Busy` answer always carries a real run id.
➕ `Agent::run_job`'s body became `execute`, which returns the `CompleteRequest` instead of posting it, plus `CurrentRun` - the run id, the dashboard URL, the log stream and a `watch` of `RunState` - which both entry points register in `Agent::current`. That is what `Attach` follows, so `rxd attach` works for a ticket run as well as a local one, and the log stream is now created by the agent rather than inside the job path.
➕ `follow` answers an `Attach` with `Started` before the replay, so an attaching operator sees which run they joined.
➕ Both binaries take `--socket <path>`, which is how the suite runs a real `rxd` against an in-process daemon on a temporary socket instead of the installed one.
➕ A local run that ends in `VersionMismatch` logs it and lets the claim loop meet the same `409` on its next poll; only the claim loop exits the process with status 2.
⚠️ `rxd --socket <path> attach` parses `attach` as the plan, because clap gives an optional positional precedence over a subcommand; `rxd attach --socket <path>` is the order that works, and `--socket` is global for that reason. Production usage (`rxd attach`) is unaffected.
⚠️ The listener's readiness cannot be waited on with `Path::exists`: a stale regular file at the socket path already exists, so the suite waits for a path whose file type is a socket.

### Task 9: launchd install and uninstall

**Files:**
- Create: `src/service.rs`
- Modify: `src/lib.rs`, `src/bin/rxd.rs`

- [x] `generate_plist(daemon_path, path_env, log_dir) -> String` producing the fields from Technical Details
- [x] `install()`: copy the daemon binary from `current_exe()`'s directory to the stable path, write the plist, `bootout` (ignored), `bootstrap` (fatal); `uninstall()`: `bootout`, remove the plist; both print the paths they touched and the `launchctl` commands to stop and start by hand
- [x] wire `rxd install` and `rxd uninstall`
- [x] write unit tests for `generate_plist`: label, program path, `RunAtLoad`, `KeepAlive`, both log paths, and `PATH` equal to the value passed in; `install` itself needs launchd and is exercised only by the Post-Completion smoke
- [x] run the gate - must pass before task 10

➕ `nix` gained the `user` feature, for `Uid::current()` in the `gui/<uid>` domain target; the alternative was reading the uid off the home directory's metadata, which is indirect for no gain.
➕ `install` and `uninstall` return `Installed` and `Uninstalled` rather than printing themselves; each one's `Display` is the block `rxd` prints (the paths touched, then the `launchctl bootout` and `bootstrap` lines from `by_hand`), so the printed text is unit-tested instead of being a side effect of the library.
➕ Paths and the `PATH` value are XML-escaped into the plist, and `generate_plist` builds the two log paths from the log directory with the `STDOUT_FILE` and `STDERR_FILE` constants; a test pins them equal to `paths::daemon_stdout_path()` and `paths::daemon_stderr_path()`.
⚠️ `current_exe()` is canonicalized before its directory is taken: `/opt/homebrew/bin/rxd` is a symlink into the Cellar keg, and only the resolved directory holds the daemon binary to copy. The existing binary at the stable path is removed before the copy, because writing over a running executable fails with `ETXTBSY`.

### Task 10: Release workflow and formula

**Files:**
- Create: `.github/workflows/release.yml`
- Create: `docs/formula-template.rb`
- Modify: `mise.toml`

- [x] `release.yml` with the eight steps from Technical Details, the certificate-import sequence written out, and the formula-rewrite job
- [x] `docs/formula-template.rb` as specified, kept in the repository so a change to it is reviewed here
- [x] extend `mise run check` with `ruby -c docs/formula-template.rb` and a structural check on `release.yml`: it must contain `security import`, `set-key-partition-list`, two `codesign --sign` invocations and two `codesign --verify` invocations, checked with `grep -c`
- [x] run the gate (`actionlint` now covers both workflows) - must pass before task 11

➕ The release job publishes `version` and `sha256` as job outputs, so the formula job substitutes them into the template with `sed` instead of downloading the tarball again to hash it. The template's placeholders are `@VERSION@` and `@SHA256@`, both inside string literals so `ruby -c` parses the template as it stands in the repository.
➕ The release itself is `softprops/action-gh-release@v2`, which is what carries the `generate_release_notes` input the plan names. The formula job pushes nothing when the rendered formula is byte-identical to the one already in the tap.
➕ The structural check is its own `check-release` task that `check` calls, because the shell it needs does not fit an entry of `check`'s run array.
⚠️ BSD `grep` treats `$` as an end-of-line anchor in the middle of a basic regular expression too, so `grep -c -- '--sign "$SIGN_IDENTITY"'` counted zero matches against lines that plainly contain it. The check uses `grep -cF` for every pattern, and a `|| true` so a zero count reaches the comparison instead of tripping `set -e`.
⚠️ `security list-keychains -d user -s` replaces the whole search list, so the existing entries are read into a bash array and passed back alongside the new keychain; splitting the command substitution unquoted would have been the shorter way and is what shellcheck rejects.

### Task 11: Verify acceptance criteria

**Files:**
- Modify: `docs/plans/20260902-ralphex-macos-runner.md`

- [x] `mise run check` is green
- [x] every row of the lifecycle table has a passing test in `tests/agent_e2e.rs`
- [x] `mise exec -- cargo test --test protocol_vectors` passes and the file contains every vector from Technical Details
- [x] the crate has no `//` line comments and no `unsafe`
- [x] every non-goal still holds: no git clone/fetch/reset/clean anywhere in `src/`, no progress endpoint call, no `RALPHEX_CONFIG_DIR`, no `slots` in config
- [x] **stop here.** Everything after this line needs the deployed farm and launchd and is the operator's to run; record any blocker with ⚠️ and continue to task 12

➕ Lifecycle rows to their tests in `tests/agent_e2e.rs`: exit 0 with a pull request - `a_finished_run_opens_a_pull_request`; exit 0 without one - `a_claimed_job_runs_to_done_and_its_output_reaches_the_farm`; nonzero exit - `a_nonzero_exit_completes_as_a_failure_with_its_tail`; `Cancel` - `a_cancel_on_the_heartbeat_stops_the_run_and_completes_it_as_canceled`; `Drain` - `a_run_that_outlasts_its_drain_completes_as_a_shutdown`; `Gone` - `a_forgotten_run_is_killed_and_never_completed`; `VersionMismatch` - `a_version_mismatch_on_the_claim_ends_the_agent` and `a_version_mismatch_on_the_heartbeat_stops_the_run_and_ends_the_agent`; spawn failure - `a_ralphex_that_cannot_be_started_completes_as_a_spawn_failure`; push failure - `a_push_that_fails_completes_as_a_push_failure`; pull request failure - `a_pull_request_that_fails_completes_as_a_creation_failure`; `ctx_invalid` - `a_checkout_that_is_not_a_repository_completes_as_an_invalid_context`; `plan_not_found` - `a_plan_outside_the_checkout_completes_as_a_missing_plan`; `runtime_mismatch` - `a_container_job_is_refused_without_spawning_anything`.
➕ All 13 conformance vectors are transcribed in `tests/protocol_vectors.rs` and its 13 tests pass.
➕ The non-goal greps find only test literals: `"reset"` appears twice as a `FarmError::Transport` message, `RALPHEX_CONFIG_DIR` only in the `tests/job.rs` assertion that it is unset, and `slots` only in the `config.rs` test that `deny_unknown_fields` refuses it. `src/pr.rs` runs `push`, `symbolic-ref`, `gh pr list/create` and `gh repo view` and nothing else; `src/job.rs` runs `git rev-parse --git-dir`.

### Task 12: Update documentation

**Files:**
- Create: `README.md`, `CLAUDE.md`
- Modify: `docs/plans/20260902-ralphex-macos-runner.md`

- [ ] `README.md`: what it is, `brew install pkarpovich/apps/ralphex-macos-runner`, `rxd install`, the config file, `rxd` usage including the waiting message, the ticket block for native runs, the update procedure (`brew upgrade` then `rxd install`), the exit-2 meaning
- [ ] `CLAUDE.md`: build and test commands, the module map from Solution Overview, the key patterns (one execution path with `LocalOptions`, the run slot, fatal `409`, the `410` rule, the cancel sequence), in the same shape as the operator's other daemon repositories
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes, informational only*

**Before the live smoke**

- Plan 1 (`ralphex-farm`) merged and deployed to the farm host.
- Cross-check the conformance vectors: diff the strings under "Conformance vectors" against the farm's `pkg/protocol/protocol_test.go` at the commit that merged plan 1. A difference is a bug in one of the two plans; fix the vector here or the golden there, then re-run `cargo test --test protocol_vectors`.
- The GitHub repository `pkarpovich/ralphex-macos-runner` created and this checkout pushed; secrets `MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD`, `HOMEBREW_TAP_TOKEN` and the variable `SIGN_IDENTITY` added.

**Live smoke, step by step, in the `ralphex-farm` checkout on this Mac**

1. `cargo build --release`; `./target/release/rxd install` (the daemon binary is picked up from the same directory).
2. Write `~/Library/Application Support/ralphex-macos-runner/config.toml` with the real `farm_url`, `token` and `name = "mbp-native"`; confirm the daemon log shows a successful first claim (`204`).
3. Confirm the farm's `/health` lists a runner named `mbp-native` with `slots: 1`.
4. Write a throwaway one-task plan under `docs/plans/` (a task that appends a line to a scratch file), then `./target/release/rxd docs/plans/<that plan>.md --no-pr`; the same output must appear in the terminal and on the dashboard.
5. Ctrl-C; `./target/release/rxd attach`; the replay must show the lines already printed, then continue live; let it finish and confirm `done` on the dashboard.
6. Run it again without `--no-pr`; confirm the pull request URL on the dashboard; run it a third time and confirm the same pull request URL is reported rather than `pr_create`.
7. Create a Linear ticket with `runtime: native` and `ctx` pointing at that checkout; confirm this daemon claims it and the container runner does not.
8. Remove the throwaway plan and branch.

**First release**

- Tag `v0.1.0` after task 12; confirm the release, then `brew install pkarpovich/apps/ralphex-macos-runner` and `rxd install` replace the locally built binaries from the smoke.

**Real validation, after this plan**

- The native runner's purpose is Xcode work. The first real acceptance is a plan from one of the Xcode projects on this Mac (`allspeak`, `glitch` or `tuclaw-client` have plans in `docs/plans`) run through `rxd`, with a simulator test in it. That run decides whether the process-group stop is good enough for `xcodebuild` and whether any environment variable beyond `PATH` and `CLAUDE_CONFIG_DIR` has to reach the daemon.

**Deferred parity**

- `POST .../progress` from the plan file, with `tasks` as `Option<Vec<_>>`, so native runs show task progress on the dashboard.
- Generated pull request descriptions, if the fixed body proves too thin.
- More than one slot, once runs are serialised per checkout.
