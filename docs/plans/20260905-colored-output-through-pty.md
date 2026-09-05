# Coloured ralphex output through rxd

## Overview

`rxd` prints every line of a run in plain text, while the same ralphex run started by hand from a terminal is coloured. The daemon hands ralphex a pipe for stdout, ralphex asks the OS whether stdout is a terminal, hears "no" and emits no colour codes at all. Nothing downstream can add them back.

This plan gives the ralphex child a pseudo-terminal for its stdout and stderr, so it emits the same escape sequences it would in a terminal, and then splits the one stream into two views inside the daemon: the local history and the attached `rxd` clients receive the bytes as written, the farm and the completion tail receive the same text with the escape sequences removed. The farm, its dashboard and the container runner stay plain, exactly as today; only the terminal a person is looking at gains colour.

Non-goals:

- No colour handling in the farm or its dashboard: the farm never sees an escape sequence after this change, so there is nothing to render or strip on that side.
- No `--color` flag, no `NO_COLOR` handling and no configuration in `rxd` or the daemon: `rxd` keeps colour when its stdout is a terminal and drops it otherwise, nothing else.
- No controlling terminal, no session leadership and no `SIGHUP` semantics for the child: the pseudo-terminal exists to answer `isatty` and to carry bytes; the process-group stop sequence stays the only way the daemon ends a run.
- No terminal size propagation from `rxd` to the pseudo-terminal: `rxd` may attach later, from a different terminal, or never; the size is a fixed constant.
- No colour for the container runner and no change to the wire protocol or the Go side.
- No change to ralphex itself.

Rejected alternatives:

- Forcing colour through an environment variable or a flag: ralphex 1.6.1 has only `--no-color`; its colour library decides by `isatty(stdout)` and honours `NO_COLOR`, nothing forces colour on. This would need an upstream change and still leave the farm side to clean.
- Stripping in the farm or rendering escape sequences in the dashboard: touches the farm, affects both runners, and every stored `output.log` would carry escape codes for the plain text the dashboard shows today.
- Making the pseudo-terminal the child's controlling terminal (`setsid` plus `TIOCSCTTY`): adds hang-up semantics and an `ioctl` the crate does not need; `isatty` is true on a slave without a controlling relationship.
- Reading the master from a blocking thread: `RunningJob::drain_output` aborts a reader that a reparented helper keeps open past the drain budget, and a blocking thread cannot be aborted. The master is read through `tokio::io::unix::AsyncFd`, which the crate's `tokio` features already include.
- Carrying stripping state across the pieces of a line the chunk cap cut: a coloured line longer than 64 KiB does not occur in practice; the stripper works on one emitted piece at a time and the edge is recorded under Technical Details.

## Skills to invoke

Load each skill below with the Skill tool and follow its conventions before implementing any task in this plan.

- `rust-style` - every file under `src/` and `tests/`: `for` loops over iterator chains, `let ... else` for early exits, newtypes over bare strings, enums over bools, exhaustive `match`, explicit destructuring, no comments.
- `rustdoc` - `///` on every public item (crate-level `//!` in `lib.rs` and each `bin`), RFC 1574 summary sentences, `# Errors` and `# Panics` sections where they apply, `# Examples` on the pure function this plan adds.
- Use the rust-analyzer LSP (`goToDefinition`, `findReferences`, `documentSymbol`) for navigation, as `rust-style` requires.

## Context (from discovery)

- `src/job.rs`: `spawn(spec, log)` builds the `tokio::process::Command` with `stdin(null)`, `stdout(piped)`, `stderr(piped)`, `process_group(0)`, `kill_on_drop(true)`, then starts one `pump` task per pipe. `pump` reads `MAX_LOG_CHUNK` at a time into a `LineAssembler`, which calls `LogStream::push_line(line, Terminator)` per emitted line and `push_break` for the newline after a cut. `RunningJob { child, pgid, readers: Vec<JoinHandle<()>> }`; `drain_output(budget)` joins the readers under one deadline and aborts what is left.
- `src/logstream.rs`: `LogStream::push_line` takes one lock and feeds four views of the same line: the outgoing farm buffer (bytes plus the newline), the `history` ring (replayed to a late `rxd attach`), the `tail` ring (carried as `log_tail` in the failed completion) and the `broadcast` channel for live subscribers. It strips one trailing `\r` for the three text views. `push_break` feeds the farm only.
- `src/bin/rxd.rs`: `show(response)` prints `Response::Line { text }` with `println!`; `session`, `follow` and `report` around it own the connection and the exit code.
- `src/ipc.rs`: `Response::Line { text: String }` is the only shape a line crosses the socket in; nothing changes there.
- `src/protocol/types.rs`: `MAX_LOG_CHUNK` 64 KiB, `LOG_TAIL_LINES` 100, `LOG_TAIL_BYTES` 64 KiB, `LOG_BUFFER_BYTES` 4 MiB, `HISTORY_LINES` 2000, `HISTORY_BYTES` 4 MiB.
- `Cargo.toml`: `nix = { version = "0.31.3", features = ["signal", "process", "user"] }`; `tokio` already has `net`, which is what gates `tokio::io::unix::AsyncFd`. No new crate is needed.
- `nix` 0.31 (verified in the registry source): `nix::pty::openpty(winsize, termios) -> Result<OpenptyResult { master: OwnedFd, slave: OwnedFd }>`, `nix::pty::Winsize` (`ws_row`, `ws_col`, `ws_xpixel`, `ws_ypixel`, all `u16`), `nix::sys::termios::{tcgetattr, cfmakeraw, tcsetattr, SetArg}` sit behind the `term` feature. ⚠️ `nix::fcntl::{fcntl, FcntlArg, OFlag}` sit behind the `fs` feature, not `term` (found while implementing task 3), so `Cargo.toml` carries both.
- ralphex 1.6.1 colour decision (verified in its vendored colour library): colour is on iff `NO_COLOR` is unset, `TERM` is not `dumb` and `isatty(stdout)`; `--no-color` only turns it off. Its only other terminal-dependent calls are the window size of stdout and an echo flag on stdin. It draws no spinners and rewrites no lines, so a terminal on stdout adds only SGR colour sequences to the stream.
- Tests: `tests/support/fake-ralphex.sh` is a `sh` script driven by `FAKE_RALPHEX_*` variables; `tests/job.rs` drives `job::spawn` against a `FakeFarm` and reads what the farm received through `requests_ending("/log")` and `text()`; `tests/rxd_e2e.rs` starts the real daemon binary and the real `rxd` binary with a piped stdout and reads its lines; `tests/logstream.rs` covers the rings and the flusher.

## Development Approach

- **testing approach**: Regular (code first, then tests, inside the same task)
- complete each task fully before moving to the next
- make small, focused changes
- **CRITICAL: every task MUST include new/updated tests** for code changes in that task
  - tests are not optional - they are a required part of the checklist
  - write unit tests for new functions/methods
  - write unit tests for modified functions/methods
  - add new test cases for new code paths
  - update existing test cases if behavior changes
  - tests cover both success and error scenarios
- **CRITICAL: all tests must pass before starting next task** - no exceptions
- **CRITICAL: update this plan file when scope changes during implementation**
- run tests after each change
- maintain backward compatibility: the wire protocol, the IPC schema and the config file do not change

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
- `///` doc comments are required on every public item and follow `rustdoc`: a one-line summary in third person singular present indicative ending with a period; `# Errors` on every fallible public function; `# Panics` where a panic is possible; `//!` at the top of `lib.rs` and each `bin`. `# Examples` is required only on public functions a doctest can exercise without a farm, a socket or a process; in this plan that is `ansi::plain`.

**Per-task gate (before marking a checkbox `[x]`):**
1. `mise run check` green: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `actionlint`, `ruby -c` on the formula template, `mise run check-release`.
2. `grep -rn "^\s*//[^/!]" src/ tests/` returns nothing (no line comments); `grep -rn "\.iter()\.\(filter\|map\)\|\.collect::<\|matches!(" src/` returns nothing new; `grep -rn "=> _\|_ =>" src/` returns nothing new; `grep -rn "unsafe" src/ tests/` returns nothing.
3. Every new `pub` item has a `///` block; every new `pub fn` returning `Result` has `# Errors`.
4. Only after 1-3 pass: mark complete.

## Testing Strategy

- **unit tests**: required for every task (see Development Approach above). `src/ansi.rs` carries a table-driven unit test module and a doctest.
- **integration tests**: `tests/job.rs`, `tests/logstream.rs` and `tests/rxd_e2e.rs` already run the real daemon code against a fake farm and the fake ralphex; every task extends the file that owns the behaviour it changes. Nothing in this plan needs the real farm, launchd, ralphex or the network.
- **e2e tests**: none of the UI kind; the `rxd` end-to-end tests in `tests/rxd_e2e.rs` cover the client.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope
- keep plan in sync with actual work done

## Solution Overview

One pseudo-terminal per run. `job::spawn` opens it before the child starts, puts the slave in raw output mode so the line discipline does not rewrite `\n` as `\r\n`, hands the slave to the child as both stdout and stderr, and reads the master through one asynchronous pump that feeds the existing `LineAssembler`. Two pipes become one ordered stream; nothing else about starting, stopping or draining a run changes.

One stripper. `ansi::plain(text)` removes escape sequences from one line of text and is the only place that knows the grammar. `LogStream::push_line` applies it to the two views the farm sees (the outgoing buffer and the completion tail) and leaves the two local views (the replay history and the live subscribers) as written. `rxd` applies the same function to every line when its own stdout is not a terminal.

The farm therefore keeps receiving exactly what it receives today, the dashboard and `output.log` stay plain, and a person watching `rxd` in a terminal sees ralphex's colours.

## Technical Details

### Pseudo-terminal setup (in `job::spawn`)

Private constants in `src/job.rs`: `PTY_ROWS: u16 = 40`, `PTY_COLS: u16 = 120`. ralphex reads the width of stdout once for wrapping; 120 columns is the width its output is laid out for.

Order of operations, each step a precondition of the next:

1. `openpty(Some(&winsize), None)` with the constants above. Failure is `JobError::SpawnFailed` naming the error, the same variant an unspawnable binary produces.
2. `tcgetattr(&slave)`, `cfmakeraw(&mut termios)`, `tcsetattr(&slave, SetArg::TCSANOW, &termios)`. Without this the slave's default output processing (`OPOST` with `ONLCR`) turns every `\n` the child writes into `\r\n` on the master, and every line would reach the farm with a trailing carriage return. Raw mode also disables input echo, which is irrelevant with `stdin(null)` but harmless.
3. `slave.try_clone()` for a second descriptor; `stdout(Stdio::from(first))`, `stderr(Stdio::from(second))`, `stdin(Stdio::null())`. `process_group(0)` and `kill_on_drop(true)` stay.
4. `fcntl(&master, F_SETFL(O_NONBLOCK))`, then `AsyncFd::new(master)`. `AsyncFd` requires a non-blocking descriptor; a blocking master would stall the runtime on the first read.
5. Spawn the child. After `spawn` returns, the `Command` (and with it every parent-side slave descriptor) must be gone before `RunningJob` is returned. The master reports end-of-output only once no process holds a slave descriptor; a slave kept open in the daemon would keep every pump alive forever.

Master pump, replacing the two pipe pumps: `readable().await`, then `try_io` with a `read` into the `MAX_LOG_CHUNK` buffer; `WouldBlock` clears readiness and loops; `Ok(0)` ends; `Err` with `EIO` ends, because macOS and Linux report `EIO` rather than a zero read on a master whose slaves are all closed; any other error ends too. Every successful read goes to the same `LineAssembler` and the pump finishes with `LineAssembler::finish`, as today. `RunningJob.readers` holds this one task; `drain_output` and `stop` are unchanged. A helper that reparented out of the process group and kept the slave open behaves exactly as it does with a pipe today: the pump stays alive until the drain budget ends and is aborted then.

`push_line` keeps stripping one trailing `\r` for the text views; the child may still write one itself.

### Escape-sequence grammar (in `src/ansi.rs`)

`pub fn plain(text: &str) -> String` works on characters and removes, wherever it finds U+001B (`ESC`):

- **CSI**: `ESC [`, then any run of characters in U+0030..=U+003F (parameters), then any run in U+0020..=U+002F (intermediates), then exactly one final character in U+0040..=U+007E. The whole sequence is removed. `ESC [ 3 2 m` and `ESC [ 0 m` are the cases ralphex emits; `ESC [ 1 ; 3 1 m`, `ESC [ K` and `ESC [ 2 J` must also disappear.
- **OSC**: `ESC ]`, then everything up to and including the first `BEL` (U+0007) or the two-character `ST` (`ESC \`). Removed.
- **Any other escape**: `ESC` followed by one character. Both are removed.
- **Truncated sequence**: an `ESC` whose sequence does not complete before the end of `text` is removed together with everything after it. This is the piece the chunk cap cut mid-sequence; the continuation piece then starts with the sequence's remaining bytes as literal text, and the plan accepts that for lines over 64 KiB.

Everything else - tabs, carriage returns, any non-ASCII character - is kept as it is. A `text` without `ESC` comes back equal to its input.

### The split in `LogStream::push_line`

| view | today | after this plan |
|---|---|---|
| outgoing farm buffer (`/log` chunks) | `line` bytes + newline | `plain(line)` bytes + newline |
| `tail` ring (`log_tail` on failure) | text | `plain(text)` |
| `history` ring (replay on attach) | text | text, as written |
| `lines` broadcast (live `rxd`) | text | text, as written |

The lock, the `LOG_BUFFER_BYTES` bound (now measured on the stripped bytes), the `MAX_LOG_CHUNK` fill notification and `push_break` are unchanged.

### `rxd`

A private enum in `src/bin/rxd.rs`, `Palette { Keep, Strip }`, decided once per invocation from `std::io::stdout().is_terminal()` (`std::io::IsTerminal`) and passed to `show`. `Response::Line { text }` prints `text` under `Keep` and `ansi::plain(&text)` under `Strip`. `install`, `uninstall` and every diagnostic line on stderr are untouched.

### Fake ralphex

`tests/support/fake-ralphex.sh` gains one variable, `FAKE_RALPHEX_COLOR`. When it is non-empty the script prints, before its other output: `tty: yes` when `[ -t 1 ]` holds and `tty: no` otherwise, then one line whose text is the word `green` wrapped in `ESC [ 3 2 m` and `ESC [ 0 m` (emitted with `printf` and octal escapes), and then one line `pieces` written as `printf '\033[3'`, a `sleep 0.05`, `printf '2mlate\033[0m\n'` - a sequence split across two writes, to show the assembler and the stripper meet at line level, not at read level.

### Testing `rxd` on a terminal

`tests/support/mod.rs` gains `rxd_on_terminal(checkout, args, env) -> (Child, AsyncFd<OwnedFd>)` (name and shape at the executor's discretion): it opens a pseudo-terminal with the same `nix` calls, gives the slave to `rxd` as stdout, keeps the master and reads it as the terminal would. The existing `rxd_argv` keeps the piped stdout. Reading the master follows the same `EIO`-as-end rule as the daemon's pump.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code, tests and README changes in this repository.
- **Post-Completion** (no checkboxes): the release, the upgrade on the Mac and the look at a real run.

## Implementation Steps

### Task 1: Add the escape-sequence stripper

**Files:**
- Modify: `Cargo.toml`
- Create: `src/ansi.rs`
- Modify: `src/lib.rs`

- [x] add `"term"` to the `nix` feature list in `Cargo.toml` and confirm `cargo build` still succeeds with nothing else changed
- [x] create `src/ansi.rs` with `pub fn plain(text: &str) -> String` implementing the grammar in Technical Details, documented with a one-line summary and an `# Examples` doctest that strips `ESC [ 3 2 m ok ESC [ 0 m` to `ok`
- [x] add `pub mod ansi;` to `src/lib.rs` and a sentence to the crate docs naming the two views the stripper separates
- [x] write a table-driven unit test module in `src/ansi.rs`: plain text unchanged, one SGR pair, several sequences in one line, a multi-parameter SGR, `ESC [ K`, an OSC ended by `BEL`, an OSC ended by `ST`, a bare `ESC` plus one character, a truncated `ESC [ 3` at the end, a tab and a carriage return kept, a line with Cyrillic and an emoji kept whole
- [x] write the error-shaped cases: an empty string, a string that is only `ESC`, a string that is only a complete sequence (all give an empty result)
- [x] run `mise run check` - must pass before task 2

### Task 2: Split the farm's views from the local views in the log stream

**Files:**
- Modify: `src/logstream.rs`
- Modify: `tests/logstream.rs`

- [x] in `LogStream::push_line`, feed the outgoing buffer and the `tail` ring the stripped line (`ansi::plain` over the lossy text, re-encoded as bytes for the buffer) and keep the `history` ring and the broadcast on the text as written; keep the single lock, the `\r` strip, the buffer bound and the fill notification
- [x] update the `///` on `push_line` so it names which two views are plain and which two keep the sequences, and update the module docs at the top of `src/logstream.rs`
- [x] write `the_farm_and_the_tail_get_plain_text_while_the_history_keeps_the_colour` in `tests/logstream.rs`: subscribe first, push one coloured line, close, and assert the farm's received text has no `ESC`, `tail()` has no `ESC`, the replay from a fresh `subscribe()` contains the sequence, and the live receiver got the sequence
- [x] write `a_coloured_line_counts_against_the_buffer_bound_by_its_plain_bytes`: push lines whose coloured length exceeds `LOG_BUFFER_BYTES` while their plain length does not, and assert nothing was dropped from the farm's copy
- [x] write `a_line_that_is_only_an_escape_sequence_reaches_the_farm_as_an_empty_line`: the farm receives the newline, the tail has an empty line
- [x] run `mise run check` - must pass before task 3

### Task 3: Give the run a pseudo-terminal

**Files:**
- Modify: `src/job.rs`
- Modify: `tests/support/fake-ralphex.sh`
- Modify: `tests/job.rs`

- [x] in `job::spawn`, open the pseudo-terminal, set the slave raw, hand two slave descriptors to the child as stdout and stderr, set the master non-blocking and wrap it in `AsyncFd`, following the five ordered steps in Technical Details; map every failure before the spawn to `JobError::SpawnFailed`
- [x] replace the two `pump` tasks with one master pump that treats `EIO` and a zero read as the end, feeds the existing `LineAssembler` and finishes it; make sure no parent-side slave descriptor outlives `spawn`
- [x] update the `///` on `spawn` and `RunningJob` (it now drains one pseudo-terminal rather than two pipes) and the module docs at the top of `src/job.rs`
- [x] extend `tests/support/fake-ralphex.sh` with `FAKE_RALPHEX_COLOR` as specified in Technical Details
- [x] write `the_run_sees_a_terminal_on_stdout` in `tests/job.rs`: spawn the fake with `FAKE_RALPHEX_COLOR=1` and assert the farm's text contains `tty: yes`
- [x] write `escape_sequences_reach_the_subscribers_but_not_the_farm`: subscribe before spawning, assert the live receiver got a line containing `\u{1b}[32m`, the farm's text contains `green` and `late` with no `ESC` anywhere, and `tail()` has no `ESC`
- [x] write `a_newline_reaches_the_farm_without_a_carriage_return`: with the raw slave, the farm's text for `FAKE_RALPHEX_LINES=3` contains no `\r`
- [x] run the existing `tests/job.rs` cases unchanged - `both_pipes_reach_the_log_stream` (now `both_output_descriptors_reach_the_log_stream`), `a_megabyte_long_line_is_chunked_for_the_farm_and_split_for_subscribers`, `a_helper_holding_the_pipes_does_not_hold_up_the_exit_status` (now `a_helper_holding_the_terminal_...`), `a_pipe_the_drain_gave_up_on_stops_feeding_the_log` (now `a_terminal_the_drain_gave_up_on_...`), `stopping_takes_the_whole_process_group_down`, `a_group_member_that_ignores_the_signal_is_killed_within_the_grace` - and rename the ones whose names say "pipes" to say what they now hold
- [x] ⚠️ `a_line_written_in_pieces_reaches_the_farm_unbroken` could not stay unchanged: one pseudo-terminal merges stdout and stderr into one ordered stream, so an unterminated stdout write joins the stderr line written after it, exactly as a terminal shows it. Renamed to `a_line_written_in_pieces_reaches_the_farm_in_the_order_it_was_written` and rewritten to assert the merged order, no loss, and that the farm's copy and the subscribers' copy carry the same stream
- [x] run `mise run check` - must pass before task 4

### Task 4: Keep colour in rxd only on a terminal

**Files:**
- Modify: `src/bin/rxd.rs`
- Modify: `tests/support/mod.rs`
- Modify: `tests/rxd_e2e.rs`

- [x] add the private `Palette` enum to `src/bin/rxd.rs`, decide it once from `std::io::stdout().is_terminal()` where the session starts, pass it to `show`, and print `Response::Line` through `ansi::plain` under `Strip`
- [x] update the `//!` at the top of `src/bin/rxd.rs` with one sentence on when colour is kept
- [x] add the pseudo-terminal launcher for `rxd` to `tests/support/mod.rs` as described in Technical Details, alongside the existing piped `rxd_argv`
- [x] write `a_client_in_a_pipe_prints_plain_text` in `tests/rxd_e2e.rs`: run the fake with `FAKE_RALPHEX_COLOR=1` through the daemon and the piped `rxd`, assert the client's lines contain `green` and no `ESC`, and the fake farm's text contains no `ESC`
- [x] write `a_client_on_a_terminal_keeps_the_colour`: same run through the pseudo-terminal launcher, assert the bytes read from the master contain `\u{1b}[32m`
- [x] write `a_late_attach_replays_the_colour_it_missed`: attach on a terminal after the run printed, assert the replay carries the sequence
- [x] run `mise run check` - must pass before task 5

### Task 5: Verify acceptance criteria

- [x] verify all requirements from Overview are implemented: the child sees a terminal (`the_run_sees_a_terminal_on_stdout`), the farm and the tail are plain and the history and the live clients carry the sequences (`escape_sequences_reach_the_subscribers_but_not_the_farm`, `the_farm_and_the_tail_get_plain_text_while_the_history_keeps_the_colour`), `rxd` strips in a pipe and keeps on a terminal (`a_client_in_a_pipe_prints_plain_text`, `a_client_on_a_terminal_keeps_the_colour`, `a_late_attach_replays_the_colour_it_missed`)
- [x] verify the edge cases: a truncated sequence at a chunk cut (`a_truncated_sequence_takes_the_rest_of_the_line_with_it` covers both halves - the cut piece loses its dangling `ESC`, the continuation piece keeps the remaining bytes as literal text, which is the behaviour Technical Details accepts), a line that is only a sequence (`a_line_that_is_only_an_escape_sequence_reaches_the_farm_as_an_empty_line`), a helper holding the slave past the drain budget (`a_helper_holding_the_terminal_does_not_hold_up_the_exit_status`, `a_terminal_the_drain_gave_up_on_stops_feeding_the_log`), `EIO` on the master ending the pump (the `Ok(Err(_closed)) => break` arm; every `tests/job.rs` case that drains after the child exits would burn its whole budget without it)
- [x] run the full gate: `mise run check` - green, 0 failures across all suites, clippy clean under `-D warnings`
- [x] run the code-quality greps from the gate over `src/` and `tests/` and confirm nothing new - all four return nothing
- [x] confirm `cargo tree -e features -i nix` shows `term` and `fs` and no other new feature, and `Cargo.lock` gained no new crate - the tree lists `default`, `signal`, `process`, `user`, `feature` (transitive of the pre-existing `user`), `term` and `fs`; `Cargo.lock` is identical to `master`

### Task 6: Update documentation and bump the version

**Files:**
- Modify: `README.md`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] in `README.md`, under "Run a plan from the terminal", add a paragraph: ralphex runs on a pseudo-terminal and its colours reach `rxd` when `rxd`'s stdout is a terminal; in a pipe `rxd` prints plain text; the farm, the dashboard and the completion tail always receive plain text
- [ ] in `README.md`, adjust the architecture bullet that says the daemon "streams output" to say it streams plain text to the farm and the terminal's bytes to `rxd`
- [ ] bump `version` in `Cargo.toml` to `0.2.0` and refresh `Cargo.lock` with `cargo update --workspace`
- [ ] run `mise run check` one last time
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes, informational only*

**Release:**
- Merge, then tag `v0.2.0` on the merge commit and push the tag; the release workflow checks the tag against `Cargo.toml`, signs, publishes and rewrites the formula in the tap.
- On the Mac: `brew upgrade ralphex-macos-runner`, then `rxd install` while no run is in progress (it refuses otherwise); the plist and the copied daemon are replaced.

**Manual verification:**
- `rxd <plan> --no-pr` from a checkout in a terminal shows ralphex's colours; the same command piped through `cat` shows plain text; `rxd attach` from a second terminal replays the colours.
- The run's page on the farm dashboard and the `log_tail` of a failed run show plain text with no `[32m` fragments.
- The daemon log carries no new warnings; a run stopped with Ctrl-C on the daemon still ends `runner_shutdown` within the drain.
