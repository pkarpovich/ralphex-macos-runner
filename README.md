# ralphex-macos-runner

A native runner for [ralphex-farm](https://github.com/pkarpovich/ralphex-farm). It runs on a Mac as a launchd user agent and executes ralphex **natively in an existing checkout** - the way you run `ralphex docs/plans/x.md` by hand - instead of inside a Docker task container. It exists for work the container cannot do (Xcode builds, SwiftUI tests against a simulator) while keeping everything the farm gives a run: a row in the runstore, live logs on the dashboard, a lease, cancel from the dashboard, and a pull request at the end.

Two binaries ship together:

- `ralphex-macos-runner` - the daemon. Long-polls the farm for jobs, runs ralphex, streams output, heartbeats, reports completion, opens the pull request.
- `rxd` - the local client. `rxd <plan>` from a project directory opens a run on the farm **without a Linear ticket**, hands it to the daemon and streams the output to the terminal; Ctrl-C detaches and the run keeps going; `rxd attach` reconnects.

Both entry points converge on the same execution path inside the daemon. The only difference is who opened the run and whether a terminal is attached.

## Install

```fish
brew install pkarpovich/apps/ralphex-macos-runner
rxd install
```

`rxd install` copies the daemon binary next to `rxd` to a stable path under `~/Library/Application Support/ralphex-macos-runner/bin/`, writes `~/Library/LaunchAgents/dev.pkarpovich.ralphex-macos-runner.plist` and loads it with `launchctl bootstrap`. The plist carries the `PATH` of the shell that ran the install - launchd agents inherit no login shell, and without it the daemon finds neither `ralphex`, `claude`, `codex`, `gh` nor `xcodebuild`. It prints the paths it touched and the `launchctl` lines to stop and start the agent by hand.

`rxd uninstall` boots the agent out and removes the plist.

Apple silicon only; there is no Intel build and no Linux build (the container runner covers Linux).

## Configure

The daemon refuses to start without `~/Library/Application Support/ralphex-macos-runner/config.toml`:

```toml
farm_url      = "http://farm.example:7077"
token         = "..."
name          = "mbp-native"
drain_timeout = "2m"       # default
ralphex_bin   = "ralphex"  # default
```

`farm_url`, `token` and `name` are required; an unknown key is refused rather than ignored. There is no slot count - this runner takes **one job at a time**, because two ralphex processes in one checkout would write over each other. Keep the file at mode `0600`: it holds the runner token, and the daemon warns when anyone else can read it.

The run uses your personal `~/.config/ralphex` (agents and skills included); `RALPHEX_CONFIG_DIR` is never set.

Logs land in `~/Library/Logs/ralphex-macos-runner/daemon.out.log` and `daemon.err.log`. `RUST_LOG` sets the level (`info` by default).

## Run a plan from the terminal

```fish
cd ~/Projects/my-ios-app
rxd docs/plans/20260902-my-plan.md
```

```
rxd <plan> [--branch <name>] [--no-pr] [--worktree]
rxd attach
rxd install
rxd uninstall
```

- The checkout is the current directory, the plan is made absolute against it, and the branch defaults to the plan file's stem. A pull request is opened unless `--no-pr`.
- `--worktree` is passed straight to ralphex, which then works in a git worktree of the checkout instead of the checkout itself. A claimed job never gets it.
- `rxd` prints `run <run_id>` and the dashboard URL before the first output line, so you can leave immediately.
- Ctrl-C prints `detached; the run continues` and exits 0. `rxd attach` reconnects, replays what has already been printed and then follows live; several terminals may attach at once.
- Staying attached to the end exits 0 for `done` and 1 for `error`.
- `CLAUDE_CONFIG_DIR` is forwarded from your environment when set, so a run started from a work shell uses the work Claude profile.

When the daemon is inside a claim long-poll, `rxd` prints `waiting for the daemon (up to 25 s)` and waits for the poll to return; nothing is aborted, so no job the farm dispatched can be lost. If the poll returns a job - or a run is already going - `rxd` reports the running run id and exits 1.

The daemon runs ralphex in the checkout **as it is**: nothing is cloned, fetched, reset or cleaned, and the checkout is left on the feature branch afterwards, exactly like a run by hand.

## Run a plan from a Linear ticket

Add `runtime: native` and `ctx` to the farm's metadata block. Routing is by runtime alone: a native issue only ever reaches a native runner, and the container runners never see it.

```
<!-- ralphex-farm
repo: my-ios-app
runtime: native
ctx: /Users/me/Projects/my-ios-app
plan: /Users/me/Projects/my-ios-app/docs/plans/20260902-my-plan.md
branch: my-feature-branch
-->
```

Both paths are absolute and the plan must sit inside `ctx` - the farm holds no checkout, so whether they exist is this runner's business. `pr: false` ends the run at the local branch in the checkout - nothing is pushed and no pull request is opened. `mode: review` re-runs only the review pipeline.

## Pull request

After a successful run, and only when the job asked for one: an existing open pull request for the branch is updated with a plain `git push` and its URL reported; otherwise the branch is pushed with `-u`, the base branch is read from `origin/HEAD` (falling back to `gh repo view`), and `gh pr create` opens it. The body is a short fixed block naming the plan, the run and the ticket - the farm's finalize-prompt machinery is not involved, so your personal `finalize_enabled` stays untouched.

## Update

```fish
brew upgrade ralphex-macos-runner
rxd install
```

Homebrew only replaces the binaries in its prefix; `rxd install` copies the new daemon to the stable path launchd runs and reloads the agent. That is the whole update procedure.

## Exit codes and restarts

The daemon exits **2** when the farm answers `409` to a claim or a heartbeat, meaning the two no longer speak the same protocol version. A running job is stopped through the normal signal sequence first, the log line names both versions, and launchd's `KeepAlive` restarts the daemon under its own throttle - so a mismatch shows up as a restart loop in the log, not as a silent runner that claims nothing. Exit 1 is a missing or invalid `config.toml` at startup; exit 0 is a clean shutdown after a drain. A farm that cannot be reached is not a startup failure: the claim loop logs `the claim failed: ...` once per poll and keeps trying.

On `SIGTERM` or `SIGINT` the daemon stops claiming and lets a running job finish for up to `drain_timeout`, then stops it and reports it as `runner_shutdown`. A run `rxd` started is drained the same way: the daemon leaves only once its slot is free again. The plist carries an `ExitTimeOut` covering the whole sequence - `drain_timeout` plus the stop grace plus the budget the completion is retried for - because launchd's default of 20 seconds would `SIGKILL` the daemon mid-drain and leave the farm to finalise the run `runner_lost`. Raising `drain_timeout` therefore needs another `rxd install` to rewrite the plist.

## Development

```fish
mise run check                 # fmt, clippy, tests, actionlint, formula and workflow checks
mise exec -- cargo test        # tests only
mise exec -- cargo run --bin ralphex-macos-runner -- --config /tmp/config.toml
```

A debug build uses `ralphex-macos-runner-dev` for its application and log directories and for its launchd label, so neither the daemon nor `rxd install` ever collides with the installed release. The test suite talks to a fake farm, a fake ralphex and `git`/`gh` shims - nothing touches the real farm, Linear, launchd or the network.

## Release

```fish
# bump version in Cargo.toml, commit, then
git tag v0.1.0
git push origin v0.1.0
```

The tag must match `version` in `Cargo.toml` exactly; the workflow fails if it does not. It runs `mise run check`, builds for `aarch64-apple-darwin`, signs both binaries with a Developer ID, publishes the tarball as a GitHub release and rewrites `Formula/ralphex-macos-runner.rb` in `pkarpovich/homebrew-apps` from `docs/formula-template.rb`. It needs the secrets `MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD` and `HOMEBREW_TAP_TOKEN`, and the repository variable `SIGN_IDENTITY`.
