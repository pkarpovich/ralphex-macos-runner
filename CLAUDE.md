# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Development Commands

```bash
mise run check                        # the gate: fmt, clippy -D warnings, tests, actionlint, ruby -c on the formula, release-workflow structure
mise exec -- cargo test               # all tests
mise exec -- cargo test --test agent_e2e            # one integration suite
mise exec -- cargo test --test agent_e2e a_cancel   # one test by name
mise exec -- cargo build --release
mise exec -- cargo run --bin ralphex-macos-runner -- --config /tmp/config.toml --socket /tmp/rxd.sock
mise exec -- cargo run --bin rxd -- --socket /tmp/rxd.sock docs/plans/x.md
```

`mise run check` is the gate for every change; nothing is done until it is green. Rust is pinned in `mise.toml`.

## Architecture Overview

A native runner for ralphex-farm: it claims jobs from the farm over HTTP and runs `ralphex` in an existing checkout on this Mac, instead of in a Docker task container. It is a second, independent implementation of the same runner protocol the Go runner in `ralphex-farm` speaks - `pkg/runner/` there is the reference behaviour, not a library this crate shares.

### Two Binaries

- **ralphex-macos-runner** (`src/bin/ralphex-macos-runner.rs`): the daemon. Loads the config, builds the farm client, spawns the signal task, serves the Unix socket and runs the agent. Exits 2 on a protocol version mismatch.
- **rxd** (`src/bin/rxd.rs`): the client. `rxd <plan>`, `rxd attach`, `rxd install`, `rxd uninstall`. Both binaries take `--socket <path>`, which is how the suite drives a real `rxd` against an in-process daemon.

### Module Structure

- `protocol/types.rs`: the wire types and every constant (`VERSION`, `RUNTIME`, `SLOTS`, timings, chunk and ring sizes). JSON keys are the farm's; `repos`/`ready` are `Option<Vec<_>>` with `#[serde(default)]` but always **sent** as `[]`.
- `protocol/client.rs`: `FarmClient` - claim, open_run, append_log, heartbeat, complete. Retry is **per operation** (see Key Patterns) and injected through the `Sleeper` trait, which carries both `sleep` and `now`.
- `logstream.rs`: the byte buffer the farm gets in `seq`-numbered chunks, the tail for `CompleteRequest.log_tail`, the history ring a late `rxd attach` replays, and the `broadcast` of lines live attachers follow. Flush cadence comes from the `Ticker` trait.
- `job.rs`: `JobSpec`/`LocalOptions`, `validate` (ctx is a git checkout, plan sits inside it), `spawn` (argv, `stdin` null, `process_group(0)`, one reader task per pipe), `RunningJob::wait`/`stop`.
- `pr.rs`: the existing-pull-request check, `git push`, base-branch resolution, `gh pr create`; `PrSpec::describe` builds the title and body for both run kinds. `PrTools` names the `git` and `gh` to run, so tests point at shims.
- `agent.rs`: the config-driven agent - the run slot, the claim loop, the heartbeat task, `execute` (the one execution path), drain and completion. `CurrentRun` is what `Attach` follows.
- `ipc.rs`: length-prefixed JSON over the Unix socket, the `Command`/`Response` enums and `serve`.
- `config.rs`, `paths.rs`, `service.rs`: `config.toml`, profile-dependent locations, the launchd plist and install/uninstall.
- `tests/support/`: the `axum` fake farm with a scripted queue per route, `fake-ralphex.sh`, and `git`/`gh` shims that record their arguments and answer what the real tools do (`gh pr list` prints `null` when there is none). Nothing in the suite touches the real farm, Linear, launchd or the network.
- `tests/daemon_process.rs`: the daemon as launchd runs it - a real process driven by `--config`/`--socket` and signals, for the exit statuses and the shutdown drain, which an in-process `Agent` cannot show.

### Key Patterns

- **One execution path, one extra argument**: `Agent::execute` serves both the claim path and `rxd`. `LocalOptions { worktree, env }` carries what only `rxd` can ask for; a claimed job passes the default. Do not fork the two paths.
- **One slot, no reservation during a poll**: `RunSlot` is `Free -> Polling -> Opening -> Running(run_id)`, guarded by a one-permit `tokio::sync::Semaphore` because the handoff needs a fair queue. A local request that arrives during a poll **waits for the poll to return** (at most 25 s) rather than aborting it: an aborted poll could lose a job the farm already dispatched, which would sit on a lease nobody heartbeats until it is finalised `runner_lost`. While a job runs, a local request is answered `Busy`.
- **Terminal events travel on one channel**: cancel, drain, `410` and `409` reach the run through a single `Terminal` enum on an `mpsc`, so every terminal condition has exactly one handling site. The process exit stays its own `select!` branch, because waiting needs `&mut RunningJob` that the stop path needs back immediately.
- **Protocol drift is fatal**: `409` on claim or heartbeat stops a running job through the normal sequence and ends the agent with `AgentExit::VersionMismatch`; the daemon exits 2 and launchd restarts it. A `409` on a local run only logs - the claim loop meets the same answer on its next poll.
- **`410` follows the Go runner exactly**: on a log chunk it latches the stream as gone and stops delivery only - the run continues and still completes. On a heartbeat it stops the process group and the job is dropped **without** a `complete`. One rule, one place.
- **Cancel is a signal sequence**: `killpg(SIGTERM)`, a 10 s grace, `killpg(SIGKILL)`; the run then completes as `canceled`. Helper daemons that reparent out of the group survive; that is accepted.
- **Reaping and draining are separate awaits**: `RunningJob::wait` only waits for the leader; the pipes are emptied by `drain_output`, which `execute` calls once after the `select!`. A surviving helper holds stdout open, so draining inside `wait` would keep that branch pending and let a late `Cancel` or `Drain` win the `select!` and discard the `0` a finished run already produced - no pull request for work that is already committed.
- **launchd must outwait the drain**: the plist's `ExitTimeOut` is `drain_timeout + STOP_GRACE + COMPLETE_BUDGET`, written by `rxd install` from the `config.toml` it finds. The launchd default is 20 s, which `SIGKILL`s the daemon before the drain and the `complete` are done.
- **Retry is per operation**: `claim` never retries (the loop polls again); `open_run` never retries (it mints a run id and a lease per call, so a retry after a lost response orphans a run); `append_log` and `heartbeat` back off 1 s doubling to 30 s for at most 6 attempts, and any `4xx` is final; `complete` retries with the same backoff **without an attempt limit** until the farm accepts it, answers `410`, or `COMPLETE_BUDGET` elapses - giving up early lets the lease expire and a successful run be finalised `runner_lost`.
- **`seq` is 1-based and spent per attempt**: every send attempt consumes the next value whether or not it lands. Gaps are normal; the farm rejects `seq = 0` and any value not above the last it accepted.
- **Every interval is injectable**: `AgentOptions` carries the heartbeat interval, drain timeout, stop grace, claim retry delay, ticker and `PrTools`, so no test waits on wall-clock time.
- **Profile-based paths**: `cfg!(debug_assertions)` picks `ralphex-macos-runner-dev` over `ralphex-macos-runner` for the application and log directories **and for the launchd label** (`paths::launchd_label`), so neither a development daemon nor a development `rxd install` ever collides with the installed one. All of it lives in `paths.rs`; `service::generate_plist` takes the label rather than reading it.
- **A shutdown waits for the slot**: `Agent::run` returns `Shutdown` only after re-acquiring the run permit, because a run `rxd` started lives in a detached task that the runtime would otherwise drop unfinished - no `complete`, an orphaned process group and a lease the farm finalises `runner_lost`.
- **The heartbeat outlives the process**: the beat task is aborted in `serve_run` after `complete`, not at the process exit, so the log close, the `git push`/`gh pr create` sequence and the completion all happen under a lease that is still being renewed.

### Non-goals (do not add without changing the plan first)

No cloning, fetching, resetting or cleaning of the checkout; no repository list or path mapping (the checkout arrives in `Job.ctx`); one run at a time; no worktree unless `rxd --worktree` asks; no `RALPHEX_CONFIG_DIR` override; no `POST .../progress`; no generated pull request descriptions; no cancel or process listing in `rxd` (both are on the dashboard); no Intel or Linux build.

## Code Style

The `rust-style` and `rustdoc` skills are the rule here, and the gate enforces them:

- **No comments.** No inline explanations, no section dividers, no TODOs, no commented-out code. `grep -rn "^\s*//[^/!]" src/ tests/` must return nothing. Names carry the meaning.
- `///` on every public item: a one-line summary in third person singular present indicative ending with a period, `# Errors` on every fallible public function, `# Panics` where one is possible, `//!` at the top of `lib.rs` and each binary. `# Examples` only where a doctest can run without a farm, a socket or a process.
- `for` loops with mutable accumulators, not `iter().filter().map().collect()` chains. `let ... else` for early exits. `match` covering every variant explicitly - no `_` wildcard, no `matches!`.
- Destructure structs and tuples explicitly, so a new field is a compiler error rather than a silent miss.
- Newtypes for strings with meaning (`RunId`, `Branch`, `Seq`, `RunnerName`, `PrUrl`) and enums over bools in signatures (`CreatePr`, `Worktree`, `Review`).
- No `unsafe` anywhere: process groups go through `CommandExt::process_group` and `nix::sys::signal::killpg`, and tests configure child environments through explicit `env` fields rather than `std::env::set_var`.

Every code change ships with its tests in the same change: unit tests in a `#[cfg(test)] mod tests` at the bottom of the module, integration tests in `tests/`, both success and failure paths.

## The plan

`docs/plans/completed/20260902-ralphex-macos-runner.md` is the plan this repository was built from. It holds the wire contract, the conformance vectors, the job lifecycle table and the reasoning behind the decisions above, including the alternatives that were rejected. Read it before changing protocol behaviour.
