//! The plan-file parser the run title and the timeline read a plan with.
//!
//! [`parse`] follows the farm's container runner line for line: fenced blocks
//! are skipped, `### Task N:` and `### Iteration N:` headers open a task, a
//! `## ` section closes it, the first `# ` heading is the plan's title, and
//! checkboxes inside an open task belong to it. Malformed input is never an
//! error.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::LazyLock;

use nix::fcntl::OFlag;
use regex::Regex;

use crate::protocol::types::TaskStatus;

static FENCE: LazyLock<Regex> = LazyLock::new(|| compile(r"^ {0,3}(`{3,}|~{3,})(.*)$"));
static TASK_HEADER: LazyLock<Regex> =
    LazyLock::new(|| compile(r"^###\s+(?:Task|Iteration)\s+([^:]+?):\s*(.*)$"));
static SECTION: LazyLock<Regex> = LazyLock::new(|| compile(r"^##(?:[^#].*)?$"));
static TITLE: LazyLock<Regex> = LazyLock::new(|| compile(r"^#\s+(.*)$"));
static CHECKBOX: LazyLock<Regex> = LazyLock::new(|| compile(r"^\s*-\s+\[([ xX])\]\s*(.*)$"));

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("plan-file patterns are valid")
}

/// The largest plan, in bytes, [`read`] accepts.
pub const READ_LIMIT: u64 = 1024 * 1024;

/// Why [`read`] could not return a plan's text.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The file could not be opened or read as UTF-8 text.
    #[error("the plan is unreadable: {0}")]
    Unreadable(#[from] io::Error),
    /// The file is larger than [`READ_LIMIT`].
    #[error("the plan is over {READ_LIMIT} bytes")]
    TooLarge,
    /// The path names a directory, a FIFO or anything else that is not a regular file.
    #[error("the plan is not a regular file")]
    NotRegular,
}

/// Reads the text of the plan at `path`, reading at most [`READ_LIMIT`] bytes.
///
/// # Errors
///
/// Returns [`ReadError::Unreadable`] when the file cannot be opened or is not
/// UTF-8 text, [`ReadError::NotRegular`] when it is not a regular file, and
/// [`ReadError::TooLarge`] when it holds more than [`READ_LIMIT`] bytes.
pub fn read(path: &Path) -> Result<String, ReadError> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(OFlag::O_NONBLOCK.bits())
        .open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(ReadError::NotRegular);
    }
    let mut content = String::new();
    file.take(READ_LIMIT + 1).read_to_string(&mut content)?;
    if content.len() as u64 > READ_LIMIT {
        return Err(ReadError::TooLarge);
    }
    Ok(content)
}

/// A parsed plan: its title and its tasks in plan order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    /// The text of the plan's first non-empty `# ` heading, if it has one.
    pub title: Option<String>,
    /// The plan's `### Task` and `### Iteration` sections, in plan order.
    pub tasks: Vec<Task>,
}

/// One `### Task` or `### Iteration` section of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The label between `Task`/`Iteration` and the colon, verbatim.
    pub number: String,
    /// The zero-based position of the task among the plan's tasks.
    pub ord: u32,
    /// The text after the colon.
    pub title: String,
    /// How far the task has come, derived from its checkboxes.
    pub status: TaskStatus,
    /// The task's checkboxes, in plan order.
    pub checkboxes: Vec<Checkbox>,
}

/// One `- [ ]` or `- [x]` line inside a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkbox {
    /// The text after the brackets.
    pub text: String,
    /// Whether the box holds `x` or `X`.
    pub checked: bool,
}

struct OpenTask {
    number: String,
    ord: u32,
    title: String,
    checkboxes: Vec<Checkbox>,
}

impl OpenTask {
    fn close(self) -> Task {
        let OpenTask {
            number,
            ord,
            title,
            checkboxes,
        } = self;
        let status = status(&checkboxes);
        Task {
            number,
            ord,
            title,
            status,
            checkboxes,
        }
    }
}

fn status(checkboxes: &[Checkbox]) -> TaskStatus {
    let mut checked = 0;
    for Checkbox {
        text: _,
        checked: is_checked,
    } in checkboxes
    {
        if *is_checked {
            checked += 1;
        }
    }
    if checked == 0 {
        return TaskStatus::Pending;
    }
    if checked == checkboxes.len() {
        return TaskStatus::Done;
    }
    TaskStatus::Active
}

fn closes_fence(line: &str, opener: &str) -> bool {
    let Some(captures) = FENCE.captures(line) else {
        return false;
    };
    captures[1].starts_with(opener) && captures[2].trim().is_empty()
}

/// Parses a plan file into its title and its tasks.
///
/// Each line, with one trailing `\r` removed, is checked in this order: a
/// fence opens or closes a fenced block whose lines are all skipped; a
/// `### Task N: title` or `### Iteration N: title` header opens a task; a
/// `## ` section closes the open task; the first `# ` heading with text sets
/// the title; a `- [ ]` or `- [x]` line inside an open task becomes one of its
/// checkboxes. A task with every checkbox checked is done, one with some
/// checked is active, and one with none checked or no checkboxes is pending.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::planfile::parse;
/// use ralphex_macos_runner::protocol::types::TaskStatus;
///
/// let plan = parse("# Require dials\n\n### Task 1: Add the parser\n- [x] write it\n- [ ] test it\n");
/// assert_eq!(plan.title.as_deref(), Some("Require dials"));
/// assert_eq!(plan.tasks[0].number, "1");
/// assert_eq!(plan.tasks[0].title, "Add the parser");
/// assert_eq!(plan.tasks[0].status, TaskStatus::Active);
/// ```
#[must_use]
pub fn parse(content: &str) -> Plan {
    let mut title = None;
    let mut tasks = Vec::new();
    let mut open: Option<OpenTask> = None;
    let mut fence: Option<String> = None;
    for line in content.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(opener) = &fence {
            if closes_fence(line, opener) {
                fence = None;
            }
            continue;
        }
        if let Some(captures) = FENCE.captures(line) {
            fence = Some(captures[1].to_string());
            continue;
        }
        if let Some(captures) = TASK_HEADER.captures(line) {
            if let Some(task) = open.take() {
                tasks.push(task.close());
            }
            open = Some(OpenTask {
                number: captures[1].trim().to_string(),
                ord: u32::try_from(tasks.len()).unwrap_or(u32::MAX),
                title: captures[2].trim().to_string(),
                checkboxes: Vec::new(),
            });
            continue;
        }
        if SECTION.is_match(line) {
            if let Some(task) = open.take() {
                tasks.push(task.close());
            }
            continue;
        }
        if let Some(captures) = TITLE.captures(line) {
            let heading = captures[1].trim();
            if title.is_none() && !heading.is_empty() {
                title = Some(heading.to_string());
            }
            continue;
        }
        let Some(task) = open.as_mut() else {
            continue;
        };
        let Some(captures) = CHECKBOX.captures(line) else {
            continue;
        };
        task.checkboxes.push(Checkbox {
            text: captures[2].trim().to_string(),
            checked: &captures[1] != " ",
        });
    }
    if let Some(task) = open {
        tasks.push(task.close());
    }
    Plan { title, tasks }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_returns_a_plan_up_to_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.md");
        let limit = usize::try_from(READ_LIMIT).unwrap();
        std::fs::write(&path, "x".repeat(limit)).unwrap();
        assert_eq!(read(&path).unwrap().len(), limit);
    }

    #[test]
    fn read_refuses_a_plan_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.md");
        let limit = usize::try_from(READ_LIMIT).unwrap();
        std::fs::write(&path, "x".repeat(limit + 1)).unwrap();
        let Err(ReadError::TooLarge) = read(&path) else {
            panic!("a plan over the limit was read");
        };
    }

    #[test]
    fn read_reports_a_missing_or_binary_plan_as_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("binary.md");
        std::fs::write(&binary, [0xff, 0xfe]).unwrap();
        for path in [dir.path().join("missing.md"), binary] {
            let Err(ReadError::Unreadable(_)) = read(&path) else {
                panic!("{} was read", path.display());
            };
        }
    }

    #[test]
    fn read_refuses_a_fifo_or_a_directory_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo.md");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        for path in [fifo, dir.path().to_path_buf()] {
            let Err(ReadError::NotRegular) = read(&path) else {
                panic!("{} was read", path.display());
            };
        }
    }

    fn checkbox(text: &str, checked: bool) -> Checkbox {
        Checkbox {
            text: text.to_string(),
            checked,
        }
    }

    fn task(
        number: &str,
        ord: u32,
        title: &str,
        status: TaskStatus,
        checkboxes: Vec<Checkbox>,
    ) -> Task {
        Task {
            number: number.to_string(),
            ord,
            title: title.to_string(),
            status,
            checkboxes,
        }
    }

    #[test]
    fn parses_title_and_tasks_in_every_status() {
        let content = "# Require dials\n\
            \n\
            ## Overview\n\
            - [ ] not a task checkbox\n\
            \n\
            ### Task 1: Add the parser\n\
            - [x] write it\n\
            \n\
            ### Task 2: Wire it\n\
            - [x] call it\n\
            - [ ] test it\n\
            \n\
            ### Task 3: Document it\n\
            - [ ] write docs\n";
        let plan = parse(content);
        assert_eq!(
            plan,
            Plan {
                title: Some("Require dials".to_string()),
                tasks: vec![
                    task(
                        "1",
                        0,
                        "Add the parser",
                        TaskStatus::Done,
                        vec![checkbox("write it", true)],
                    ),
                    task(
                        "2",
                        1,
                        "Wire it",
                        TaskStatus::Active,
                        vec![checkbox("call it", true), checkbox("test it", false)],
                    ),
                    task(
                        "3",
                        2,
                        "Document it",
                        TaskStatus::Pending,
                        vec![checkbox("write docs", false)],
                    ),
                ],
            }
        );
    }

    #[test]
    fn keeps_task_labels_verbatim() {
        let cases = [
            ("### Iteration 2.5: x", "2.5", "x"),
            (
                "### Task [Final] Update docs: x",
                "[Final] Update docs",
                "x",
            ),
            ("### Task 7:   spaced title  ", "7", "spaced title"),
            ("### Task 8:", "8", ""),
        ];
        for (line, number, title) in cases {
            assert_eq!(
                parse(line).tasks,
                vec![task(number, 0, title, TaskStatus::Pending, Vec::new())],
                "{line}"
            );
        }
    }

    #[test]
    fn ignores_lines_that_are_not_task_headers() {
        let cases = [
            "### Notes: x",
            "#### Task 1: x",
            "### Task 1 without colon",
            "###Task 1: x",
        ];
        for line in cases {
            assert!(parse(line).tasks.is_empty(), "{line}");
        }
    }

    #[test]
    fn section_closes_the_open_task() {
        let content = "### Task 1: a\n- [x] one\n## Post-Completion\n- [ ] after\n";
        let plan = parse(content);
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].checkboxes, vec![checkbox("one", true)]);
        assert_eq!(plan.tasks[0].status, TaskStatus::Done);
    }

    #[test]
    fn bare_double_hash_closes_the_open_task() {
        let plan = parse("### Task 1: a\n##\n- [ ] after\n");
        assert!(plan.tasks[0].checkboxes.is_empty());
    }

    #[test]
    fn deeper_heading_keeps_the_task_open() {
        let plan = parse("### Task 1: a\n#### Details\n- [ ] still inside\n");
        assert_eq!(
            plan.tasks[0].checkboxes,
            vec![checkbox("still inside", false)]
        );
    }

    #[test]
    fn skips_checkboxes_inside_fences() {
        let cases = [
            "### Task 1: a\n```\n- [x] fenced\n```\n- [ ] real\n",
            "### Task 1: a\n~~~\n- [x] fenced\n~~~\n- [ ] real\n",
            "### Task 1: a\n```markdown\n- [x] fenced\n````\n- [ ] real\n",
            "### Task 1: a\n~~~~\n- [x] fenced\n~~~\n- [x] still fenced\n~~~~~  \n- [ ] real\n",
            "### Task 1: a\n   ```\n- [x] fenced\n```\n- [ ] real\n",
            "### Task 1: a\n```\n- [x] fenced\n~~~\n- [x] still fenced\n```\n- [ ] real\n",
            "### Task 1: a\n```\n- [x] fenced\n``` trailing\n- [x] still fenced\n```\n- [ ] real\n",
        ];
        for content in cases {
            let plan = parse(content);
            assert_eq!(
                plan.tasks[0].checkboxes,
                vec![checkbox("real", false)],
                "{content}"
            );
        }
    }

    #[test]
    fn four_spaces_do_not_open_a_fence() {
        let plan = parse("### Task 1: a\n    ```\n- [ ] real\n");
        assert_eq!(plan.tasks[0].checkboxes, vec![checkbox("real", false)]);
    }

    #[test]
    fn skips_headers_inside_fences() {
        let content =
            "```\n# Fenced title\n### Task 1: fenced\n## Fenced section\n```\n# Real title\n";
        let plan = parse(content);
        assert_eq!(plan.title.as_deref(), Some("Real title"));
        assert!(plan.tasks.is_empty());
    }

    #[test]
    fn first_title_wins() {
        let plan = parse("# First\n# Second\n");
        assert_eq!(plan.title.as_deref(), Some("First"));
    }

    #[test]
    fn crlf_parses_like_lf() {
        let content = "# Title\n### Task 1: a\n- [x] one\n- [ ] two\n## Section\n";
        let crlf = content.replace('\n', "\r\n");
        assert_eq!(parse(&crlf), parse(content));
    }

    #[test]
    fn checkbox_forms() {
        let cases = [
            ("- [X] upper", checkbox("upper", true)),
            ("- [x] lower", checkbox("lower", true)),
            ("- [ ] open", checkbox("open", false)),
            ("  - [ ]   indented  ", checkbox("indented", false)),
            ("-  [x]no space", checkbox("no space", true)),
        ];
        for (line, expected) in cases {
            let plan = parse(&format!("### Task 1: a\n{line}\n"));
            assert_eq!(plan.tasks[0].checkboxes, vec![expected], "{line}");
        }
    }

    #[test]
    fn ignores_lines_that_are_not_checkboxes() {
        let cases = ["- [y] other", "* [ ] star", "-[ ] tight", "[ ] bare"];
        for line in cases {
            let plan = parse(&format!("### Task 1: a\n{line}\n"));
            assert!(plan.tasks[0].checkboxes.is_empty(), "{line}");
        }
    }

    #[test]
    fn empty_input_has_no_title_and_no_tasks() {
        assert_eq!(parse(""), Plan::default());
    }

    #[test]
    fn plan_without_task_headers_has_no_tasks() {
        let plan = parse("# Title\n## Overview\n- [ ] loose\n");
        assert_eq!(plan.title.as_deref(), Some("Title"));
        assert!(plan.tasks.is_empty());
    }

    #[test]
    fn task_without_checkboxes_is_pending() {
        let plan = parse("### Task 1: a\nsome prose\n");
        assert_eq!(plan.tasks[0].status, TaskStatus::Pending);
        assert!(plan.tasks[0].checkboxes.is_empty());
    }

    #[test]
    fn blank_title_is_no_title() {
        let cases = ["#   ", "# \t", "#"];
        for line in cases {
            assert_eq!(parse(line).title, None, "{line:?}");
        }
    }

    #[test]
    fn blank_title_leaves_room_for_a_later_one() {
        let plan = parse("#   \n# Later\n");
        assert_eq!(plan.title.as_deref(), Some("Later"));
    }

    #[test]
    fn unclosed_fence_hides_the_rest() {
        let plan = parse("### Task 1: a\n```\n- [x] fenced\n### Task 2: b\n");
        assert_eq!(plan.tasks.len(), 1);
        assert!(plan.tasks[0].checkboxes.is_empty());
    }
}
