//! Linux CLIPRDR backend with clipboard change events and delayed rendering.
//!
//! Wayland uses the shared [`crate::cliprdr::data_control`] client. X11/XWayland uses
//! XFixes selection notifications and selection ownership. Text and images are
//! requested on paste; file lists and contents are fetched before offering URIs.
//! Plain text (`CF_UNICODETEXT`), PNG images (`CF_DIB`/`CF_DIBV5`), and file
//! copy/paste travel in both directions. Remote files are streamed to a confined
//! staging directory before their URIs are offered to local applications.

pub mod data_control;
pub mod files;
pub mod os;
pub mod stub;
pub mod worker;
pub mod x11;

use core::fmt;
use core::time::Duration;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;

use ironrdp_cliprdr::backend::{ClipboardMessageProxy, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId,
};
use ironrdp_core::{IntoOwned as _, impl_as_any};
use tracing::{debug, warn};

use self::worker::{Command, Worker};

/// How long [`LinuxClipboard::new`] waits for the OS clipboard to open. A stale
/// `DISPLAY` or an unresponsive compositor must not stall the session setup.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum LinuxCliprdrError {
    /// No usable clipboard: neither a Wayland data-control clipboard nor an X11
    /// display could be opened.
    Clipboard(String),
    Spawn(std::io::Error),
    /// The clipboard thread stopped before reporting whether it could start.
    WorkerStopped,
    /// The OS clipboard did not open within the startup timeout.
    TimedOut,
}

impl fmt::Display for LinuxCliprdrError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Clipboard(error) => write!(f, "cannot open the OS clipboard: {error}"),
            Self::Spawn(error) => write!(f, "cannot start the clipboard thread: {error}"),
            Self::WorkerStopped => f.write_str("the clipboard thread stopped unexpectedly"),
            Self::TimedOut => f.write_str("the OS clipboard did not open in time"),
        }
    }
}

impl core::error::Error for LinuxCliprdrError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Clipboard(_) => None,
            Self::Spawn(error) => Some(error),
            Self::WorkerStopped | Self::TimedOut => None,
        }
    }
}

/// The Linux clipboard bridge. Owns the thread that talks to the OS clipboard;
/// keep it alive for as long as the connection it serves, and build one
/// [`CliprdrBackend`] per channel initialization through
/// [`backend_factory`](Self::backend_factory).
pub struct LinuxClipboard {
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl fmt::Debug for LinuxClipboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LinuxClipboard").finish_non_exhaustive()
    }
}

impl LinuxClipboard {
    /// Opens the OS clipboard and starts the clipboard thread.
    ///
    /// `message_proxy` delivers the resulting `CLIPRDR` messages to the RDP
    /// session; it is called from the clipboard thread.
    ///
    /// Gives up with [`LinuxCliprdrError::TimedOut`] if the OS clipboard does not
    /// open within 10 seconds. A thread still blocked in the OS call is left
    /// behind and exits without starting the bridge if the call later returns.
    pub fn new(
        message_proxy: impl ClipboardMessageProxy + 'static,
    ) -> Result<Self, LinuxCliprdrError> {
        Self::with_staging_root(message_proxy, None)
    }

    pub fn with_staging_root(
        message_proxy: impl ClipboardMessageProxy + 'static,
        staging_root: Option<std::path::PathBuf>,
    ) -> Result<Self, LinuxCliprdrError> {
        let (commands, receiver) = mpsc::channel();
        let (ready_sender, ready) = mpsc::channel();

        let events = commands.clone();
        let thread = std::thread::Builder::new()
            .name("cliprdr-linux".to_owned())
            .spawn(move || {
                let os = match os::NativeClipboard::open(events) {
                    Ok(os) => os,
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                if ready_sender.send(Ok(())).is_err() {
                    // `new` timed out and nobody holds the bridge.
                    return;
                }
                Worker::new(os, message_proxy, staging_root).run(&receiver);
                debug!("Clipboard thread stopped");
            })
            .map_err(LinuxCliprdrError::Spawn)?;

        match ready.recv_timeout(STARTUP_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                commands,
                thread: Some(thread),
            }),
            Ok(Err(error)) => Err(LinuxCliprdrError::Clipboard(error)),
            Err(RecvTimeoutError::Timeout) => Err(LinuxCliprdrError::TimedOut),
            Err(RecvTimeoutError::Disconnected) => Err(LinuxCliprdrError::WorkerStopped),
        }
    }

    /// Advertise dropped local files to the remote over CLIPRDR.
    pub fn offer_files(&self, paths: Vec<std::path::PathBuf>) {
        if let Err(error) = self.commands.send(Command::OfferFiles(paths)) {
            warn!(%error, "Could not queue dropped files for clipboard transfer");
        }
    }

    pub fn backend_factory(&self) -> Box<dyn CliprdrBackendFactory + Send> {
        Box::new(LinuxCliprdrBackendFactory {
            commands: self.commands.clone(),
        })
    }
}

impl Drop for LinuxClipboard {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct LinuxCliprdrBackendFactory {
    commands: Sender<Command>,
}

impl CliprdrBackendFactory for LinuxCliprdrBackendFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        let _ = self.commands.send(Command::Reset);
        Box::new(LinuxCliprdrBackend {
            commands: self.commands.clone(),
            remote_epoch: 0,
        })
    }
}

/// Forwards the `CLIPRDR` callbacks to the clipboard thread.
#[derive(Debug)]
pub struct LinuxCliprdrBackend {
    commands: Sender<Command>,
    remote_epoch: u64,
}

impl_as_any!(LinuxCliprdrBackend);

impl LinuxCliprdrBackend {
    fn send(&self, command: Command) {
        if self.commands.send(command).is_err() {
            warn!("The clipboard thread is gone; dropping a CLIPRDR event");
        }
    }
}

impl CliprdrBackend for LinuxCliprdrBackend {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::STREAM_FILECLIP_ENABLED
            | ClipboardGeneralCapabilityFlags::FILECLIP_NO_FILE_PATHS
            | ClipboardGeneralCapabilityFlags::CAN_LOCK_CLIPDATA
    }

    fn on_ready(&mut self) {
        self.send(Command::BackendReady);
    }

    fn on_request_format_list(&mut self) {
        self.send(Command::AdvertiseLocal);
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        debug!(?capabilities, "CLIPRDR capabilities negotiated");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        self.remote_epoch = self.remote_epoch.wrapping_add(1);
        self.send(Command::RemoteCopy(
            available_formats.to_vec(),
            self.remote_epoch,
        ));
    }

    fn on_remote_file_list(
        &mut self,
        files: &[ironrdp_cliprdr::pdu::FileDescriptor],
        clip_data_id: Option<u32>,
    ) {
        self.send(Command::RemoteFileList(
            files.to_vec(),
            clip_data_id,
            self.remote_epoch,
        ));
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        self.send(Command::RenderLocal(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let data = (!response.is_error() && response.data().len() <= worker::MAX_TRANSFER_BYTES)
            .then(|| response.data().to_vec());
        self.send(Command::RemoteData(data));
    }

    fn on_file_contents_request(&mut self, request: FileContentsRequest) {
        self.send(Command::Upload(request));
    }

    fn on_file_contents_response(&mut self, response: FileContentsResponse<'_>) {
        let response = if response.data().len() > 1024 * 1024 {
            FileContentsResponse::new_error(response.stream_id()).into_owned()
        } else {
            response.into_owned()
        };
        self.send(Command::RemoteFileData(response));
    }

    fn on_lock(&mut self, data_id: LockDataId) {
        self.send(Command::Lock(data_id));
    }

    fn on_unlock(&mut self, data_id: LockDataId) {
        self.send(Command::Unlock(data_id));
    }

    fn now_ms(&self) -> u64 {
        crate::cliprdr::native_now_ms()
    }
}

use std::sync::OnceLock;
use std::time::Instant;

pub use crate::cliprdr::stub::{StubClipboard, StubCliprdrBackend};

/// Process-wide monotonic clock epoch for `CliprdrBackend::now_ms()` on native platforms.
fn epoch() -> &'static Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now)
}

/// Returns monotonic milliseconds since process start.
pub fn native_now_ms() -> u64 {
    u64::try_from(epoch().elapsed().as_millis()).unwrap_or(u64::MAX)
}
