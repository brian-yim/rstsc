//! Clipboard plumbing: queues engine-bound clipboard messages from the OS clipboard
//! thread until the session input sender exists, then forwards them.

use ironrdp::cliprdr::backend::{ClipboardMessage, ClipboardMessageProxy};

/// Proxy handed to `LinuxClipboard::new`. Called from the clipboard worker thread.
#[derive(Debug)]
pub struct QueuedCliprdrProxy {
    tx: std::sync::mpsc::Sender<ClipboardMessage>,
}

impl QueuedCliprdrProxy {
    pub fn new() -> (Self, std::sync::mpsc::Receiver<ClipboardMessage>) {
        let (tx, rx) = std::sync::mpsc::channel();
        (Self { tx }, rx)
    }
}

impl ClipboardMessageProxy for QueuedCliprdrProxy {
    fn send_clipboard_message(&self, message: ClipboardMessage) {
        let description = match &message {
            ClipboardMessage::SendInitiateFileCopy(files) => format!("file list ({})", files.len()),
            ClipboardMessage::SendFileContentsRequest(request) => {
                format!("file request {}", request.stream_id)
            }
            ClipboardMessage::SendFileContentsResponse(response) => format!(
                "file response {} (error={})",
                response.stream_id(),
                response.is_error()
            ),
            ClipboardMessage::SendInitiatePaste(format) => format!("format request {:?}", format),
            ClipboardMessage::SendInitiateCopy(formats) => {
                format!("format list ({})", formats.len())
            }
            ClipboardMessage::SendFormatData(response) => {
                format!("format data (error={})", response.is_error())
            }
            ClipboardMessage::Error(_) => "clipboard error".to_owned(),
        };
        tracing::debug!(%description, "Queueing CLIPRDR message for RDP session");
        if self.tx.send(message).is_err() {
            tracing::warn!(%description, "RDP session input queue rejected CLIPRDR message");
        }
    }
}

/// Pumps queued clipboard messages into the session input sender.
pub fn spawn_clipboard_forwarder(
    rx: std::sync::mpsc::Receiver<ClipboardMessage>,
    input: ironrdp::client::rdp::RdpInputSender,
) {
    std::thread::Builder::new()
        .name("cliprdr-forward".into())
        .spawn(move || {
            while let Ok(message) = rx.recv() {
                let description = match &message {
                    ClipboardMessage::SendInitiateFileCopy(files) => {
                        format!("file list ({})", files.len())
                    }
                    ClipboardMessage::SendFileContentsRequest(request) => {
                        format!("file request {}", request.stream_id)
                    }
                    ClipboardMessage::SendFileContentsResponse(response) => {
                        format!("file response {}", response.stream_id())
                    }
                    ClipboardMessage::SendInitiatePaste(format) => {
                        format!("format request {:?}", format)
                    }
                    ClipboardMessage::SendInitiateCopy(formats) => {
                        format!("format list ({})", formats.len())
                    }
                    ClipboardMessage::SendFormatData(_) => "format data".to_owned(),
                    ClipboardMessage::Error(_) => "clipboard error".to_owned(),
                };
                if input.send_clipboard(message).is_err() {
                    tracing::warn!(%description, "RDP session rejected CLIPRDR message");
                    break;
                }
                tracing::debug!(%description, "Forwarded CLIPRDR message to RDP session");
            }
        })
        .ok();
}
