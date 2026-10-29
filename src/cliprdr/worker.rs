//! Serializes the CLIPRDR request/response stream and OS clipboard events.

use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Instant;

use ironrdp_cliprdr::backend::{ClipboardMessage, ClipboardMessageProxy};
use ironrdp_cliprdr::loop_detector::{ClipboardSource, LoopDetector};
use ironrdp_cliprdr::pdu::{
    ClipboardFileAttributes, ClipboardFormat, ClipboardFormatId, ClipboardFormatName,
    FileContentsFlags, FileContentsRequest, FileContentsResponse, FileDescriptor,
    FormatDataResponse, LockDataId, OwnedFileContentsResponse,
};
use ironrdp_cliprdr_format::bitmap;
use ironrdp_core::IntoOwned as _;
use tracing::{debug, info, warn};

use super::data_control::find_mime_match;
use super::files::{self, GNOME_COPIED_FILES, URI_LIST};
use super::os::{OsClipboard, PNG, TEXT, Transfer};

pub const PASTE_TIMEOUT: Duration = Duration::from_secs(5);
const FILE_TIMEOUT: Duration = Duration::from_secs(10);
const FILE_CHUNK: u64 = 1024 * 1024;
pub const MAX_TRANSFER_BYTES: usize = 100 * 1024 * 1024;
const MAX_WAITERS: usize = 16;
const EMPTY_SELECTION_DEBOUNCE: Duration = Duration::from_millis(100);
static NEXT_STAGING_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub enum Command {
    AdvertiseLocal,
    BackendReady,
    LocalChanged,
    RemoteCopy(Vec<ClipboardFormat>, u64),
    RemoteData(Option<Vec<u8>>),
    RemoteFileList(Vec<FileDescriptor>, Option<u32>, u64),
    RemoteFileData(OwnedFileContentsResponse),
    Upload(FileContentsRequest),
    OfferFiles(Vec<PathBuf>),
    Lock(LockDataId),
    Unlock(LockDataId),
    RenderLocal(ClipboardFormatId),
    Paste(Transfer),
    Reset,
    Shutdown,
}

struct Pending {
    format: ClipboardFormatId,
    generation: u64,
    deadline: Instant,
    expired: bool,
    waiters: Vec<Transfer>,
}

struct Queued {
    deadline: Instant,
    transfer: Transfer,
}

struct Download {
    index: i32,
    destination: PathBuf,
    partial: PathBuf,
    output: std::fs::File,
    size: Option<u64>,
    offset: u64,
    stream_id: u32,
    clip_data_id: Option<u32>,
    deadline: Instant,
}

pub struct Worker<O, P> {
    os: O,
    proxy: P,
    ready: bool,
    advertise_when_ready: bool,
    remote: Vec<ClipboardFormat>,
    owns_remote: bool,
    generation: u64,
    pending: Option<Pending>,
    queue: VecDeque<Queued>,
    cache: HashMap<ClipboardFormatId, Vec<u8>>,
    staging_root: Option<PathBuf>,
    staging_dir: Option<PathBuf>,
    staged_files: Vec<PathBuf>,
    staged_fingerprint: files::FileListFingerprint,
    local_files: Vec<PathBuf>,
    locked_uploads: HashMap<u32, Vec<PathBuf>>,
    remote_file_list_format: Option<ClipboardFormatId>,
    remote_epoch: u64,
    next_stream_id: u32,
    downloads: VecDeque<Download>,
    active_download: Option<Download>,
    download_budget: u64,
    empty_selection_since: Option<Instant>,
    loops: LoopDetector,
    /// File list announced by the remote whose downloads are deferred until a
    /// local application actually pastes; `None` when nothing is pending.
    pending_remote_files: Option<(Vec<FileDescriptor>, Option<u32>)>,
    /// When the deferral of `pending_remote_files` began (for the auto-start).
    deferral_since: Option<Instant>,
    /// Whether the init-time (empty) format list was already sent; real
    /// content announces happen only on genuine local copies afterwards.
    init_advertised: bool,
}

impl<O: OsClipboard, P: ClipboardMessageProxy> Worker<O, P> {
    pub fn new(os: O, proxy: P, staging_root: Option<std::path::PathBuf>) -> Self {
        Self {
            advertise_when_ready: false,
            os,
            proxy,
            ready: false,
            remote: Vec::new(),
            owns_remote: false,
            generation: 0,
            pending: None,
            queue: VecDeque::new(),
            cache: HashMap::new(),
            staging_root,
            staging_dir: None,
            staged_files: Vec::new(),
            staged_fingerprint: Vec::new(),
            local_files: Vec::new(),
            locked_uploads: HashMap::new(),
            remote_file_list_format: None,
            remote_epoch: 0,
            next_stream_id: 1,
            downloads: VecDeque::new(),
            active_download: None,
            download_budget: files::MAX_FILE_BYTES,
            empty_selection_since: None,
            loops: LoopDetector::new(),
            pending_remote_files: None,
            deferral_since: None,
            init_advertised: false,
        }
    }

    pub fn run(mut self, receiver: &Receiver<Command>) {
        loop {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => self.expire(Instant::now()),
                Ok(command) => {
                    self.handle(command);
                    self.expire(Instant::now());
                }
            }
        }
        let _ = self.os.clear();
        self.clear_staging();
    }

    pub fn handle(&mut self, command: Command) {
        match command {
            Command::BackendReady => {
                self.ready = true;
                if self.advertise_when_ready && !self.owns_remote {
                    self.advertise_when_ready = false;
                    debug!("CLIPRDR became ready; sending deferred local clipboard offer");
                    self.advertise_local();
                }
            }
            Command::AdvertiseLocal => {
                // Part of the channel initialization sequence: IronRDP asks for
                // the local format list before marking the channel ready, and
                // the server's FormatListResponse is what completes init.
                // `SendInitiateCopy` is exempt from the pre-ready drop; only
                // file-copy offers need to wait for `BackendReady`.
                debug!("CLIPRDR requested local clipboard advertisement");
                if !self.owns_remote {
                    self.advertise_local();
                }
            }
            Command::LocalChanged => self.local_changed(Instant::now()),
            Command::RemoteCopy(formats, epoch) => {
                debug!(?formats, epoch, "Received remote clipboard format list");
                self.remote_epoch = epoch;
                self.remote_file_list_format = formats
                    .iter()
                    .find(|format| {
                        format
                            .name
                            .as_ref()
                            .is_some_and(|name| *name == ClipboardFormatName::FILE_LIST)
                    })
                    .map(|format| format.id());
                // A pure file-list re-announcement of content we have
                // already staged (rdpclip re-announces regularly — Windows
                // Explorer pairs FileGroupDescriptorW with "Preferred
                // DropEffect", so the list is two formats, not one) must not
                // tear the mirror down: clearing X11 ownership and the
                // staging directory mid-paste breaks readers, and the
                // owner churn makes rdpclip re-announce again (ping-pong of
                // format lists on the wire, during which pastes on the
                // remote resolve against a clipboard that keeps changing —
                // Explorer paste silently does nothing). The handler below
                // in `remote_file_list` short-circuits unchanged content
                // via the stored fingerprint, so any file-list-only
                // announcement is safe to keep: it either restages content
                // that actually changed or keeps the current mirror.
                if self.remote_file_list_format.is_some()
                    && !formats.iter().any(|format| {
                        matches!(
                            format.id(),
                            ClipboardFormatId::CF_UNICODETEXT
                                | ClipboardFormatId::CF_DIB
                                | ClipboardFormatId::CF_DIBV5
                        )
                    })
                {
                    self.remote = formats;
                    self.owns_remote = true;
                    self.request_remote_file_list();
                    return;
                }
                if self.owns_remote {
                    let _ = self.os.clear();
                }
                self.invalidate();
                let supported = self.remote_file_list_format.is_some()
                    || formats.iter().any(|format| {
                        matches!(
                            format.id(),
                            ClipboardFormatId::CF_UNICODETEXT
                                | ClipboardFormatId::CF_DIB
                                | ClipboardFormatId::CF_DIBV5
                        )
                    });
                if !supported {
                    debug!("No supported format in the remote clipboard");
                    self.owns_remote = false;
                    self.remote.clear();
                    return;
                }
                self.remote = formats;
                self.owns_remote = true;
                if self.remote_file_list_format.is_some() {
                    self.request_remote_file_list();
                } else {
                    self.offer_remote();
                }
            }
            Command::RemoteFileList(files, clip_data_id, epoch) => {
                debug!(
                    file_count = files.len(),
                    ?clip_data_id,
                    epoch,
                    "CLIPRDR delivered remote file list callback"
                );
                self.remote_file_list(files, clip_data_id, epoch);
            }
            Command::RemoteData(data) => {
                debug!(
                    bytes = data.as_ref().map_or(0, Vec::len),
                    success = data.is_some(),
                    "Received clipboard format data response"
                );
                self.remote_data(data)
            }
            Command::RemoteFileData(response) => {
                debug!(
                    stream_id = response.stream_id(),
                    failed = response.is_error(),
                    bytes = response.data().len(),
                    "Received clipboard file contents response"
                );
                self.remote_file_data(response)
            }
            Command::Upload(request) => {
                debug!(stream_id = request.stream_id, index = request.index, flags = ?request.flags, data_id = ?request.data_id, "Received clipboard file contents request");
                self.upload(request)
            }
            Command::Lock(id) => {
                self.locked_uploads.insert(id.0, self.local_files.clone());
            }
            Command::Unlock(id) => {
                self.locked_uploads.remove(&id.0);
            }
            Command::OfferFiles(paths) => self.offer_files(paths),
            Command::Paste(transfer) => self.paste(transfer),
            Command::RenderLocal(format) => {
                debug!(
                    ?format,
                    "Received clipboard format data request from remote"
                );
                self.render_local(format)
            }
            Command::Reset => {
                self.invalidate();
                self.pending = None;
                self.advertise_when_ready = false;
                self.remote.clear();
                if self.owns_remote {
                    let _ = self.os.clear();
                }
                self.owns_remote = false;
                self.local_files.clear();
                self.locked_uploads.clear();
                self.remote_file_list_format = None;
                self.pending_remote_files = None;
                self.deferral_since = None;
                self.loops.clear();
            }
            Command::Shutdown => {}
        }
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.queue.clear();
        self.cache.clear();
        self.local_files.clear();
        self.downloads.clear();
        self.active_download = None;
        self.pending_remote_files = None;
        self.deferral_since = None;
        self.staged_files.clear();
        self.staged_fingerprint.clear();
        self.clear_staging();
        if let Some(pending) = &mut self.pending {
            pending.waiters.clear();
        }
    }

    fn clear_staging(&mut self) {
        if let Some(directory) = self.staging_dir.take() {
            if let Err(error) = std::fs::remove_dir_all(&directory) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    debug!(%error, path = %directory.display(), "Could not remove clipboard staging directory");
                }
            }
        }
    }

    fn local_changed(&mut self, now: Instant) {
        if self.os.is_owner() {
            self.empty_selection_since = None;
            return;
        }
        let mimes = self.os.mime_types();
        if mimes.is_empty() {
            let since = self.empty_selection_since.get_or_insert(now);
            if now.saturating_duration_since(*since) < EMPTY_SELECTION_DEBOUNCE {
                debug!("Deferring an empty clipboard selection during owner handoff");
                return;
            }
        }
        self.empty_selection_since = None;
        // An empty local selection is not real clipboard content — it is the
        // X11 release our own `os.clear()` triggers when re-announcing the
        // remote clipboard (rdpclip re-announces the same content often). If a
        // remote mirror is live, keep it: re-acquire ownership, never tear the
        // mirror down (which aborts in-flight file downloads and sends the
        // remote a bogus empty format list).
        if self.owns_remote && mimes.is_empty() {
            self.offer_remote();
            return;
        }
        self.invalidate();
        self.revert_to_local();
    }

    /// Drop the remote formats and announce what the local clipboard holds.
    fn revert_to_local(&mut self) {
        self.owns_remote = false;
        self.remote.clear();
        if self.ready {
            self.advertise_local();
        }
    }
    fn advertise_local(&mut self) {
        let local_mimes = self.os.mime_types();
        debug!(?local_mimes, "Inspecting local clipboard formats for files");

        // Text/image formats: this is the answer to IronRDP's
        // initialization-time `on_request_format_list` too. `SendInitiateCopy`
        // is what completes the channel's initialization sequence
        // (Capabilities + Temporary Directory + Format List); skipping it
        // leaves the channel in Initialization state forever and `on_ready`
        // never fires. It must always go out, whatever the clipboard holds.
        //
        // BUT a FormatList carrying actual formats REPLACES the remote's
        // clipboard (rdpclip re-registers its content, destroying the shell's
        // pending file-copy object the user may still want to paste on the
        // remote). mstsc does not announce its clipboard at connection time —
        // it only reacts to user copies. Match that: send an EMPTY list when
        // fulfilling the init handshake, and announce real content only in
        // response to a genuine LocalChanged (a copy made locally while
        // connected). The first genuine LocalChanged after `init_advertised`
        // carries the full content.
        if !self.init_advertised {
            self.init_advertised = true;
            if !local_mimes.is_empty() {
                debug!(
                    ?local_mimes,
                    "Init handshake: announcing empty formats; real local content advertises on the first LocalChanged"
                );
                self.proxy
                    .send_clipboard_message(ClipboardMessage::SendInitiateCopy(Vec::new()));
                // Pre-connection local content is NOT sent proactively; the
                // next genuine LocalChanged announces it.
                return;
            }
        }
        let mut formats = Vec::new();
        if find_mime_match(TEXT, &local_mimes).is_some() {
            formats.push(ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT));
        }
        if find_mime_match(PNG, &local_mimes).is_some() {
            formats.extend([
                ClipboardFormat::new(ClipboardFormatId::CF_DIB),
                ClipboardFormat::new(ClipboardFormatId::CF_DIBV5),
            ]);
        }
        if formats.is_empty() {
            debug!("Advertising no supported local clipboard formats");
        }
        self.proxy
            .send_clipboard_message(ClipboardMessage::SendInitiateCopy(formats));

        // Then the file-copy offer, which DOES require a ready channel
        // (`initiate_file_copy` fails with "not in Ready state" otherwise), so
        // it is deferred when the channel is still initializing.
        if local_mimes
            .iter()
            .any(|mime| matches!(mime.as_str(), URI_LIST | GNOME_COPIED_FILES))
        {
            let uri_data = if local_mimes.iter().any(|mime| mime == GNOME_COPIED_FILES) {
                self.os.read(GNOME_COPIED_FILES)
            } else {
                self.os.read(URI_LIST)
            };
            let decoded = uri_data
                .and_then(|data| {
                    data.ok_or_else(|| "copied file list is no longer available".to_owned())
                })
                .and_then(|data| files::decode_uri_list(&data))
                .and_then(|paths| files::local_files(&paths));
            match decoded {
                Ok((descriptors, paths)) => {
                    self.local_files = paths;
                    if self.ready {
                        debug!(
                            files = descriptors.len(),
                            "Sending copied local files to RDP peer"
                        );
                        self.proxy
                            .send_clipboard_message(ClipboardMessage::SendInitiateFileCopy(
                                descriptors,
                            ));
                    } else {
                        self.advertise_when_ready = true;
                        debug!(
                            files = descriptors.len(),
                            "Deferring local file offer until CLIPRDR channel is ready"
                        );
                    }
                }
                Err(error) => {
                    self.local_files.clear();
                    debug!(%error, "Could not read the local copied file list")
                }
            }
        }
    }
    fn offer_files(&mut self, roots: Vec<PathBuf>) {
        debug!(roots = ?roots, "Offering dropped paths to CLIPRDR");
        match files::local_files(&roots) {
            Ok((descriptors, paths)) if !descriptors.is_empty() => {
                debug!(
                    files = descriptors.len(),
                    "Sending dropped file list to RDP peer"
                );
                self.local_files = paths;
                self.proxy
                    .send_clipboard_message(ClipboardMessage::SendInitiateFileCopy(descriptors));
            }
            Ok(_) => debug!("Dropped file list is empty"),
            Err(error) => warn!(%error, "Could not offer dropped clipboard files"),
        }
    }

    fn paste(&mut self, transfer: Transfer) {
        debug!(
            mime = %transfer.mime_type(),
            generation = transfer.generation(),
            current = self.generation,
            owns_remote = self.owns_remote,
            "OS paste request received"
        );
        if !self.owns_remote || transfer.generation() != self.generation {
            return;
        }
        if matches!(transfer.mime_type(), URI_LIST | GNOME_COPIED_FILES) {
            if self.active_download.is_none()
                && self.downloads.is_empty()
                && self.staged_files.is_empty()
                && self.pending_remote_files.is_some()
            {
                // First local paste of the mirrored file clipboard:
                // download the deferred list now, ask this paste to retry.
                info!(
                    mime = %transfer.mime_type(),
                    "Local paste requested the staged URIs; starting deferred downloads"
                );
                self.begin_pending_downloads();
                // Data is not ready yet; close this paste. The re-announce at
                // staging completion lets the next paste succeed. (GTK
                // applications typically re-paste on user retry.)
                transfer.finish(None);
                return;
            }
            if self.active_download.is_none()
                && self.downloads.is_empty()
                && !self.staged_files.is_empty()
            {
                let mut data = Vec::new();
                if transfer.mime_type() == GNOME_COPIED_FILES {
                    data.extend_from_slice(b"copy\n");
                }
                // A trailing delimiter makes GTK's strsplit produce an
                // empty URI entry; nautilus turns that into a GFile whose
                // basename is NULL and crashes in g_utf8_validate during
                // the copy job. Emit entries separated by newlines with
                // none after the last.
                let uris = files::encode_uri_list(&self.staged_files);
                let uris = &uris[..uris.len().saturating_sub(1)];
                data.extend_from_slice(uris);
                transfer.finish(Some(data));
            } else {
                transfer.finish(None);
            }
            return;
        }
        let has = |id| self.remote.iter().any(|f| f.id() == id);
        let format = if transfer.mime_type().split(';').next() == Some("text/plain")
            && has(ClipboardFormatId::CF_UNICODETEXT)
        {
            ClipboardFormatId::CF_UNICODETEXT
        } else if transfer.mime_type() == PNG && has(ClipboardFormatId::CF_DIBV5) {
            ClipboardFormatId::CF_DIBV5
        } else if transfer.mime_type() == PNG && has(ClipboardFormatId::CF_DIB) {
            ClipboardFormatId::CF_DIB
        } else {
            return;
        };
        if let Some(data) = self.cache.get(&format) {
            transfer.finish(Some(data.clone()));
            return;
        }
        if let Some(pending) = &mut self.pending {
            if pending.expired || MAX_WAITERS <= pending.waiters.len() + self.queue.len() {
                return;
            }
            if pending.generation == self.generation && pending.format == format {
                pending.waiters.push(transfer);
            } else {
                self.queue.push_back(Queued {
                    deadline: Instant::now() + PASTE_TIMEOUT,
                    transfer,
                });
            }
        } else {
            self.start(format, transfer);
        }
    }

    fn start(&mut self, format: ClipboardFormatId, transfer: Transfer) {
        self.pending = Some(Pending {
            format,
            generation: self.generation,
            deadline: Instant::now() + PASTE_TIMEOUT,
            expired: false,
            waiters: vec![transfer],
        });
        self.proxy
            .send_clipboard_message(ClipboardMessage::SendInitiatePaste(format));
    }

    /// Expire OS waiters, but keep the outstanding wire request until its reply arrives.
    /// [MS-RDPECLIP 2.2.5.2] has no response identifier, so starting another request
    /// after a timeout would misinterpret the late reply as the new clipboard content.
    ///
    /// [MS-RDPECLIP 2.2.5.2]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeclip/28c193b8-4cec-413e-a07b-9235e5e15f6b
    pub fn expire(&mut self, now: Instant) {
        if self
            .empty_selection_since
            .is_some_and(|since| now.saturating_duration_since(since) >= EMPTY_SELECTION_DEBOUNCE)
        {
            self.local_changed(now);
        }
        let file_list_expired = self.pending.as_ref().is_some_and(|pending| {
            pending.deadline <= now
                && pending.generation == self.generation
                && Some(pending.format) == self.remote_file_list_format
        });
        // Deferred downloads auto-begin after a short grace period: the remote
        // keeps its file stream data under a lock that expires after 60s of
        // inactivity, so waiting for a local paste forever risks serving the
        // URIs from an expired stream. 10s covers a human pasting at normal
        // speed and stays far inside the lock window.
        if self.pending_remote_files.is_some()
            && self.deferral_since
                .is_some_and(|since| now.saturating_duration_since(since) >= FILE_TIMEOUT)
        {
            info!("Starting deferred remote file downloads after the paste grace period");
            self.begin_pending_downloads();
        }
        if let Some(pending) = &mut self.pending {
            if pending.deadline <= now {
                pending.expired = true;
                pending.waiters.clear();
            }
        }
        // A timed-out file-list request must retire its `pending` slot: it is
        // the gate for `request_remote_file_list()`, and no file-list response
        // will ever otherwise arrive to clear it. Keeping it wedged the whole
        // file clipboard across every later re-announcement (rdpclip
        // re-announces the format list, the shortcut calls
        // `request_remote_file_list()`, it refuses while `pending` is set).
        // The epoch check in `remote_file_list()` already identifies a truly
        // late reply, so an unused retired slot cannot misattribute one.
        if file_list_expired {
            self.pending = None;
        }
        self.queue.retain(|queued| now < queued.deadline);
        if self
            .active_download
            .as_ref()
            .is_some_and(|download| download.deadline <= now)
        {
            warn!("Remote clipboard file download timed out");
            self.fail_downloads();
        }
        if file_list_expired {
            self.remote_file_list_format = None;
            self.offer_remote();
        }
    }

    fn remote_data(&mut self, data: Option<Vec<u8>>) {
        let Some(pending) = self.pending.take() else {
            debug!("Discarding an unsolicited clipboard response");
            return;
        };
        if Some(pending.format) == self.remote_file_list_format {
            if pending.generation == self.generation && self.owns_remote {
                debug!("Could not retrieve the remote file list");
                self.remote_file_list_format = None;
                self.offer_remote();
            }
            self.request_remote_file_list();
            return;
        }
        if !pending.expired
            && Instant::now() < pending.deadline
            && pending.generation == self.generation
            && self.owns_remote
        {
            let decoded = data
                .filter(|data| data.len() <= MAX_TRANSFER_BYTES)
                .and_then(|data| {
                    let result = match pending.format {
                        ClipboardFormatId::CF_UNICODETEXT => {
                            FormatDataResponse::new_data(data.as_slice())
                                .to_unicode_string()
                                .map(String::into_bytes)
                                .map_err(|error| error.to_string())
                        }
                        ClipboardFormatId::CF_DIB => {
                            bitmap::dib_to_png(&data).map_err(|error| error.to_string())
                        }
                        ClipboardFormatId::CF_DIBV5 => {
                            bitmap::dibv5_to_png(&data).map_err(|error| error.to_string())
                        }
                        _ => return None,
                    };
                    match result {
                        Ok(data) => Some(data),
                        Err(error) => {
                            warn!(%error, "Could not decode the remote clipboard");
                            None
                        }
                    }
                });
            if let Some(data) = &decoded {
                self.loops.record_content(
                    data,
                    ClipboardSource::Remote,
                    crate::cliprdr::native_now_ms(),
                );
                self.cache.insert(pending.format, data.clone());
            }
            for transfer in pending.waiters {
                transfer.finish(decoded.clone());
            }
        }
        while self.pending.is_none() {
            let Some(queued) = self.queue.pop_front() else {
                break;
            };
            if Instant::now() < queued.deadline {
                self.paste(queued.transfer);
            }
        }
        self.request_remote_file_list();
    }

    fn render_local(&mut self, format: ClipboardFormatId) {
        let result = (|| -> Result<Vec<u8>, String> {
            if self.owns_remote {
                return Err("local clipboard selection was replaced".into());
            }
            let mime = match format {
                ClipboardFormatId::CF_UNICODETEXT => TEXT,
                ClipboardFormatId::CF_DIB | ClipboardFormatId::CF_DIBV5 => PNG,
                _ => return Err("unsupported clipboard format".into()),
            };
            let data = self
                .os
                .read(mime)?
                .ok_or("clipboard format is no longer available")?;
            if MAX_TRANSFER_BYTES < data.len() {
                return Err("clipboard data exceeds the size limit".into());
            }
            // Native APIs suppress our own selection events. This also stops a
            // clipboard manager that republishes recently received content.
            if self.loops.would_cause_content_loop(
                &data,
                ClipboardSource::Local,
                crate::cliprdr::native_now_ms(),
            ) {
                return Err("clipboard content would echo a recent remote paste".into());
            }
            self.loops.record_content(
                &data,
                ClipboardSource::Local,
                crate::cliprdr::native_now_ms(),
            );
            match format {
                ClipboardFormatId::CF_UNICODETEXT => {
                    let text = core::str::from_utf8(&data).map_err(|error| error.to_string())?;
                    let units = text.encode_utf16().count().saturating_add(1);
                    if MAX_TRANSFER_BYTES / 2 < units {
                        return Err("encoded clipboard text exceeds the size limit".into());
                    }
                    Ok(FormatDataResponse::new_unicode_string(text)
                        .into_data()
                        .into_owned())
                }
                ClipboardFormatId::CF_DIB => {
                    bitmap::png_to_cf_dib(&data).map_err(|error| error.to_string())
                }
                ClipboardFormatId::CF_DIBV5 => {
                    bitmap::png_to_cf_dibv5(&data).map_err(|error| error.to_string())
                }
                _ => Err("unsupported clipboard format".into()),
            }
        })();
        let response = match result {
            Ok(data) => FormatDataResponse::new_data(data).into_owned(),
            Err(error) => {
                debug!(%error, "Could not render the local clipboard");
                FormatDataResponse::new_error()
            }
        };
        self.proxy
            .send_clipboard_message(ClipboardMessage::SendFormatData(response));
    }

    fn request_remote_file_list(&mut self) {
        if !self.owns_remote || self.pending.is_some() {
            return;
        }
        let Some(format) = self.remote_file_list_format else {
            return;
        };
        self.pending = Some(Pending {
            format,
            deadline: Instant::now() + PASTE_TIMEOUT,
            generation: self.generation,
            expired: false,
            waiters: Vec::new(),
        });
        tracing::debug!(format = ?format, "Requesting remote file list format");
        self.proxy
            .send_clipboard_message(ClipboardMessage::SendInitiatePaste(format));
    }

    fn offer_remote(&mut self) {
        let mut mimes = Vec::new();
        if self
            .remote
            .iter()
            .any(|format| format.id() == ClipboardFormatId::CF_UNICODETEXT)
        {
            mimes.extend([TEXT.to_owned(), "text/plain".to_owned()]);
        }
        if self.remote.iter().any(|format| {
            matches!(
                format.id(),
                ClipboardFormatId::CF_DIB | ClipboardFormatId::CF_DIBV5
            )
        }) {
            mimes.push(PNG.to_owned());
        }
        // Files may still be deferred (`pending_remote_files`); the X11 offer is
        // delayed-rendered anyway — the URIs are served from `staged_files` only
        // after the deferred downloads finish, so advertising them now claims
        // ownership without needing the data yet.
        if !self.staged_files.is_empty() || self.pending_remote_files.is_some() {
            mimes.extend([URI_LIST.to_owned(), GNOME_COPIED_FILES.to_owned()]);
        }
        if mimes.is_empty() {
            return;
        }
        if let Err(error) = self.os.offer(&mimes, self.generation) {
            warn!(%error, "Could not advertise the remote clipboard");
            let _ = self.os.clear();
            self.revert_to_local();
        }
    }

    fn remote_file_list(
        &mut self,
        files: Vec<FileDescriptor>,
        clip_data_id: Option<u32>,
        epoch: u64,
    ) {
        let matches_request = self
            .pending
            .as_ref()
            .is_some_and(|pending| Some(pending.format) == self.remote_file_list_format);
        if matches_request {
            self.pending = None;
        }
        if epoch != self.remote_epoch || !matches_request || !self.owns_remote {
            debug!("Discarding a stale remote clipboard file list");
            self.request_remote_file_list();
            return;
        }
        self.remote_file_list_format = None;
        debug!(
            files = files.len(),
            ?clip_data_id,
            "Received remote clipboard file list"
        );
        // rdpclip re-announces the same clipboard repeatedly. Re-staging
        // identical content recreates the staging directory and re-takes
        // X11 ownership while a paste may be reading that data mid-flight
        // (observed as a Nautilus crash: a GFile basename turning NULL
        // between copy steps). Identical descriptors only update the lock
        // snapshot; the staged files stay where they are.
        if self.downloads.is_empty()
            && self.active_download.is_none()
            && !self.staged_files.is_empty()
            && files::same_fingerprint(&self.staged_fingerprint, &files)
        {
            debug!("Remote file list is unchanged; keeping the staged files");
            if !self.os.is_owner() {
                self.offer_remote();
            }
            return;
        }
        // Defer downloading: hold the announced list until a local
        // application actually pastes the file URIs. An eager
        // FileContentsRequest per announcement (rdpclip re-announces the
        // same list often) forces the remote's rdpclip to open/close a
        // file stream every few seconds, which interacts badly with its
        // single-stream serving state and wedges the LOCAL file paste
        // path on the remote itself (observed: Explorer's Ctrl+V doing
        // nothing while rstsc is connected). Stage on real demand only.
        self.pending_remote_files = Some((files, clip_data_id));
        self.deferral_since = Some(Instant::now());
        self.offer_remote();
    }

    /// Kicks deferred remote file downloads; called when something pastes the
    /// staged URIs or when the mirror needs real files.
    fn begin_pending_downloads(&mut self) {
        let Some((files, clip_data_id)) = self.pending_remote_files.take() else {
            self.deferral_since = None;
            return;
        };
        self.deferral_since = None;
        match self.prepare_downloads(&files, clip_data_id) {
            Ok(()) => self.start_next_download(),
            Err(error) => {
                warn!(%error, "Could not stage remote clipboard files");
                self.staged_files.clear();
                self.clear_staging();
                self.offer_remote();
            }
        }
    }

    fn prepare_downloads(
        &mut self,
        files: &[FileDescriptor],
        clip_data_id: Option<u32>,
    ) -> Result<(), String> {
        if files.len() > files::MAX_FILES {
            return Err("remote clipboard file list is too large".into());
        }
        let base = match &self.staging_root {
            Some(root) => root.join(".clipboard-staging"),
            None => std::env::temp_dir().join("rstsc-clipboard"),
        };
        std::fs::create_dir_all(&base).map_err(|error| error.to_string())?;
        let id = NEXT_STAGING_ID.fetch_add(1, Ordering::Relaxed);
        let directory = base.join(format!("{}-{id}", std::process::id()));
        std::fs::create_dir(&directory).map_err(|error| error.to_string())?;

        let result = (|| {
            let mut downloads = VecDeque::new();
            let mut roots = Vec::new();
            let mut destinations = HashSet::new();
            let mut known_bytes = 0_u64;
            for (index, descriptor) in files.iter().enumerate() {
                let destination = files::staged_path(&directory, descriptor)?;
                if !destinations.insert(destination.clone()) {
                    return Err("duplicate path in remote clipboard file list".to_owned());
                }
                if descriptor
                    .relative_path
                    .as_deref()
                    .is_none_or(str::is_empty)
                {
                    roots.push(destination.clone());
                }
                let directory_entry = descriptor.attributes.is_some_and(|attributes| {
                    attributes.contains(ClipboardFileAttributes::DIRECTORY)
                });
                if directory_entry {
                    std::fs::create_dir_all(&destination).map_err(|error| error.to_string())?;
                    continue;
                }
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                known_bytes = known_bytes
                    .checked_add(descriptor.file_size.unwrap_or(0))
                    .ok_or("remote clipboard size overflow")?;
                if known_bytes > files::MAX_FILE_BYTES {
                    return Err("remote clipboard files exceed the 2 GiB limit".to_owned());
                }
                let mut partial = destination.clone().into_os_string();
                partial.push(".rdp-part");
                let partial = PathBuf::from(partial);
                let output = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&partial)
                    .map_err(|error| error.to_string())?;
                downloads.push_back(Download {
                    index: i32::try_from(index).map_err(|_| "too many remote files")?,
                    destination,
                    partial,
                    output,
                    size: descriptor.file_size,
                    offset: 0,
                    stream_id: 0,
                    clip_data_id,
                    deadline: Instant::now() + FILE_TIMEOUT,
                });
            }
            self.downloads = downloads;
            self.staged_files = roots;
            self.download_budget = files::MAX_FILE_BYTES - known_bytes;
            Ok(())
        })();
        if result.is_ok() {
            self.staging_dir = Some(directory);
            self.staged_fingerprint = files::fingerprint(files);
        } else {
            let _ = std::fs::remove_dir_all(directory);
        }
        result
    }

    fn download_request(download: &Download) -> Result<Option<FileContentsRequest>, String> {
        let (flags, requested_size) = match download.size {
            None => (FileContentsFlags::SIZE, 8),
            Some(size) if download.offset < size => (
                FileContentsFlags::RANGE,
                u32::try_from((size - download.offset).min(FILE_CHUNK)).unwrap(),
            ),
            Some(size) if download.offset == size => return Ok(None),
            Some(_) => return Err("remote file download exceeded its declared size".into()),
        };
        Ok(Some(FileContentsRequest {
            stream_id: download.stream_id,
            index: download.index,
            flags,
            position: download.offset,
            requested_size,
            data_id: download.clip_data_id,
        }))
    }

    fn continue_download(&mut self, mut download: Download) {
        match Self::download_request(&download) {
            Ok(Some(request)) => {
                debug!(stream_id = request.stream_id, index = request.index, flags = ?request.flags, offset = request.position, "Requesting remote clipboard file bytes");
                self.proxy
                    .send_clipboard_message(ClipboardMessage::SendFileContentsRequest(request));
                // Each in-flight chunk gets a fresh timeout; the struct's
                // deadline field starts at `Instant::now()` and would
                // otherwise expire immediately in `expire()`.
                let mut download = download;
                download.deadline = Instant::now() + FILE_TIMEOUT;
                self.active_download = Some(download);
            }
            Ok(None) => {
                drop(download.output);
                if let Err(error) = std::fs::rename(&download.partial, &download.destination) {
                    warn!(%error, "Could not finish a staged clipboard file");
                    self.fail_downloads();
                    return;
                }
                self.start_next_download();
            }
            Err(error) => {
                warn!(%error, "Could not construct a clipboard file request");
                self.fail_downloads();
            }
        }
    }

    fn start_next_download(&mut self) {
        if self.active_download.is_some() {
            return;
        }
        let id = self.next_stream_id;
        self.next_stream_id = self.next_stream_id.wrapping_add(1);
        if let Some(mut download) = self.downloads.pop_front() {
            download.stream_id = id;
            self.continue_download(download);
        } else {
            self.offer_remote();
        }
    }

    fn remote_file_data(&mut self, response: OwnedFileContentsResponse) {
        if self
            .active_download
            .as_ref()
            .map(|download| download.stream_id)
            != Some(response.stream_id())
        {
            debug!("Discarding a stale clipboard file contents response");
            return;
        }
        let Some(mut download) = self.active_download.take() else {
            return;
        };
        let result = (|| {
            if response.is_error() {
                return Err("remote file contents request failed".to_owned());
            }
            if download.size.is_none() {
                let size = response.data_as_size().map_err(|error| error.to_string())?;
                self.download_budget = self
                    .download_budget
                    .checked_sub(size)
                    .ok_or("remote clipboard files exceed the 2 GiB limit")?;
                download.size = Some(size);
            } else {
                let expected = (download.size.unwrap() - download.offset).min(FILE_CHUNK);
                if response.data().is_empty() || response.data().len() as u64 > expected {
                    return Err("invalid remote file contents range".to_owned());
                }
                download
                    .output
                    .write_all(&response.data())
                    .map_err(|error| error.to_string())?;
                download.offset += response.data().len() as u64;
            }
            Ok(())
        })();
        if let Err(error) = result {
            warn!(%error, "Remote clipboard file download failed");
            self.fail_downloads();
            return;
        }
        self.continue_download(download);
    }

    fn fail_downloads(&mut self) {
        warn!(
            files_remaining = self.downloads.len() + usize::from(self.active_download.is_some()),
            "Aborting remote clipboard file download"
        );
        self.active_download = None;
        self.downloads.clear();
        self.staged_files.clear();
        self.remote_file_list_format = None;
        self.clear_staging();
        self.offer_remote();
    }

    fn upload(&mut self, request: FileContentsRequest) {
        debug!(stream_id = request.stream_id, index = request.index, flags = ?request.flags, position = request.position, requested_size = request.requested_size, data_id = ?request.data_id, "Serving local file request");
        let response = (|| {
            let paths = match request.data_id {
                Some(id) => self
                    .locked_uploads
                    .get(&id)
                    .ok_or("unknown locked clipboard file snapshot"),
                None => Ok(&self.local_files),
            }?;
            let index =
                usize::try_from(request.index).map_err(|_| "negative clipboard file index")?;
            let path = paths
                .get(index)
                .ok_or("clipboard file index is out of bounds")?;
            let metadata = std::fs::metadata(path).map_err(|error| error.to_string())?;
            if !metadata.is_file() {
                return Err("clipboard file contents requested for a directory".to_owned());
            }
            request
                .flags
                .validate()
                .map_err(|error| error.to_string())?;
            if request.flags.contains(FileContentsFlags::SIZE) {
                return Ok(FileContentsResponse::new_size_response(
                    request.stream_id,
                    metadata.len(),
                )
                .into_owned());
            }
            if !request.flags.contains(FileContentsFlags::RANGE) || request.requested_size == 0 {
                return Err("invalid clipboard file range request".to_owned());
            }
            let data =
                files::read_range(path, request.position, request.requested_size, FILE_CHUNK)?;
            Ok(FileContentsResponse::new_data_response(request.stream_id, data).into_owned())
        })()
        .unwrap_or_else(|error| {
            debug!(%error, "Could not serve a local clipboard file");
            FileContentsResponse::new_error(request.stream_id).into_owned()
        });
        debug!(
            index = request.index,
            error = response.is_error(),
            bytes = response.data().len(),
            "Serving local clipboard file contents"
        );
        self.proxy
            .send_clipboard_message(ClipboardMessage::SendFileContentsResponse(response));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Default)]
    struct Recorder(Arc<Mutex<Vec<ClipboardMessage>>>);

    impl ClipboardMessageProxy for Recorder {
        fn send_clipboard_message(&self, message: ClipboardMessage) {
            self.0.lock().unwrap().push(message);
        }
    }

    struct FakeClipboard;

    impl OsClipboard for FakeClipboard {
        fn is_owner(&self) -> bool {
            false
        }
        fn mime_types(&self) -> Vec<String> {
            Vec::new()
        }
        fn read(&mut self, _: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(None)
        }
        fn offer(&mut self, _: &[String], _: u64) -> Result<(), String> {
            Ok(())
        }
        fn clear(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    #[test]
    fn init_format_list_is_sent_without_waiting_for_ready() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut worker = Worker::new(FakeClipboard, Recorder(Arc::clone(&messages)), None);
        worker.handle(Command::AdvertiseLocal);

        // `SendInitiateCopy` is exempt from IronRDP's pre-ready drop; the
        // server's FormatListResponse for it is what completes initialization.
        assert!(
            matches!(&messages.lock().unwrap()[..], [ClipboardMessage::SendInitiateCopy(formats)] if formats.is_empty())
        );
        assert!(!worker.advertise_when_ready);
    }
    #[test]
    fn file_offer_waits_until_backend_ready() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct FileClipboard {
            mimes: Mutex<Vec<String>>,
            uri_list: Vec<u8>,
        }
        impl OsClipboard for FileClipboard {
            fn is_owner(&self) -> bool {
                false
            }
            fn mime_types(&self) -> Vec<String> {
                self.mimes.lock().unwrap().clone()
            }
            fn read(&mut self, mime: &str) -> Result<Option<Vec<u8>>, String> {
                Ok(matches!(mime, URI_LIST | GNOME_COPIED_FILES).then(|| self.uri_list.clone()))
            }
            fn offer(&mut self, _: &[String], _: u64) -> Result<(), String> {
                Ok(())
            }
            fn clear(&mut self) -> Result<(), String> {
                Ok(())
            }
        }

        let root = std::env::temp_dir().join(format!("rstsc-defer-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("upload.txt");
        std::fs::write(&path, b"uploaded").unwrap();
        let clipboard = FileClipboard {
            mimes: Mutex::new(vec![URI_LIST.to_owned()]),
            uri_list: files::encode_uri_list(std::slice::from_ref(&path)),
        };
        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut worker = Worker::new(clipboard, Recorder(Arc::clone(&messages)), None);

        worker.handle(Command::AdvertiseLocal);
        // Init handshake announces an EMPTY format list (replacing nothing on
        // the remote); pre-connection local files are withheld until a
        // genuine local change or BackendReady. The worker must NOT advertise
        // content at init.
        assert!(
            matches!(&messages.lock().unwrap()[..], [ClipboardMessage::SendInitiateCopy(formats)] if formats.is_empty()),
            "init format list must be empty and complete the initialization"
        );
        assert!(!worker.advertise_when_ready);

        // The channel becomes ready while the files sit on the local OS
        // clipboard untouched: on_ready sees nothing to advertise (no
        // LocalChanged since connect). The files flow on the next genuine
        // local event. Simulate one: a LocalChanged now that the channel is
        // READY must produce the full announce (format list + file offer).
        worker.handle(Command::BackendReady);
        assert!(!worker.advertise_when_ready);
        worker.handle(Command::LocalChanged);
        let messages = messages.lock().unwrap();
        assert!(matches!(
            &messages[..],
            [
                ClipboardMessage::SendInitiateCopy(_),
                ClipboardMessage::SendInitiateCopy(_),
                ClipboardMessage::SendInitiateFileCopy(files)
            ] if files.len() == 1 && files[0].name == "upload.txt"
        ));
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn empty_selection_handoff_preserves_new_file_clipboard() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct ChangingClipboard {
            owner: AtomicBool,
            mimes: Mutex<Vec<String>>,
            uri_list: Vec<u8>,
        }
        impl OsClipboard for ChangingClipboard {
            fn is_owner(&self) -> bool {
                self.owner.load(Ordering::SeqCst)
            }
            fn mime_types(&self) -> Vec<String> {
                self.mimes.lock().unwrap().clone()
            }
            fn read(&mut self, mime: &str) -> Result<Option<Vec<u8>>, String> {
                Ok(matches!(mime, URI_LIST | GNOME_COPIED_FILES).then(|| self.uri_list.clone()))
            }
            fn offer(&mut self, _: &[String], _: u64) -> Result<(), String> {
                Ok(())
            }
            fn clear(&mut self) -> Result<(), String> {
                Ok(())
            }
        }

        let root =
            std::env::temp_dir().join(format!("rstsc-handoff-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("upload.txt");
        std::fs::write(&path, b"uploaded").unwrap();
        let clipboard = ChangingClipboard {
            owner: AtomicBool::new(false),
            mimes: Mutex::new(Vec::new()),
            uri_list: files::encode_uri_list(std::slice::from_ref(&path)),
        };
        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut worker = Worker::new(clipboard, Recorder(Arc::clone(&messages)), None);
        worker.ready = true;

        worker.handle(Command::LocalChanged);
        // First change with an empty local selection: wrung through the
        // debounce (nothing announced yet).
        assert!(
            messages.lock().unwrap().is_empty(),
            "empty selection during handoff should be deferred"
        );
        // Debounce elapses: the init handshake gets satisfied ONCE with an
        // empty format list (announcing nothing on the remote).
        worker.expire(Instant::now() + EMPTY_SELECTION_DEBOUNCE);
        assert!(
            matches!(&messages.lock().unwrap()[..], [ClipboardMessage::SendInitiateCopy(formats)] if formats.is_empty()),
            "debounced empty selection fulfills the init handshake with an empty list"
        );

        // A genuine local file copy arrives: full announce.
        worker.os.mimes.lock().unwrap().push(URI_LIST.to_owned());
        worker.handle(Command::LocalChanged);

        let messages = messages.lock().unwrap();
        assert!(matches!(
            &messages[messages.len() - 1],
            ClipboardMessage::SendInitiateFileCopy(files)
            if files.len() == 1 && files[0].name == "upload.txt"
        ));
        assert!(matches!(
            &messages[messages.len() - 2],
            ClipboardMessage::SendInitiateCopy(_)
        ));
        drop(messages);
        assert_eq!(worker.local_files, vec![path]);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn dropped_files_are_advertised_to_remote() {
        let root = std::env::temp_dir().join(format!("rstsc-drop-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("upload.txt");
        std::fs::write(&path, b"uploaded").unwrap();

        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut worker = Worker::new(FakeClipboard, Recorder(Arc::clone(&messages)), None);
        worker.handle(Command::OfferFiles(vec![path]));

        let messages = messages.lock().unwrap();
        assert!(
            matches!(&messages[..], [ClipboardMessage::SendInitiateFileCopy(files)] if files.len() == 1 && files[0].name == "upload.txt")
        );
        assert_eq!(worker.local_files, vec![root.join("upload.txt")]);

        drop(messages);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn copied_file_list_serves_size_and_range_requests() {
        let root =
            std::env::temp_dir().join(format!("rstsc-upload-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("upload.txt");
        std::fs::write(&path, b"uploaded").unwrap();

        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut worker = Worker::new(FakeClipboard, Recorder(Arc::clone(&messages)), None);
        worker.local_files = vec![path];
        worker.locked_uploads.insert(42, worker.local_files.clone());
        worker.upload(FileContentsRequest {
            stream_id: 7,
            index: 0,
            flags: FileContentsFlags::SIZE,
            position: 0,
            requested_size: 8,
            data_id: Some(42),
        });
        worker.upload(FileContentsRequest {
            stream_id: 8,
            index: 0,
            flags: FileContentsFlags::RANGE,
            position: 2,
            requested_size: 3,
            data_id: Some(42),
        });

        let messages = messages.lock().unwrap();
        assert!(matches!(&messages[..], [
            ClipboardMessage::SendFileContentsResponse(size),
            ClipboardMessage::SendFileContentsResponse(range)
        ] if !size.is_error() && size.stream_id() == 7 && size.data_as_size().unwrap() == 8
            && !range.is_error() && range.stream_id() == 8 && range.data() == b"loa"));
        drop(messages);
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn remote_format_list_advertises_only_supported_file_and_text_types() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut worker = Worker::new(FakeClipboard, Recorder(Arc::clone(&messages)), None);
        let file_list = ClipboardFormat::new(ClipboardFormatId::new(0xC0FE))
            .with_name(ClipboardFormatName::FILE_LIST);
        let file_list_id = file_list.id();
        worker.handle(Command::RemoteCopy(vec![file_list], 1));

        let messages = messages.lock().unwrap();
        assert!(
            matches!(&messages[..], [ClipboardMessage::SendInitiatePaste(format)]
                if *format == file_list_id)
        );
    }
    #[test]
    fn timed_out_file_list_request_arms_again_on_reannouncement() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut worker = Worker::new(FakeClipboard, Recorder(Arc::clone(&messages)), None);
        let format = ClipboardFormat::new(ClipboardFormatId::new(0xC0DE))
            .with_name(ClipboardFormatName::FILE_LIST);

        // First announcement arms a file-list request that is never answered.
        worker.handle(Command::RemoteCopy(vec![format.clone()], 1));
        assert!(worker.pending.is_some());
        // The paste request times out (rdpclip busy).
        worker.expire(Instant::now() + PASTE_TIMEOUT);
        assert!(worker.remote_file_list_format.is_none());

        // rdpclip re-announces the same clipboard: a fresh request must be sent
        // instead of being blocked by the retired-but-still-set `pending` slot.
        worker.handle(Command::RemoteCopy(vec![format], 2));
        let send_pastes = messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| matches!(m, ClipboardMessage::SendInitiatePaste(_)))
            .count();
        assert_eq!(
            send_pastes, 2,
            "second announcement must arm another file-list paste request"
        );
    }
    #[test]
    fn identical_remote_file_list_reannouncement_keeps_staging() {
        let root =
            std::env::temp_dir().join(format!("rstsc-restage-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("download.bin");
        std::fs::write(&path, vec![0_u8; 4096]).unwrap();

        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut worker = Worker::new(
            FakeClipboard,
            Recorder(Arc::clone(&messages)),
            Some(root.clone()),
        );
        let format = ClipboardFormat::new(ClipboardFormatId::new(0xC0DE))
            .with_name(ClipboardFormatName::FILE_LIST);
        worker.ready = true;
        worker.handle(Command::RemoteCopy(vec![format.clone()], 1));
        worker.handle(Command::RemoteFileList(
            vec![FileDescriptor::new("download.bin").with_file_size(4096)],
            Some(1),
            1,
        ));
        // The list is held deferred now; a local paste of the URIs starts the
        // downloads.
        worker.begin_pending_downloads();
        // Download completes.
        worker.handle(Command::RemoteFileData(
            FileContentsResponse::new_data_response(1, vec![0_u8; 4096]).into_owned(),
        ));
        let staged_dir = worker.staging_dir.clone().expect("staged after download");
        assert_eq!(
            worker.staged_fingerprint,
            files::fingerprint(&[FileDescriptor::new("download.bin").with_file_size(4096)])
        );

        // rdpclip re-announces the identical list: the staged files and the
        // staging directory must survive untouched.
        worker.handle(Command::RemoteCopy(vec![format], 2));
        worker.handle(Command::RemoteFileList(
            vec![FileDescriptor::new("download.bin").with_file_size(4096)],
            Some(2),
            2,
        ));
        assert_eq!(worker.staging_dir.as_ref(), Some(&staged_dir));
        assert!(staged_dir.exists());
        let _ = std::fs::remove_dir_all(root);
    }
    #[test]
    fn remote_file_list_is_deferred_until_local_paste() {
        let messages = Arc::new(Mutex::new(Vec::new()));
        let mut worker = Worker::new(FakeClipboard, Recorder(Arc::clone(&messages)), None);
        let format = ClipboardFormat::new(ClipboardFormatId::new(0xC0DE))
            .with_name(ClipboardFormatName::FILE_LIST);
        worker.ready = true;

        // Remote announces a file list: no FileContentsRequest may be sent
        // eagerly (it wedges rdpclip's stream state and breaks remote→self
        // pastes); the list is held.
        worker.handle(Command::RemoteCopy(vec![format], 1));
        worker.handle(Command::RemoteFileList(
            vec![FileDescriptor::new("deferred.bin").with_file_size(4)],
            Some(1),
            1,
        ));
        assert!(
            worker.pending_remote_files.is_some(),
            "fresh remote file list must be held for deferred staging"
        );
        assert!(worker.staging_dir.is_none(), "nothing backed yet");
        let wire_requests_early = messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| matches!(m, ClipboardMessage::SendFileContentsRequest(_)))
            .count();
        assert_eq!(
            wire_requests_early, 0,
            "no file contents request may go out before a local paste"
        );

        // A local paste of the file URIs starts the downloads.
        worker.expire(Instant::now() + FILE_TIMEOUT);
        let wire_requests_after = messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| matches!(m, ClipboardMessage::SendFileContentsRequest(_)))
            .count();
        assert_eq!(
            wire_requests_after, 1,
            "deferred downloads must begin after the paste grace period"
        );
        assert!(worker.pending_remote_files.is_none());
    }
}
