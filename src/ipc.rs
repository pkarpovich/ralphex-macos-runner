//! The Unix socket the daemon and the `rxd` client speak over.
//!
//! A message is a 4-byte little-endian length followed by that many bytes of
//! JSON, and nothing over [`MAX_MESSAGE_BYTES`] is written or read. [`serve`]
//! owns the daemon's end: it binds the socket with mode `0600`, hands a `Run`
//! command to the agent's run slot and streams the run's output back as
//! [`Response::Line`] messages until the run ends.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};

use crate::agent::{Agent, CurrentRun, LocalStart, raised};
use crate::job::Worktree;
use crate::protocol::types::{Branch, CompleteRequest, CompleteStatus, CreatePr, RunId};

/// The largest message either end sends or accepts.
pub const MAX_MESSAGE_BYTES: usize = 10 * 1024 * 1024;

/// The permissions the daemon's socket carries.
pub const SOCKET_MODE: u32 = 0o600;

/// The permissions a directory the daemon creates for its socket carries.
///
/// An existing directory keeps the permissions it has: the socket path can be
/// given on the command line, and a shared directory is not the daemon's to
/// close down.
pub const DIRECTORY_MODE: u32 = 0o700;

/// What `rxd` asks the daemon to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RunRequest {
    /// The checkout the run executes in.
    pub ctx: String,
    /// The absolute path of the plan file.
    pub plan: String,
    /// The branch ralphex works on.
    pub branch: Branch,
    /// Whether the run ends with a pull request.
    pub create_pr: CreatePr,
    /// Whether ralphex works in a worktree.
    pub worktree: Worktree,
    /// Environment entries added to the daemon's own.
    pub env: Vec<(String, String)>,
}

/// What a client asks the daemon for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Command {
    /// Opens a ticketless run and streams it.
    Run(RunRequest),
    /// Follows the run in progress.
    Attach,
}

/// What the daemon answers a client with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub enum Response {
    /// The run is in flight and its output follows.
    Started {
        /// The identifier the farm gave the run.
        run_id: RunId,
        /// The dashboard page of the run.
        dashboard_url: String,
    },
    /// One line of the run's output.
    Line {
        /// The line, without its newline.
        text: String,
    },
    /// The run reached the farm's records as finished.
    Ended {
        /// How the run ended.
        status: CompleteStatus,
        /// The pull request the run opened, empty when it opened none.
        pr_url: String,
        /// The machine-readable reason a failed run failed.
        fail_reason: String,
    },
    /// Another run holds the daemon's only slot.
    Busy {
        /// The identifier of the run that holds the slot.
        run_id: RunId,
    },
    /// Nothing is running.
    NoRun,
    /// The request could not be served.
    Error {
        /// What went wrong.
        message: String,
    },
}

/// Why a message could not be exchanged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IpcError {
    /// The other end closed the connection.
    #[error("the connection was closed")]
    Closed,
    /// A message was larger than the cap.
    #[error("the message is {0} bytes, over the 10 MiB cap")]
    TooLarge(usize),
    /// The socket could not be read or written.
    #[error("the connection failed: {0}")]
    Io(String),
    /// The message could not be turned into JSON.
    #[error("the message could not be encoded: {0}")]
    Encode(String),
    /// The bytes on the wire were not the expected JSON.
    #[error("the message could not be decoded: {0}")]
    Decode(String),
}

/// Writes `message` to `writer` with its length in front.
///
/// # Errors
///
/// Returns [`IpcError::Encode`] when the message is not encodable,
/// [`IpcError::TooLarge`] when it exceeds [`MAX_MESSAGE_BYTES`] and
/// [`IpcError::Io`] when the socket refuses the bytes.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::ipc::{self, Command};
///
/// let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
/// runtime.block_on(async {
///     let mut wire = Vec::new();
///     ipc::send(&mut wire, &Command::Attach).await.unwrap();
///     let header = u32::from_le_bytes(wire[..4].try_into().unwrap());
///     assert_eq!(header as usize, wire.len() - 4);
///     let mut read = wire.as_slice();
///     let command: Command = ipc::receive(&mut read).await.unwrap();
///     assert_eq!(command, Command::Attach);
/// });
/// ```
pub async fn send<M, W>(writer: &mut W, message: &M) -> Result<(), IpcError>
where
    M: Serialize + ?Sized,
    W: AsyncWrite + Unpin,
{
    let bytes = match serde_json::to_vec(message) {
        Ok(bytes) => bytes,
        Err(error) => return Err(IpcError::Encode(error.to_string())),
    };
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(IpcError::TooLarge(bytes.len()));
    }
    let Ok(length) = u32::try_from(bytes.len()) else {
        return Err(IpcError::TooLarge(bytes.len()));
    };
    if let Err(error) = writer.write_all(&length.to_le_bytes()).await {
        return Err(IpcError::Io(error.to_string()));
    }
    if let Err(error) = writer.write_all(&bytes).await {
        return Err(IpcError::Io(error.to_string()));
    }
    if let Err(error) = writer.flush().await {
        return Err(IpcError::Io(error.to_string()));
    }
    Ok(())
}

/// Reads one length-prefixed message from `reader`.
///
/// # Errors
///
/// Returns [`IpcError::Closed`] when the other end closed the connection,
/// [`IpcError::TooLarge`] when the announced length exceeds
/// [`MAX_MESSAGE_BYTES`], [`IpcError::Io`] when the socket fails and
/// [`IpcError::Decode`] when the bytes are not the expected JSON.
///
/// # Examples
///
/// ```
/// use ralphex_macos_runner::ipc::{self, IpcError, Response};
///
/// let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
/// runtime.block_on(async {
///     let mut empty: &[u8] = &[];
///     let closed = ipc::receive::<Response, _>(&mut empty).await.unwrap_err();
///     assert_eq!(closed, IpcError::Closed);
/// });
/// ```
pub async fn receive<M, R>(reader: &mut R) -> Result<M, IpcError>
where
    M: DeserializeOwned,
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 4];
    if let Err(error) = reader.read_exact(&mut header).await {
        return Err(match error.kind() {
            std::io::ErrorKind::UnexpectedEof => IpcError::Closed,
            std::io::ErrorKind::ConnectionReset => IpcError::Closed,
            _other => IpcError::Io(error.to_string()),
        });
    }
    let length = u32::from_le_bytes(header) as usize;
    if length > MAX_MESSAGE_BYTES {
        return Err(IpcError::TooLarge(length));
    }
    let mut body = vec![0u8; length];
    if let Err(error) = reader.read_exact(&mut body).await {
        return Err(match error.kind() {
            std::io::ErrorKind::UnexpectedEof => IpcError::Closed,
            std::io::ErrorKind::ConnectionReset => IpcError::Closed,
            _other => IpcError::Io(error.to_string()),
        });
    }
    match serde_json::from_slice(&body) {
        Ok(message) => Ok(message),
        Err(error) => Err(IpcError::Decode(error.to_string())),
    }
}

/// Serves clients on the socket at `path` until `shutdown` is raised.
///
/// A stale socket file is removed first, the new one is given
/// [`SOCKET_MODE`], and it is removed again when the listener stops.
///
/// # Errors
///
/// Returns [`IpcError::Io`] when the socket's directory cannot be created, the
/// socket cannot be bound or its permissions cannot be set.
pub async fn serve(
    path: PathBuf,
    agent: Arc<Agent>,
    shutdown: watch::Receiver<bool>,
) -> Result<(), IpcError> {
    let listener = match bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(
                "the client socket {} could not be bound: {error}; rxd cannot reach this daemon",
                path.display()
            );
            return Err(error);
        }
    };
    tracing::info!("the client socket is {}", path.display());
    let mut shutdown = shutdown;
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _raised = raised(&mut shutdown) => break,
        };
        let Ok((stream, _address)) = accepted else {
            continue;
        };
        tokio::spawn(handle(stream, Arc::clone(&agent), shutdown.clone()));
    }
    drop(listener);
    let _ = std::fs::remove_file(&path);
    Ok(())
}

fn bind(path: &Path) -> Result<UnixListener, IpcError> {
    if let Some(parent) = path.parent()
        && let Err(error) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(DIRECTORY_MODE)
            .create(parent)
    {
        return Err(IpcError::Io(error.to_string()));
    }
    let _ = std::fs::remove_file(path);
    let listener = match UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(error) => return Err(IpcError::Io(error.to_string())),
    };
    let permissions = std::fs::Permissions::from_mode(SOCKET_MODE);
    if let Err(error) = std::fs::set_permissions(path, permissions) {
        return Err(IpcError::Io(error.to_string()));
    }
    Ok(listener)
}

async fn handle(stream: UnixStream, agent: Arc<Agent>, shutdown: watch::Receiver<bool>) {
    let mut stream = stream;
    let command = match receive::<Command, _>(&mut stream).await {
        Ok(command) => command,
        Err(error) => {
            tracing::warn!("a client sent no usable command: {error}");
            return;
        }
    };
    let served = match command {
        Command::Run(request) => start(&mut stream, agent, request, shutdown).await,
        Command::Attach => attach(&mut stream, &agent).await,
    };
    if let Err(error) = served {
        tracing::info!("a client left: {error}");
    }
}

async fn start(
    stream: &mut UnixStream,
    agent: Arc<Agent>,
    request: RunRequest,
    shutdown: watch::Receiver<bool>,
) -> Result<(), IpcError> {
    match agent.start_local(request, shutdown).await {
        LocalStart::Busy { run_id } => send(stream, &Response::Busy { run_id }).await,
        LocalStart::Refused { message } => send(stream, &Response::Error { message }).await,
        LocalStart::Started(current) => follow(stream, &current).await,
    }
}

async fn attach(stream: &mut UnixStream, agent: &Agent) -> Result<(), IpcError> {
    let Some(current) = agent.current() else {
        return send(stream, &Response::NoRun).await;
    };
    follow(stream, &current).await
}

async fn follow(stream: &mut UnixStream, current: &CurrentRun) -> Result<(), IpcError> {
    send(
        stream,
        &Response::Started {
            run_id: current.run_id().clone(),
            dashboard_url: current.dashboard_url().to_string(),
        },
    )
    .await?;
    let (replay, mut lines) = current.log().subscribe();
    for text in replay {
        send(stream, &Response::Line { text }).await?;
    }
    let ended = loop {
        let received = tokio::select! {
            received = lines.recv() => received,
            ended = current.ended() => break ended,
        };
        match received {
            Ok(text) => send(stream, &Response::Line { text }).await?,
            Err(broadcast::error::RecvError::Lagged(_missed)) => {}
            Err(broadcast::error::RecvError::Closed) => break current.ended().await,
        }
    };
    while let Ok(text) = lines.try_recv() {
        send(stream, &Response::Line { text }).await?;
    }
    let Some(completion) = ended else {
        return send(
            stream,
            &Response::Error {
                message: "the farm no longer knows this run".to_string(),
            },
        )
        .await;
    };
    let CompleteRequest {
        status,
        pr_url,
        fail_reason,
        message: _,
        log_tail: _,
    } = completion;
    send(
        stream,
        &Response::Ended {
            status,
            pr_url,
            fail_reason,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_command_survives_the_wire() {
        let request = RunRequest {
            ctx: "/abs/checkout".to_string(),
            plan: "/abs/checkout/docs/plans/x.md".to_string(),
            branch: Branch("x".to_string()),
            create_pr: CreatePr::Yes,
            worktree: Worktree::No,
            env: vec![("CLAUDE_CONFIG_DIR".to_string(), "/work".to_string())],
        };
        let mut wire = Vec::new();
        send(&mut wire, &Command::Run(request.clone()))
            .await
            .unwrap();
        let mut read = wire.as_slice();
        let received: Command = receive(&mut read).await.unwrap();
        assert_eq!(received, Command::Run(request));
    }

    #[tokio::test]
    async fn a_length_over_the_cap_is_refused_before_it_is_read() {
        let mut wire = Vec::new();
        let length = u32::try_from(MAX_MESSAGE_BYTES + 1).unwrap();
        wire.extend_from_slice(&length.to_le_bytes());
        let mut read = wire.as_slice();
        let refused = receive::<Response, _>(&mut read).await.unwrap_err();
        assert_eq!(refused, IpcError::TooLarge(MAX_MESSAGE_BYTES + 1));
    }

    #[tokio::test]
    async fn a_message_over_the_cap_is_never_written() {
        let text = "x".repeat(MAX_MESSAGE_BYTES + 1);
        let mut wire = Vec::new();
        let refused = send(&mut wire, &Response::Line { text }).await.unwrap_err();
        let IpcError::TooLarge(size) = refused else {
            panic!("an oversized message is refused for its size");
        };
        assert!(size > MAX_MESSAGE_BYTES);
        assert!(wire.is_empty());
    }

    #[tokio::test]
    async fn an_empty_wire_reads_as_a_closed_connection() {
        let empty: Vec<u8> = Vec::new();
        let mut read = empty.as_slice();
        let closed = receive::<Response, _>(&mut read).await.unwrap_err();
        assert_eq!(closed, IpcError::Closed);
    }

    #[tokio::test]
    async fn a_truncated_body_reads_as_a_closed_connection() {
        let mut wire = Vec::new();
        send(&mut wire, &Response::NoRun).await.unwrap();
        wire.pop();
        let mut read = wire.as_slice();
        let closed = receive::<Response, _>(&mut read).await.unwrap_err();
        assert_eq!(closed, IpcError::Closed);
    }

    #[tokio::test]
    async fn bytes_that_are_not_json_are_refused() {
        let mut wire = Vec::new();
        let length = u32::try_from(3usize).unwrap();
        wire.extend_from_slice(&length.to_le_bytes());
        wire.extend_from_slice(b"{[}");
        let mut read = wire.as_slice();
        let refused = receive::<Response, _>(&mut read).await.unwrap_err();
        let IpcError::Decode(_message) = refused else {
            panic!("unreadable bytes are a decode failure");
        };
    }

    #[tokio::test]
    async fn several_messages_share_one_stream() {
        let mut wire = Vec::new();
        send(
            &mut wire,
            &Response::Line {
                text: "one".to_string(),
            },
        )
        .await
        .unwrap();
        send(
            &mut wire,
            &Response::Line {
                text: "two".to_string(),
            },
        )
        .await
        .unwrap();
        send(&mut wire, &Response::NoRun).await.unwrap();
        let mut read = wire.as_slice();
        let first: Response = receive(&mut read).await.unwrap();
        let second: Response = receive(&mut read).await.unwrap();
        let third: Response = receive(&mut read).await.unwrap();
        assert_eq!(
            first,
            Response::Line {
                text: "one".to_string()
            }
        );
        assert_eq!(
            second,
            Response::Line {
                text: "two".to_string()
            }
        );
        assert_eq!(third, Response::NoRun);
    }
}
