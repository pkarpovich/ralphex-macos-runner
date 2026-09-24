//! The pull request description ralphex's finalize step writes for a run.
//!
//! Before ralphex is spawned the daemon gives the run an [`OutputDir`] of its
//! own and points `FARM_PR_FILE` at [`OutputDir::pr_file`] in it; finalize
//! writes the title and the body there. After a clean exit [`read`] takes the
//! file back and [`parse`] turns it into a [`Description`], rejecting the whole
//! file on any fault. A rejected or missing file leaves the pull request with
//! [`fallback_title`] and an empty body, and [`footer`] is appended to either
//! body. The directory is removed once the run is completed.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::OFlag;

use crate::pr::RunOrigin;
use crate::protocol::types::RunId;

/// The environment variable that tells finalize where to write the description.
pub const PR_FILE_VAR: &str = "FARM_PR_FILE";

/// The name of the description file inside a run's output directory.
pub const PR_FILE_NAME: &str = "pr.md";

/// The largest description file, in bytes, that is read.
pub const READ_LIMIT: u64 = 1024 * 1024;

/// The most characters a title may have.
pub const TITLE_LIMIT: usize = 256;

/// The most characters a body may have.
pub const BODY_CHAR_LIMIT: usize = 60_000;

/// The most bytes a body may have.
pub const BODY_BYTE_LIMIT: usize = 100_000;

const OUTPUT_MODE: u32 = 0o750;
const ELLIPSIS: &str = "...";

/// The title and the body finalize wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    /// The title of the pull request.
    pub title: String,
    /// The body of the pull request, without the farm's footer.
    pub body: String,
}

/// Why a description file cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum Unusable {
    /// No file was written.
    #[error("no description was written")]
    Missing,
    /// The path holds a symlink, a directory or anything else that is not a regular file.
    #[error("the description is not a regular file")]
    NotRegular,
    /// The file holds more than [`READ_LIMIT`] bytes.
    #[error("the description is over {READ_LIMIT} bytes")]
    TooLarge,
    /// The file could not be inspected or read.
    #[error("the description could not be read: {0}")]
    Unreadable(io::Error),
    /// The file is not UTF-8 text.
    #[error("the description is not UTF-8 text")]
    NotText,
    /// The file holds a NUL byte.
    #[error("the description holds a NUL byte")]
    Nul,
    /// Every line of the file is blank.
    #[error("the description has no title line")]
    NoTitle,
    /// The title line holds nothing but `#` and spaces.
    #[error("the description's title is empty")]
    EmptyTitle,
    /// The title has more than [`TITLE_LIMIT`] characters.
    #[error("the description's title is over {TITLE_LIMIT} characters")]
    LongTitle,
    /// Nothing but blank lines follows the title.
    #[error("the description has no body")]
    NoBody,
    /// The body has more than [`BODY_CHAR_LIMIT`] characters or [`BODY_BYTE_LIMIT`] bytes.
    #[error(
        "the description's body is over {BODY_CHAR_LIMIT} characters or {BODY_BYTE_LIMIT} bytes"
    )]
    LongBody,
}

/// Why a run got no output directory.
#[derive(Debug, thiserror::Error)]
pub enum OutputError {
    /// The run id is not a single path segment.
    #[error("run id {0:?} cannot name a directory")]
    RunId(String),
    /// The directory could not be created.
    #[error("{path} could not be created: {source}")]
    Create {
        /// The directory that was asked for.
        path: String,
        /// Why its creation failed.
        source: io::Error,
    },
}

/// The directory one run's finalize writes its description into.
#[derive(Debug)]
pub struct OutputDir {
    path: PathBuf,
}

impl OutputDir {
    /// Creates the directory of `run_id` under `root`, with `root` if it is missing.
    ///
    /// # Errors
    ///
    /// Returns [`OutputError::RunId`] when the run id is empty, `.` or `..` or
    /// holds a `/` or a `\`, and [`OutputError::Create`] when the directory
    /// cannot be created.
    pub fn create(root: &Path, run_id: &RunId) -> Result<OutputDir, OutputError> {
        let segment = run_id.as_str();
        let usable = match segment {
            "" | "." | ".." => false,
            segment => !segment.contains(['/', '\\']),
        };
        if !usable {
            return Err(OutputError::RunId(segment.to_string()));
        }
        let path = root.join(segment);
        let mut builder = DirBuilder::new();
        builder.recursive(true).mode(OUTPUT_MODE);
        if let Err(source) = builder.create(&path) {
            return Err(OutputError::Create {
                path: path.display().to_string(),
                source,
            });
        }
        Ok(OutputDir { path })
    }

    /// Returns the directory's path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the path finalize writes the description to.
    #[must_use]
    pub fn pr_file(&self) -> PathBuf {
        self.path.join(PR_FILE_NAME)
    }

    /// Removes the directory with everything in it.
    ///
    /// # Errors
    ///
    /// Returns the [`io::Error`] of a removal that failed; a directory that is
    /// already gone is not an error.
    ///
    /// [`io::Error`]: std::io::Error
    pub fn remove(self) -> io::Result<()> {
        match fs::remove_dir_all(&self.path) {
            Ok(()) => Ok(()),
            Err(error) => match error.kind() {
                io::ErrorKind::NotFound => Ok(()),
                _other => Err(error),
            },
        }
    }
}

/// Reads and parses the description file at `path`.
///
/// # Errors
///
/// Returns [`Unusable::Missing`] when nothing is at `path`,
/// [`Unusable::NotRegular`] when a symlink, a directory or anything else that
/// is not a regular file is, [`Unusable::TooLarge`] when it holds more than
/// [`READ_LIMIT`] bytes, [`Unusable::Unreadable`] when it cannot be inspected
/// or read, [`Unusable::NotText`] when it is not UTF-8, and every error of
/// [`parse`] for its content.
pub fn read(path: &Path) -> Result<Description, Unusable> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => match error.kind() {
            io::ErrorKind::NotFound => return Err(Unusable::Missing),
            _other => return Err(Unusable::Unreadable(error)),
        },
    };
    if !metadata.file_type().is_file() {
        return Err(Unusable::NotRegular);
    }
    if metadata.len() > READ_LIMIT {
        return Err(Unusable::TooLarge);
    }
    let file = open_regular(path)?;
    let mut content = Vec::new();
    if let Err(error) = file.take(READ_LIMIT + 1).read_to_end(&mut content) {
        return Err(Unusable::Unreadable(error));
    }
    if content.len() as u64 > READ_LIMIT {
        return Err(Unusable::TooLarge);
    }
    let Ok(content) = String::from_utf8(content) else {
        return Err(Unusable::NotText);
    };
    parse(&content)
}

fn open_regular(path: &Path) -> Result<File, Unusable> {
    let opened = OpenOptions::new()
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits())
        .open(path);
    let file = match opened {
        Ok(file) => file,
        Err(error) => return Err(Unusable::Unreadable(error)),
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return Err(Unusable::Unreadable(error)),
    };
    if !metadata.file_type().is_file() {
        return Err(Unusable::NotRegular);
    }
    Ok(file)
}

/// Parses the content of a description file into its title and body.
///
/// The title is the first line that is not blank, trimmed and stripped of
/// every leading `#` and space; the body is every line after it, trimmed.
///
/// # Errors
///
/// Returns [`Unusable::Nul`] for a NUL byte anywhere, [`Unusable::NoTitle`]
/// when every line is blank, [`Unusable::EmptyTitle`] when the title line holds
/// only `#` and spaces, [`Unusable::LongTitle`] for a title over
/// [`TITLE_LIMIT`] characters, [`Unusable::NoBody`] when nothing follows the
/// title, and [`Unusable::LongBody`] for a body over [`BODY_CHAR_LIMIT`]
/// characters or [`BODY_BYTE_LIMIT`] bytes.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::prdesc::{self, Description, Unusable};
///
/// let Description { title, body } =
///     prdesc::parse("\n# Require dials\r\n\r\nEvery dial goes through degradation.\n").unwrap();
/// assert_eq!(title, "Require dials");
/// assert_eq!(body, "Every dial goes through degradation.");
///
/// let Err(Unusable::NoBody) = prdesc::parse("# Require dials\n\n") else {
///     panic!("a description without a body was accepted");
/// };
/// ```
pub fn parse(content: &str) -> Result<Description, Unusable> {
    if content.contains('\0') {
        return Err(Unusable::Nul);
    }
    let content = content.replace("\r\n", "\n");
    let mut title = None;
    let mut body = Vec::new();
    for line in content.split('\n') {
        match title {
            Some(_) => body.push(line),
            None => {
                if !line.trim().is_empty() {
                    title = Some(line);
                }
            }
        }
    }
    let Some(title) = title else {
        return Err(Unusable::NoTitle);
    };
    let title = title.trim().trim_start_matches(['#', ' ']);
    if title.is_empty() {
        return Err(Unusable::EmptyTitle);
    }
    if title.chars().count() > TITLE_LIMIT {
        return Err(Unusable::LongTitle);
    }
    let body = body.join("\n");
    let body = body.trim();
    if body.is_empty() {
        return Err(Unusable::NoBody);
    }
    if body.len() > BODY_BYTE_LIMIT || body.chars().count() > BODY_CHAR_LIMIT {
        return Err(Unusable::LongBody);
    }
    Ok(Description {
        title: title.to_string(),
        body: body.to_string(),
    })
}

/// Returns the title a pull request gets when finalize wrote no usable description.
///
/// A ticket job is titled `<identifier>: <title>`, a local run after the name
/// the farm gave it; a title over [`TITLE_LIMIT`] characters is cut to fit
/// with `...`.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::pr::RunOrigin;
/// use ralphex_macos_runner::prdesc::fallback_title;
///
/// let ticket = RunOrigin::Ticket {
///     identifier: "FARM-12".to_string(),
///     issue_url: String::new(),
///     title: "split farm and runner".to_string(),
/// };
/// assert_eq!(fallback_title(&ticket), "FARM-12: split farm and runner");
///
/// let local = RunOrigin::Local {
///     title: "Require dials".to_string(),
/// };
/// assert_eq!(fallback_title(&local), "Require dials");
/// ```
#[must_use]
pub fn fallback_title(origin: &RunOrigin) -> String {
    let title = match origin {
        RunOrigin::Ticket {
            identifier,
            issue_url: _,
            title,
        } => format!("{identifier}: {title}"),
        RunOrigin::Local { title } => title.clone(),
    };
    if title.chars().count() <= TITLE_LIMIT {
        return title;
    }
    let mut cut = String::new();
    for (taken, character) in title.chars().enumerate() {
        if taken == TITLE_LIMIT - ELLIPSIS.len() {
            break;
        }
        cut.push(character);
    }
    cut.push_str(ELLIPSIS);
    cut
}

/// Returns the footer the farm appends to every pull request body.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::pr::RunOrigin;
/// use ralphex_macos_runner::prdesc::footer;
/// use ralphex_macos_runner::protocol::types::RunId;
///
/// let local = RunOrigin::Local {
///     title: "Require dials".to_string(),
/// };
/// assert_eq!(
///     footer(&local, "/abs/docs/plans/x.md", &RunId("local-1".to_string())),
///     "\n\n---\n\nplan: `/abs/docs/plans/x.md` - run `local-1`\n\nOpened automatically by ralphex-farm."
/// );
/// ```
#[must_use]
pub fn footer(origin: &RunOrigin, plan: &str, run_id: &RunId) -> String {
    let head = format!("plan: `{plan}` - run `{run_id}`");
    let head = match origin {
        RunOrigin::Ticket {
            identifier,
            issue_url,
            title: _,
        } => match issue_url.is_empty() {
            true => format!("{identifier} - {head}"),
            false => format!("[{identifier}]({issue_url}) - {head}"),
        },
        RunOrigin::Local { title: _ } => head,
    };
    format!("\n\n---\n\n{head}\n\nOpened automatically by ralphex-farm.")
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn ticket(issue_url: &str, title: &str) -> RunOrigin {
        RunOrigin::Ticket {
            identifier: "FARM-12".to_string(),
            issue_url: issue_url.to_string(),
            title: title.to_string(),
        }
    }

    fn described(title: &str, body: &str) -> Description {
        Description {
            title: title.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn a_valid_description_parses_into_its_title_and_body() {
        let cases = [
            (
                "Require dials\n\nEvery dial.\n\n- one\n- two\n",
                described("Require dials", "Every dial.\n\n- one\n- two"),
            ),
            (
                "# Require dials\nEvery dial.",
                described("Require dials", "Every dial."),
            ),
            (
                "  ## # Require dials  \nEvery dial.",
                described("Require dials", "Every dial."),
            ),
            (
                "\n   \n\nRequire dials\n\n\nEvery dial.\n\n",
                described("Require dials", "Every dial."),
            ),
            (
                "Require dials\r\n\r\nEvery dial.\r\nSecond line.\r\n",
                described("Require dials", "Every dial.\nSecond line."),
            ),
        ];
        for (content, expected) in cases {
            assert_eq!(parse(content).unwrap(), expected, "{content:?}");
        }
    }

    fn named(error: &Unusable) -> &'static str {
        match error {
            Unusable::Missing => "missing",
            Unusable::NotRegular => "not regular",
            Unusable::TooLarge => "too large",
            Unusable::Unreadable(_) => "unreadable",
            Unusable::NotText => "not text",
            Unusable::Nul => "nul",
            Unusable::NoTitle => "no title",
            Unusable::EmptyTitle => "empty title",
            Unusable::LongTitle => "long title",
            Unusable::NoBody => "no body",
            Unusable::LongBody => "long body",
        }
    }

    #[test]
    fn a_faulty_description_is_rejected_whole() {
        let long_title = format!("{}\nbody", "t".repeat(TITLE_LIMIT + 1));
        let long_body = format!("title\n{}", "b".repeat(BODY_CHAR_LIMIT + 1));
        let heavy_body = format!("title\n{}", "é".repeat(BODY_BYTE_LIMIT / 2 + 1));
        let cases = [
            ("title\nbo\0dy", "nul"),
            ("", "no title"),
            ("\n  \r\n\t\n", "no title"),
            ("#\nbody", "empty title"),
            ("# # #\nbody", "empty title"),
            (long_title.as_str(), "long title"),
            ("title\n\n  \n", "no body"),
            ("title", "no body"),
            (long_body.as_str(), "long body"),
            (heavy_body.as_str(), "long body"),
        ];
        for (content, expected) in cases {
            let Err(error) = parse(content) else {
                panic!("{content:?} was accepted");
            };
            assert_eq!(named(&error), expected, "{content:?}");
        }
    }

    #[test]
    fn a_description_at_every_limit_is_accepted() {
        let title = "t".repeat(TITLE_LIMIT);
        let body = "b".repeat(BODY_CHAR_LIMIT);
        let parsed = parse(&format!("{title}\n{body}")).unwrap();
        assert_eq!(parsed, described(&title, &body));

        let body = "é".repeat(BODY_BYTE_LIMIT / 2);
        let Description {
            title: _,
            body: kept,
        } = parse(&format!("title\n{body}")).unwrap();
        assert_eq!(kept.len(), BODY_BYTE_LIMIT);
    }

    #[test]
    fn a_written_file_is_read_and_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PR_FILE_NAME);
        fs::write(&path, "# Require dials\n\nEvery dial.\n").unwrap();

        assert_eq!(
            read(&path).unwrap(),
            described("Require dials", "Every dial.")
        );
    }

    #[test]
    fn a_missing_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let Err(Unusable::Missing) = read(&dir.path().join(PR_FILE_NAME)) else {
            panic!("a missing file was read");
        };
    }

    #[test]
    fn a_symlink_or_a_directory_is_not_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.md");
        fs::write(&target, "title\nbody\n").unwrap();
        let link = dir.path().join("link.md");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let directory = dir.path().join("directory.md");
        fs::create_dir(&directory).unwrap();

        for path in [link, directory] {
            let Err(Unusable::NotRegular) = read(&path) else {
                panic!("{} was read", path.display());
            };
        }
    }

    #[test]
    fn a_fifo_is_not_a_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fifo.md");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::S_IRWXU).unwrap();

        let Err(Unusable::NotRegular) = read(&path) else {
            panic!("a fifo was read");
        };
    }

    #[test]
    fn a_file_over_the_read_limit_is_too_large() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PR_FILE_NAME);
        let limit = usize::try_from(READ_LIMIT).unwrap();
        fs::write(&path, format!("title\n{}", "b".repeat(limit))).unwrap();

        let Err(Unusable::TooLarge) = read(&path) else {
            panic!("a file over the limit was read");
        };
    }

    #[test]
    fn a_file_that_is_not_utf8_is_not_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PR_FILE_NAME);
        fs::write(&path, [b't', b'\n', 0xff, 0xfe]).unwrap();

        let Err(Unusable::NotText) = read(&path) else {
            panic!("a binary file was read");
        };
    }

    #[test]
    fn a_ticket_with_a_url_links_its_identifier_in_the_footer() {
        assert_eq!(
            footer(
                &ticket("https://linear.app/example/issue/FARM-12", "split"),
                "/abs/docs/plans/x.md",
                &RunId("FARM-12-1".to_string()),
            ),
            concat!(
                "\n\n---\n\n",
                "[FARM-12](https://linear.app/example/issue/FARM-12) - plan: `/abs/docs/plans/x.md` - run `FARM-12-1`",
                "\n\nOpened automatically by ralphex-farm.",
            )
        );
    }

    #[test]
    fn a_ticket_without_a_url_names_its_bare_identifier_in_the_footer() {
        assert_eq!(
            footer(
                &ticket("", "split"),
                "/abs/docs/plans/x.md",
                &RunId("FARM-12-1".to_string()),
            ),
            concat!(
                "\n\n---\n\n",
                "FARM-12 - plan: `/abs/docs/plans/x.md` - run `FARM-12-1`",
                "\n\nOpened automatically by ralphex-farm.",
            )
        );
    }

    #[test]
    fn a_local_run_starts_its_footer_at_the_plan() {
        assert_eq!(
            footer(
                &RunOrigin::Local {
                    title: "Require dials".to_string(),
                },
                "/abs/docs/plans/x.md",
                &RunId("local-1".to_string()),
            ),
            concat!(
                "\n\n---\n\n",
                "plan: `/abs/docs/plans/x.md` - run `local-1`",
                "\n\nOpened automatically by ralphex-farm.",
            )
        );
    }

    #[test]
    fn the_fallback_title_names_the_ticket_or_the_run() {
        assert_eq!(
            fallback_title(&ticket("https://linear.app/x", "split farm and runner")),
            "FARM-12: split farm and runner"
        );
        assert_eq!(
            fallback_title(&RunOrigin::Local {
                title: "20260907-require-dials.md".to_string(),
            }),
            "20260907-require-dials.md"
        );
    }

    #[test]
    fn an_over_long_fallback_title_is_cut_with_an_ellipsis() {
        let exact = "é".repeat(TITLE_LIMIT);
        assert_eq!(
            fallback_title(&RunOrigin::Local {
                title: exact.clone()
            }),
            exact
        );

        let title = fallback_title(&RunOrigin::Local {
            title: "é".repeat(TITLE_LIMIT + 1),
        });
        assert_eq!(title.chars().count(), TITLE_LIMIT);
        assert_eq!(
            title,
            format!("{}...", "é".repeat(TITLE_LIMIT - ELLIPSIS.len()))
        );

        let title = fallback_title(&ticket("", &"t".repeat(TITLE_LIMIT)));
        assert_eq!(title.chars().count(), TITLE_LIMIT);
        assert!(title.starts_with("FARM-12: t"), "{title}");
        assert!(title.ends_with("t..."), "{title}");
    }

    #[test]
    fn an_output_directory_is_made_under_the_root_and_removed_whole() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("farm-out");
        let output = OutputDir::create(&root, &RunId("local-1".to_string())).unwrap();

        assert_eq!(output.path(), root.join("local-1"));
        assert_eq!(output.pr_file(), root.join("local-1").join(PR_FILE_NAME));
        let mode = fs::metadata(output.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, OUTPUT_MODE);
        fs::write(output.pr_file(), "title\nbody\n").unwrap();
        let path = output.path().to_path_buf();

        output.remove().unwrap();
        assert!(!path.exists());
        assert!(root.exists());
    }

    #[test]
    fn an_output_directory_that_is_already_gone_removes_cleanly() {
        let root = tempfile::tempdir().unwrap();
        let output = OutputDir::create(root.path(), &RunId("local-1".to_string())).unwrap();
        fs::remove_dir(output.path()).unwrap();

        output.remove().unwrap();
    }

    #[test]
    fn a_run_id_that_is_not_one_segment_gets_no_output_directory() {
        let root = tempfile::tempdir().unwrap();
        for run_id in ["", ".", "..", "a/b", "a\\b", "/abs"] {
            let Err(OutputError::RunId(refused)) =
                OutputDir::create(root.path(), &RunId(run_id.to_string()))
            else {
                panic!("{run_id:?} named a directory");
            };
            assert_eq!(refused, run_id);
        }
    }

    #[test]
    fn a_root_that_cannot_be_made_gets_no_output_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("file");
        fs::write(&root, "").unwrap();

        let Err(OutputError::Create { path, source: _ }) =
            OutputDir::create(&root, &RunId("local-1".to_string()))
        else {
            panic!("a directory was made under a file");
        };
        assert!(path.ends_with("local-1"), "{path}");
    }
}
