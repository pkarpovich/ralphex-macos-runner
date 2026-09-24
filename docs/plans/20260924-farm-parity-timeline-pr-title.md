# Farm parity: timeline, generated pull request description and run title

## Overview

Three things a container run on the farm has and a native run does not:

1. **No timeline.** The dashboard draws a run's timeline only from plan-progress snapshots the runner posts to `POST /api/runner/jobs/{run_id}/progress`. The container runner watches the plan file, parses its tasks and checkboxes, reads the phase off section markers in the log and posts snapshots; this daemon posts none, so a native run shows only "Agent working / elapsed".
2. **No generated pull request description.** A container run gets a title and body written by ralphex's finalize step, which the farm enables and points at a file through `FARM_PR_FILE`. This daemon opens every pull request titled after the branch with a fixed three-line body, and every one of them has been retitled and rewritten by hand afterwards.
3. **A file name for a run name.** A ticketless run has no Linear title, so the farm names it after the plan file (`20260907-require-dials-through-degradation.md`). The farm now accepts an optional `title` on `POST /api/runner/runs` (ralphex-farm PR #53, merged) and falls back to the file name without it.

This plan makes the daemon do what the container runner does, in the same way. Every rule below is the container runner's behaviour, written out here so this plan is the whole specification; the few places a native run forces a difference are called out as such.

The operator's side is already in place and is not part of this plan: `finalize_enabled = true` in the personal ralphex config, the farm's finalize prompt at `prompts/finalize.txt` there (it tells the finalize session to write the title and body to `$FARM_PR_FILE`, and to write nothing when the variable is unset), and `--skip-finalize` on the `rx`/`rxw` aliases so runs by hand never finalize.

**Acceptance scenario** (the operator's real use): from a checkout such as `turtle-hub`, on a branch holding only the plan, `rxd docs/plans/<plan>.md` with a pull request. On the dashboard the run is named after the plan's `# ` heading, its timeline lists the plan's tasks and ticks them as the agent works while the phase moves `setup` -> `tasks` -> `review` -> `pr`; the pull request opens with the title and body finalize wrote plus the farm's footer, and nobody edits it afterwards.

**Non-goals:**

- No refresh of an existing pull request: when one is already open for the branch the daemon pushes and reports its URL exactly as today, without `gh pr edit`. The operator always starts from a fresh branch holding only the plan.
- No new phase: the phase domain stays `setup | tasks | review | pr`. The finalize step, and every ralphex 1.7 section the marker rules below do not name, leave the phase where it is.
- No change to the marker rules: they are the container runner's two expressions exactly, even though ralphex 1.7 prints sections they do not match (`review N`, `custom review iteration N`, `<tool> external review`, `finalize step`). Native and container runs behave the same.
- The daemon never writes the ralphex config or any prompt: finalize being enabled is the operator's configuration.
- No polling of the plan file, no retry of a progress post, no progress for a run that never spawned ralphex.
- No change for ticket jobs beyond what the one execution path gives them: their name still comes from Linear.

**Rejected alternatives:**

- Polling the plan every few seconds instead of filesystem events: the container runner uses events with a debounce, and this plan follows it.
- A daemon-owned `claude -p` session writing the description after ralphex exits: finalize inside ralphex is already covered by the run's process group, its stop sequence, the drain and the log, and it is what the container runner uses.
- `--config-dir` pointing ralphex at a daemon-owned config with finalize enabled: it would replace the operator's agents and skills for the run.
- Refreshing an existing pull request with `gh pr edit`: see the non-goal above; the case does not occur in the operator's workflow.

## Skills to invoke

Load each skill below with the Skill tool and follow its conventions before implementing any task in this plan.

- `rust-style` - every file under `src/` and `tests/`: `for` loops over iterator chains, `let ... else` for early exits, newtypes over bare strings, enums over bools, exhaustive `match`, explicit destructuring, no comments.
- `rustdoc` - `///` on every public item, RFC 1574 summary sentences, `# Errors` and `# Panics` sections where they apply, `# Examples` on the pure public functions this plan adds (`planfile::parse`, the description parser, the footer and fallback-title builders, the marker scanner).
- Use the rust-analyzer LSP (`goToDefinition`, `findReferences`, `documentSymbol`) for navigation, as `rust-style` requires.

## Context (from discovery)

- `src/agent.rs`: `Agent::start_local` builds the `OpenRunRequest` (fields `runner, version, runtime, repo, ctx, plan, branch, create_pr`) and calls `FarmClient::open_run`. `Agent::execute` validates the job, spawns ralphex through `job::spawn(&spec, log)`, waits in `ended(...)` against the terminal channel (`Cancel`, `Drain`, `Gone`, `VersionMismatch`), and on a clean exit runs `finish(...)`, which drains the output, closes the log and, for `CreatePr::Yes`, calls `pr::open_pull_request` with the `PrSpec` from `PrSpec::describe`. `stopped(...)` turns a terminal into the completion (`canceled`, `runner_shutdown`, none for `Gone` and `VersionMismatch`).
- `src/pr.rs`: `PrSpec { branch, title, body }`, `PrSpec::describe(branch, &RunOrigin, plan, run_id)` builds today's title (`<identifier>: <title>` for a ticket, the plan's file stem for a local run) and body (`Plan: ...`, `Run: ...`, optional `Resolves ...`, `Automated by ralphex-macos-runner.`). `open_pull_request` lists an open pull request for the branch (`gh pr list --head <branch> --state open --json url --jq .[0].url`); if one exists it pushes and returns its URL, otherwise it pushes with `-u`, resolves the base and runs `gh pr create --head --base --title --body`.
- `src/job.rs`: `JobSpec { ctx, plan, branch, review, local, ralphex_bin }`; `spawn` sets `LocalOptions.env` entries on the child and hands it a pseudo-terminal.
- `src/logstream.rs`: `LogStream::push_line(line, terminator)` is the single place every emitted line passes; it strips one trailing `\r` and feeds the farm buffer, the tail, the history and the live broadcast. Lines carry ralphex's colour escape sequences; `ansi::plain` removes them.
- `src/protocol/types.rs`: wire types and constants (`REQUEST_TIMEOUT` 30 s, `PR_BUDGET`, ...). `src/protocol/client.rs`: `FarmClient` with a per-operation retry policy.
- `src/service.rs`: `exit_timeout(drain_timeout)` sums every await on the shutdown path into the plist's `ExitTimeOut`; CLAUDE.md requires every new await on that path to be added there.
- `src/paths.rs`: `app_dir()` is `~/Library/Application Support/ralphex-macos-runner` (`-dev` for a debug build).
- Tests: `tests/support/fake_farm.rs` (scripted replies per route plus a request recorder; routes today: claim, runs, log, heartbeat, complete), `tests/support/fake-ralphex.sh` (driven by `FAKE_RALPHEX_*` variables), `git`/`gh` shims under `tests/support/bin`, `tests/protocol_vectors.rs` (byte-exact JSON vectors), `tests/agent_e2e.rs`, `tests/rxd_e2e.rs`.
- Dependencies to add: `notify` (latest stable release, not a release candidate; default features, which is the FSEvents backend on macOS) and `regex`. Nothing else.

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
- maintain backward compatibility: a farm without `title` support ignores the field; every existing wire vector stays byte-identical

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
- `///` doc comments are required on every public item and follow `rustdoc`: a one-line summary in third person singular present indicative ending with a period; `# Errors` on every fallible public function; `# Panics` where a panic is possible; `//!` at the top of `lib.rs`, each `bin` and each new module. `# Examples` is required only on public functions a doctest can exercise without a farm, a socket or a process.

**Per-task gate (before marking a checkbox `[x]`):**
1. `mise run check` green: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `actionlint`, `ruby -c` on the formula template, `mise run check-release`.
2. `grep -rn "^\s*//[^/!]" src/ tests/` returns nothing (no line comments); `grep -rn "\.iter()\.\(filter\|map\)\|\.collect::<\|matches!(" src/` returns nothing new; `grep -rn "=> _\|_ =>" src/` returns nothing new; `grep -rn "unsafe" src/ tests/` returns nothing.
3. Every new `pub` item has a `///` block; every new `pub fn` returning `Result` has `# Errors`.
4. Only after 1-3 pass: mark complete.

## Testing Strategy

- **unit tests**: required for every task. The plan parser, the marker scanner, the description parser, the footer and the fallback title are pure and carry table-driven unit tests plus doctests.
- **integration tests**: the watcher is tested against a real temporary directory with a recording progress sender and short debounce/retry seams; the end-to-end behaviour runs the real agent against the fake farm, the fake ralphex and the `git`/`gh` shims. Nothing needs the real farm, ralphex, `gh`, launchd or the network.
- **e2e tests**: none of the UI kind.

## Progress Tracking

- mark completed items with `[x]` immediately when done
- add newly discovered tasks with ➕ prefix
- document issues/blockers with ⚠️ prefix
- update plan if implementation deviates from original scope
- keep plan in sync with actual work done

## Solution Overview

- **Run title.** Before opening a local run the daemon parses the plan with the same parser the timeline uses and sends its title as `OpenRunRequest.title`, omitted when the plan has none. The farm returns the name it chose in `Job.title`.
- **Timeline.** A plan watcher per run, started after validation and before ralphex is spawned: it watches the plan's directory and the `completed/` directory beside it with `notify`, debounces bursts, re-resolves the plan file on every snapshot, parses it and posts snapshots from a single task. A phase tracker fed by `LogStream::push_line` moves the phase on section markers. The agent posts `pr` itself before pushing and a terminal `failed` snapshot when a spawned run ends in error.
- **Pull request description.** Before spawning, the daemon creates `farm-out/<run_id>/` under its application directory and sets `FARM_PR_FILE` on the child to `pr.md` in it. After a clean exit and before the push it reads and validates that file; a valid one supplies the title and body, anything else falls back. The farm's footer is always appended. The directory is removed when the run is over.

## Technical Details

### Wire

`OpenRunRequest` gains `title: Option<String>`, serialized only when `Some` (`skip_serializing_if`), placed last. Vector for a request with a title, byte-exact (it mirrors the farm's own golden for this case):

```
{"runner":"mbp","version":"1","runtime":"native","repo":"nhop","ctx":"/Users/op/Projects/nhop","plan":"/Users/op/Projects/nhop/docs/plans/20260907-require-dials.md","branch":"20260907-require-dials","create_pr":false,"title":"Require dials through degradation"}
```

The existing vectors without a title stay unchanged.

New types, field order and JSON keys exactly as listed:

- `ProgressRequest { phase: Phase, failed: bool, tasks: Option<Vec<ProgressTask>> }`. `tasks: None` serializes as `null` and means "the plan could not be read, keep what you hold"; `Some(vec![])` is a plan with no tasks.
- `ProgressTask { number: String, ord: u32, title: String, status: TaskStatus, checkboxes: Vec<ProgressCheckbox> }`; `checkboxes` is always an array, never `null`.
- `ProgressCheckbox { text: String, checked: bool }`.
- `Phase` serializes as `"setup" | "tasks" | "review" | "pr"`; `TaskStatus` as `"pending" | "active" | "done"`.

Vectors:

```
{"phase":"setup","failed":false,"tasks":null}
{"phase":"tasks","failed":true,"tasks":[{"number":"1","ord":0,"title":"Add the parser","status":"done","checkboxes":[{"text":"write it","checked":true}]},{"number":"2","ord":1,"title":"Wire it","status":"active","checkboxes":[{"text":"call it","checked":true},{"text":"test it","checked":false}]},{"number":"3","ord":2,"title":"Document it","status":"pending","checkboxes":[]}]}
```

`FarmClient::post_progress(&self, run_id: &RunId, request: &ProgressRequest) -> Result<(), FarmError>`: `POST {farm}/api/runner/jobs/{run_id}/progress`, JSON body, bearer token, `REQUEST_TIMEOUT`. `204` is success. It is **never retried**: snapshots supersede each other. A `410` latches the run id as gone inside the client; every later call for that run returns the gone error without a request.

### Plan parser (`src/planfile.rs`)

`pub fn parse(content: &str) -> Plan` with `Plan { title: Option<String>, tasks: Vec<Task> }`, `Task { number: String, ord: u32, title: String, status: TaskStatus, checkboxes: Vec<Checkbox> }`, `Checkbox { text: String, checked: bool }`. Line by line, each line with one trailing `\r` removed:

1. **Fences.** A line matching `^ {0,3}(` + "`" + `{3,}|~{3,})(.*)$` opens a fence (remember capture 1) when none is open; while one is open every line is skipped, and a line matching the same expression whose capture 1 starts with the remembered fence and whose capture 2 is blank after trimming closes it (and is skipped too).
2. **Task header.** `^###\s+(?:Task|Iteration)\s+([^:]+?):\s*(.*)$` starts a task: `number` is capture 1 trimmed, `title` capture 2 trimmed, `ord` the count of tasks before it; it becomes the open task.
3. **Section.** `^##(?:[^#].*)?$` closes the open task.
4. **Title.** `^#\s+(.*)$` sets the plan title to capture 1 trimmed, only if no title was set yet; an empty result counts as no title.
5. **Checkbox.** `^\s*-\s+\[([ xX])\]\s*(.*)$` inside an open task appends `{ text: capture 2 trimmed, checked: capture 1 != " " }`.

The order of the checks matters: fence, task header, section, title, checkbox. Then each task's status: every checkbox checked and at least one checkbox -> `done`; some checked -> `active`; none -> `pending`. Malformed input is never an error: at worst an empty task list and no title.

### Run title

In `start_local`, before `open_run`: read the plan (at most 1 MiB; a read error or a larger file gives no title), `planfile::parse`, send `title` when `Some`. The run's name then comes back in `Job.title`.

### Phase tracker and markers

A marker is judged on `ansi::plain` of the line `push_line` receives, after its `\r` strip. A line longer than 256 bytes is never a marker. Two expressions, matched against the whole line:

- `^--- task iteration \d+ ---$` -> phase `tasks`, and request a snapshot even if the phase did not change.
- `^--- (claude review \d+.*|codex external review|codex iteration \d+|claude evaluating codex findings) ---$` -> phase `review`.

Any other line changes nothing. Setting a phase records it for every later snapshot and requests one. The phase starts as `setup`. Once the agent has posted `pr` the phase is frozen: later markers (a drained tail) change nothing.

### Plan watcher (`src/progress.rs`)

Configured with the progress sender, the run id, the **expected** plan path, a debounce (default 300 ms) and an attach retry (default 2 s).

- **Expected path.** Without a worktree it is the job's plan path. With `--worktree` it is `<ctx>/.ralphex/worktrees/<branch>/<plan relative to ctx>`; the branch is used verbatim as path segments, so a branch with `/` gives nested directories. This is the one native difference: the container runner's base is its own clone.
- **Resolve**, afresh for every snapshot: the first of `<expected>` and `<dir of expected>/completed/<file name>` that exists and is not a directory. None -> the plan is not on disk right now.
- **Snapshot** = `{ phase: current phase, failed: false, tasks }`, where `tasks` is `None` when the plan does not resolve, cannot be read, or parses to zero tasks, and otherwise the parsed tasks mapped one to one.
- **Watching.** A `notify` watcher, non-recursive, on the expected directory and on its `completed/` sibling. Adding a directory that does not exist yet fails; the expected directory then falls back to watching its nearest existing ancestor (never the expected directory itself), and both are retried on every attach-retry tick and on every event until they attach; once the expected directory attaches, the ancestor watch is dropped. `completed/` has no ancestor fallback: its parent is the expected directory. An event is interesting only if it is a create, modify (data or name) or remove, and one of its paths has the plan's file name; compare file names only, because FSEvents reports `/private/var/...` for a `/var/...` directory. An interesting event restarts the debounce; when the debounce elapses a snapshot is requested.
- **Posting.** One task owns every post, so two posts are never in flight and their order is total. Requests coalesce: while one is pending, more requests add nothing. The first thing that task does is post a snapshot with phase `setup` (the plan is often unreadable then, which sends `tasks: null`), under its own 10 s deadline, before it waits for requests. A failed post is logged at warn and forgotten.
- **Stop** ends the watcher and the posting task. **Post phase** (used for `pr`): record the phase, freeze it, and post a snapshot synchronously on the caller under a 10 s deadline. **Post failure**: post a snapshot with `failed: true` synchronously under a 10 s deadline. Both are called after stop, so they are the run's last posts. The 10 s deadline is a new constant `PROGRESS_POST_TIMEOUT`.

### Where the agent drives the watcher

- Start it after `job::validate` succeeds and before `job::spawn`; register its phase tracker with the run's `LogStream` so `push_line` feeds it.
- Clean exit with `CreatePr::Yes`: stop, post phase `pr`, then read the description, then push and open the pull request.
- Clean exit with `CreatePr::No`: stop; no `pr` phase.
- A spawned run that is completed as an error (`nonzero_exit`, `canceled`, `runner_shutdown`, any pull-request failure): stop, post failure, then complete.
- `Gone` and `VersionMismatch`: stop only; the farm has no use for a snapshot.
- A run refused before spawning (runtime mismatch, validation, spawn failure) never had a watcher.

### Pull request description (`src/prdesc.rs`)

- **Output directory.** `<app dir>/farm-out/<run_id>/`, mode `0750`, created before spawn. The run id is used as one path segment only if it contains no `/` or `\` and is not `.` or `..`; otherwise, or if creation fails, log a warning, set no `FARM_PR_FILE`, and the run falls back. On success the child gets `FARM_PR_FILE=<that dir>/pr.md`. The directory is removed with everything in it once the run is completed, on every path; a removal failure is a warning. The daemon binary passes the `farm-out` root to the agent; tests point it at a temporary directory.
- **Reading**, after a clean exit and before the push: `lstat` the file; it must be a regular file (a symlink, a directory or a FIFO is rejected) of at most 1 MiB; read at most 1 MiB.
- **Parsing** the content; any failure rejects the whole file:
  - a NUL byte anywhere -> rejected;
  - normalize `\r\n` to `\n`; the title line is the first line that is not blank after trimming; none -> rejected;
  - title = that line trimmed, then with every leading `#` and space removed; empty -> rejected; more than 256 characters -> rejected;
  - body = every line after the title line joined with `\n`, trimmed; empty -> rejected; more than 60000 characters or more than 100000 bytes -> rejected.
- **Fallback** on any rejection or a missing file, with the reason logged at warn: title = `<identifier>: <job title>` for a ticket job and `<job title>` for a local run, cut to 253 characters plus `...` when over 256; body empty.
- **Footer**, appended to the generated or the empty fallback body: `"\n\n---\n\n" + head + "\n\nOpened automatically by ralphex-farm."`, where `head` is `` <id> - plan: `<plan path>` - run `<run id>` `` and `<id>` is `[<identifier>](<issue url>)` when the job has an issue URL, the bare identifier when it has only an identifier, and nothing for a local run, in which case `head` starts at `plan:`. The plan path stays text, because the plan moves into `completed/`.
- The resulting title and body replace what `PrSpec::describe` produced; `open_pull_request` is otherwise unchanged, including an existing open pull request being pushed and returned untouched.

### Shutdown budget

`exit_timeout` gains `PROGRESS_POST_TIMEOUT` once: a run finishing during the drain posts either `pr` or `failed`, never both.

## What Goes Where

- **Implementation Steps** (`[ ]` checkboxes): code, tests and documentation in this repository.
- **Post-Completion** (no checkboxes): the release, the upgrade on the Mac and the look at a real run.

## Implementation Steps

### Task 1: Add the progress wire types, the run title field and the progress call

**Files:**
- Modify: `src/protocol/types.rs`
- Modify: `src/protocol/client.rs`
- Modify: `tests/protocol_vectors.rs`
- Modify: `tests/client.rs`
- Modify: `tests/support/fake_farm.rs`

- [x] add `ProgressRequest`, `ProgressTask`, `ProgressCheckbox`, `Phase`, `TaskStatus` and `PROGRESS_POST_TIMEOUT` to `src/protocol/types.rs` as specified under "Wire", and `title: Option<String>` to `OpenRunRequest`, skipped when `None`
- [x] add `FarmClient::post_progress` to `src/protocol/client.rs`: never retried, `REQUEST_TIMEOUT`, `410` latched per run id so later calls for that run send nothing
- [x] add a progress route to the fake farm (scripted replies, default `204`, recorded like the others)
- [x] add the three vectors from "Wire" to `tests/protocol_vectors.rs` and confirm every existing vector still passes unchanged
- [x] write client tests: `204` succeeds; a `500` is returned without a second attempt; a `410` returns the gone error and a second call for the same run makes no request, while a call for another run still does
- [x] run `mise run check` - must pass before task 2

### Task 2: Parse the plan file

**Files:**
- Modify: `Cargo.toml`
- Create: `src/planfile.rs`
- Modify: `src/lib.rs`

- [x] add `regex` with `cargo add regex`
- [x] create `src/planfile.rs` with `pub fn parse(content: &str) -> Plan` and the `Plan`, `Task`, `Checkbox` types following "Plan parser" exactly, including the order of the checks, with a doctest
- [x] add `pub mod planfile;` to `src/lib.rs`
- [x] write table-driven tests: a plan with a title and three tasks in `done`/`active`/`pending`; `### Iteration 2.5: x` and `### Task [Final] Update docs: x` keep their labels verbatim; checkboxes after a `## ` section are not attached; checkboxes inside a fenced block (both backtick and tilde, with a longer closing fence) are ignored; a `# ` heading inside a fence is not the title; the first `# ` heading wins; `\r\n` input parses like `\n`; `X` counts as checked
- [x] write the edge cases: empty input, a plan with no task headers, a task with no checkboxes is `pending`, a `# ` line with only spaces is no title
- [x] run `mise run check` - must pass before task 3

### Task 3: Name a local run after its plan

**Files:**
- Modify: `src/agent.rs`
- Modify: `tests/rxd_e2e.rs`

- [x] in `Agent::start_local`, read the plan (at most 1 MiB), parse it and set `OpenRunRequest.title`, as specified under "Run title"; a read failure is not an error and sends no title
- [x] write a test in `tests/rxd_e2e.rs`: a plan starting with `# Require dials` opens a run whose recorded `runs` request carries `"title":"Require dials"`
- [x] write tests for the fallback: a plan without a `# ` heading and a plan the daemon cannot read both open a run whose recorded request has no `title` key
- [x] run `mise run check` - must pass before task 4

### Task 4: Track the phase from the section markers

**Files:**
- Create: `src/progress.rs`
- Modify: `src/lib.rs`
- Modify: `src/logstream.rs`
- Modify: `tests/logstream.rs`

- [x] create `src/progress.rs` with the marker scanner and the phase tracker as specified under "Phase tracker and markers": a pure `fn` that classifies one line into task-iteration, review or nothing (doctested), and a tracker holding the current phase, the frozen flag and a snapshot-request signal
- [x] give `LogStream` an optional phase tracker set once per run, and call it from `push_line` with the plain text of each line, outside the buffer lock
- [x] write table-driven scanner tests: both expressions match their examples (`--- task iteration 12 ---`, `--- claude review 0: all findings ---`, `--- codex external review ---`, `--- codex iteration 3 ---`, `--- claude evaluating codex findings ---`); near misses do not (`--- review 1 ---`, `--- finalize step ---`, `--- task iteration x ---`, a marker with trailing text); a coloured marker matches after stripping; a 257-byte line never matches
- [x] write tracker tests: the phase starts `setup`; a task iteration requests a snapshot even when the phase is already `tasks`; freezing ignores later markers
- [x] write a `tests/logstream.rs` test: lines pushed through a stream with a tracker move its phase
- [x] run `mise run check` - must pass before task 5

### Task 5: Watch the plan and post snapshots

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/progress.rs`
- Create: `tests/progress.rs`

- [x] add `notify` with `cargo add notify` (latest stable, default features)
- [x] implement the plan watcher in `src/progress.rs` as specified under "Plan watcher": expected path and resolve, snapshot building, non-recursive watches on the plan directory and `completed/` with the ancestor fallback and attach retry, file-name filtering, debounce, the single coalescing posting task with the initial `setup` snapshot, stop, post phase and post failure; the progress sender is a trait so tests can record, and the debounce and attach retry are constructor parameters
- [x] bridge `notify`'s callback thread into the tokio task through a channel; the watcher and its thread end with stop
- [x] write tests in `tests/progress.rs` against a temporary directory and a recording sender, with a 20 ms debounce and a 50 ms retry: the first post is `setup` with `tasks: null` when the plan is absent; creating the plan posts its tasks; ticking a checkbox posts the new status; moving the plan into `completed/` keeps posting from there; a plan directory created after start (the worktree case) attaches through its ancestor and posts
- [x] write tests for the edges: a burst of writes inside the debounce posts once; an unreadable or task-less plan posts `tasks: null`; a sender error does not stop later posts; post phase `pr` freezes and is the last post after stop; post failure carries `failed: true` and the re-read tasks
- [x] run `mise run check` - must pass before task 6
- ➕ a directory is only handed to `notify` once it exists: its FSEvents backend stops and restarts the whole stream on every `watch` call, failed ones included, so retrying a missing `completed/` dropped the plan's own events. Attaching a directory after start requests a snapshot, because the plan may have landed there before the watch did
- ➕ the bounded plan read moved from `agent.rs` to `planfile::read` (`READ_LIMIT`, `ReadError`), shared by the run title and the snapshots

### Task 6: Drive the watcher from the run

**Files:**
- Modify: `src/agent.rs`
- Modify: `src/job.rs`
- Modify: `tests/support/fake-ralphex.sh`
- Modify: `tests/support/mod.rs`
- Modify: `tests/agent_e2e.rs`

- [ ] in `Agent::execute`, start the watcher after validation and before spawn with the expected path from "Plan watcher" (worktree-aware), register its tracker on the run's log stream, and apply every rule under "Where the agent drives the watcher"; the posting uses the agent's `FarmClient`
- [ ] add debounce and attach-retry fields to `AgentOptions`, defaulting to 300 ms and 2 s, so tests shorten them
- [ ] extend `tests/support/fake-ralphex.sh` with variables that: print a given marker line; tick every checkbox of the plan it was given (in place); move the plan into `completed/`; with `--worktree`, copy the plan to `<cwd>/.ralphex/worktrees/<branch>/<same relative path>` and tick it there
- [ ] write `tests/agent_e2e.rs` tests: a clean run with a pull request posts `setup` first, then tasks after the fake ticks the plan, then `tasks`/`review` phases from the markers, and `pr` as its last snapshot before the push; a `--no-pr` run posts no `pr`; a worktree run posts the tasks read from the worktree copy
- [ ] write tests for failures: a nonzero exit posts a last snapshot with `failed: true` before the completion; a canceled run does too; a run refused at validation posts nothing
- [ ] run `mise run check` - must pass before task 7

### Task 7: Use the description finalize wrote

**Files:**
- Create: `src/prdesc.rs`
- Modify: `src/lib.rs`
- Modify: `src/pr.rs`
- Modify: `src/agent.rs`
- Modify: `src/paths.rs`
- Modify: `src/bin/ralphex-macos-runner.rs`
- Modify: `tests/support/fake-ralphex.sh`
- Modify: `tests/agent_e2e.rs`
- Modify: `tests/pr.rs`

- [ ] create `src/prdesc.rs` with the output-directory handling, the reader, the parser, the fallback title and the footer exactly as specified under "Pull request description"; the parser, the fallback title and the footer are pure and doctested
- [ ] add the `farm-out` root to `src/paths.rs` and pass it from the daemon binary into `AgentOptions`
- [ ] in `Agent::execute`, create the run's output directory and add `FARM_PR_FILE` to the child's environment before spawn, read the description after `pr` is posted and before the push, build the pull request's title and body from it plus the footer, and remove the directory once the run is completed on every path
- [ ] replace the title and body `PrSpec::describe` builds in `src/pr.rs` with the ones from `src/prdesc.rs`, leaving the push, the existing-pull-request check and `gh pr create` untouched; update its doctests
- [ ] extend `tests/support/fake-ralphex.sh` with a variable whose value it writes to `$FARM_PR_FILE`
- [ ] write parser tests: a valid file; a leading `# ` on the title; blank lines before the title; `\r\n`; a NUL byte, no title, a title of only `#`, no body, a 257-character title, a body over 60000 characters and one over 100000 bytes are each rejected
- [ ] write reader tests: a missing file, a symlink, a directory and a file over 1 MiB are rejected; the footer for a ticket with a URL, a ticket without one and a local run; the fallback title for a ticket, a local run and an over-long title
- [ ] write `tests/agent_e2e.rs` tests: the child sees `FARM_PR_FILE`; a written description reaches `gh pr create` with the footer appended; no file or an invalid file opens with the fallback title and a footer-only body; the output directory is gone after a done, a failed and a canceled run
- [ ] run `mise run check` - must pass before task 8

### Task 8: Budget the progress post in the shutdown timeout

**Files:**
- Modify: `src/service.rs`

- [ ] add `PROGRESS_POST_TIMEOUT` to `service::exit_timeout` and to its doc comment listing the awaits
- [ ] update the test that pins the sum so it includes the new term
- [ ] run `mise run check` - must pass before task 9

### Task 9: Verify acceptance criteria

- [ ] verify all three requirements from Overview: a local run carries its plan's title; snapshots follow the watcher and marker rules with `pr` and `failed` last; the pull request uses a valid finalize description with the footer and falls back otherwise
- [ ] verify the non-goals held: no `gh pr edit` anywhere, no phase outside `setup | tasks | review | pr`, the two marker expressions unchanged, no write under any ralphex config directory
- [ ] run the full gate: `mise run check`
- [ ] run the code-quality greps from the gate over `src/` and `tests/` and confirm nothing new
- [ ] confirm `Cargo.toml` gained exactly `notify` and `regex`

### Task 10: [Final] Update documentation, bump the version and close the plan

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] `README.md`: a section on the timeline (what is posted and when), on the generated description (finalize must be enabled in the personal ralphex config with the farm's prompt, and `--skip-finalize` belongs on by-hand aliases; without it the pull request opens with the fallback), and the run name
- [ ] `CLAUDE.md`: the new modules in the module map, the watcher and description rules under Key Patterns, and the new non-goals (no pull request refresh, no new phases, marker rules identical to the farm's)
- [ ] bump `version` in `Cargo.toml` to `0.3.0` and refresh `Cargo.lock` with `cargo update --workspace`
- [ ] run `mise run check` one last time
- [ ] move this plan to `docs/plans/completed/`

## Post-Completion

*Items requiring manual intervention or external systems - no checkboxes, informational only*

**Release:**
- Confirm the farm on bravo runs a revision that includes ralphex-farm PR #53 (`docker logs` shows `master-a31e7c0-...` or later) before tagging; an older farm ignores `title` and the run keeps its file name.
- Merge, tag `v0.3.0` on the merge commit, push the tag; the release workflow signs, publishes and rewrites the tap formula.
- On the Mac: `brew upgrade ralphex-macos-runner`, then `rxd install` while no run is in progress.

**Manual verification (the acceptance scenario):**
- From a checkout on a branch holding only a new plan, `rxd docs/plans/<plan>.md` with a pull request: the dashboard names the run after the plan's heading, the timeline lists its tasks, ticks them live and moves through `setup`, `tasks`, `review`, `pr`; the pull request opens with finalize's title and body and the farm footer.
- A `--no-pr` run shows the timeline without a `pr` phase; a run killed from the dashboard ends with its active task marked failed.
- The daemon log carries no `unusable` warning for the description on a normal run.
