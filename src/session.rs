//! Session driver: builds engine `Config` from a `Profile`, runs `RdpClient` on a
//! dedicated thread, and pumps framebuffer/control events to the GUI.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Context as _;

use crate::profile::Profile;

/// Full framebuffer in egui colors, plus a dirty flag for the UI thread.
pub struct SharedFrame {
    inner: Mutex<FrameBuf>,
    dirty: AtomicBool,
}

pub struct FrameBuf {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<egui::Color32>,
}

impl SharedFrame {
    fn new() -> Self {
        Self {
            inner: Mutex::new(FrameBuf {
                width: 0,
                height: 0,
                pixels: Vec::new(),
            }),
            dirty: AtomicBool::new(false),
        }
    }

    fn blit(&self, update: ironrdp::client::rdp::DesktopUpdate) {
        let (buffer, width, height, region) = update.into_parts();
        let mut frame = self.inner.lock().expect("frame lock");
        if frame.width != width.get() || frame.height != height.get() {
            frame.width = width.get();
            frame.height = height.get();
            frame.pixels =
                vec![egui::Color32::BLACK; usize::from(width.get()) * usize::from(height.get())];
        }
        let stride = usize::from(frame.width);
        for (row, y) in (region.top..=region.bottom).enumerate() {
            let row_width = usize::from(region.right - region.left + 1);
            let src = &buffer[row * row_width..(row + 1) * row_width];
            let dst_start = usize::from(y) * stride + usize::from(region.left);
            let dst = &mut frame.pixels[dst_start..dst_start + src.len()];
            for (d, s) in dst.iter_mut().zip(src.iter()) {
                // Engine packs 0x00RRGGBB; egui wants RGBA.
                *d = egui::Color32::from_rgb(
                    ((s >> 16) & 0xFF) as u8,
                    ((s >> 8) & 0xFF) as u8,
                    (s & 0xFF) as u8,
                );
            }
        }
        drop(frame);
        self.dirty.store(true, Ordering::Release);
    }

    /// Returns a copy of the framebuffer if new pixels arrived since the last
    /// call. The shared buffer is left intact: the engine keeps accumulating
    /// region updates into it, so consecutive snapshots stay complete. (An
    /// earlier `take`-based version reset the buffer, so every snapshot after
    /// the first reallocated it black and the UI flickered between one freshly
    /// painted region and a black field.)
    pub fn snapshot_if_dirty(&self) -> Option<FrameBuf> {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return None;
        }
        let frame = self.inner.lock().expect("frame lock");
        if frame.width == 0 || frame.height == 0 {
            return None;
        }
        Some(FrameBuf {
            width: frame.width,
            height: frame.height,
            pixels: frame.pixels.clone(),
        })
    }

    pub fn inner_size(&self) -> (u16, u16) {
        let frame = self.inner.lock().expect("frame lock");
        (frame.width, frame.height)
    }
}

#[derive(Debug)]
pub enum SessionEvent {
    Connected,
    LoginComplete,
    Failed {
        error: String,
    },
    Terminated {
        reason: String,
    },
    Pointer(ironrdp::client::rdp::RdpOutputEvent),
    AutoReconnecting {
        attempt: u32,
        maximum: u32,
    },
    AutoReconnected,
    ResizeFallback {
        reason: ironrdp::client::rdp::DisplayResizeFallbackReason,
    },
    Note {
        text: String,
    },
}

pub struct SessionHandle {
    pub input: ironrdp::client::rdp::RdpInputSender,
    pub frame: Arc<SharedFrame>,
    pub events: std::sync::mpsc::Receiver<SessionEvent>,
    /// Keeps the OS clipboard bridge (and its thread) alive for the session.
    pub _clipboard: Option<crate::cliprdr::LinuxClipboard>,
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        // Hard cancel: works during connect and when the queue is full.
        self.input.request_close();
    }
}

pub fn start(profile: &Profile, ctx: egui::Context) -> anyhow::Result<SessionHandle> {
    let config = build_config(profile)?;

    let (output_tx, mut output_rx) = ironrdp::client::output_channel::output_channel(64);
    let mut client = ironrdp::client::rdp::RdpClient::new(config, output_tx)
        .with_desktop_updates()
        .with_auto_reconnect(profile.reconnect_attempts.max(1));

    let (event_tx, event_rx) = std::sync::mpsc::channel::<SessionEvent>();

    // Drive redirection: share a local folder as a drive on the remote side.
    let mut clipboard_staging_root = None;
    if let Some(root) = share_folder(profile) {
        match std::fs::canonicalize(&root) {
            Ok(canonical) => {
                let factory = ironrdp_rdpdr_native::backend::NixRdpdrBackendFactory::new(
                    canonical.to_string_lossy().into_owned(),
                )
                .with_drive(1, "RDPShare".to_owned());
                client = client.with_rdpdr_backend_factory(Box::new(factory));
                clipboard_staging_root = Some(canonical.clone());
                tracing::info!(
                    "sharing local folder {} as drive RDPShare",
                    canonical.display()
                );
            }
            Err(error) => {
                let message = format!(
                    "Shared folder {root:?} unavailable: {error}; drive redirection disabled"
                );
                tracing::warn!("{message}");
                let _ = event_tx.send(SessionEvent::Note { text: message });
            }
        }
    }

    // Clipboard: OS bridge thread forwards engine messages into the session.
    let mut clipboard = None;
    if profile.clipboard {
        let (proxy, rx) = crate::clipboard::QueuedCliprdrProxy::new();
        match crate::cliprdr::LinuxClipboard::with_staging_root(proxy, clipboard_staging_root) {
            Ok(os_clipboard) => {
                crate::clipboard::spawn_clipboard_forwarder(rx, client.input_sender());
                client = client.with_cliprdr_backend_factory(os_clipboard.backend_factory());
                clipboard = Some(os_clipboard);
            }
            Err(error) => {
                let message = format!("Clipboard redirection unavailable: {error}");
                tracing::warn!(%message);
                let _ = event_tx.send(SessionEvent::Note { text: message });
            }
        }
    }

    let input = client.input_sender();
    let frame = Arc::new(SharedFrame::new());
    let frame_pump = Arc::clone(&frame);

    std::thread::Builder::new()
        .name("rdp-session".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                // `run()` is not Send (holds the clipboard backend factory across
                // awaits), so drive it on a LocalSet alongside the output pump.
                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async move {
                        let runner = tokio::task::spawn_local(client.run());
                        while let Some(event) = output_rx.recv().await {
                            use ironrdp::client::rdp::RdpOutputEvent as E;
                            match event {
                                E::DesktopUpdate(update) => {
                                    frame_pump.blit(update);
                                    ctx.request_repaint();
                                }
                                E::Image {
                                    buffer,
                                    width,
                                    height,
                                } => {
                                    // Full snapshot fallback: blit as one region.
                                    let full = ironrdp::pdu::geometry::InclusiveRectangle {
                                        left: 0,
                                        top: 0,
                                        right: width.get().saturating_sub(1),
                                        bottom: height.get().saturating_sub(1),
                                    };
                                    if let Some(update) = ironrdp::client::rdp::DesktopUpdate::new(
                                        buffer, width, height, full,
                                    ) {
                                        frame_pump.blit(update);
                                        ctx.request_repaint();
                                    }
                                }
                                E::Connected => {
                                    let _ = event_tx.send(SessionEvent::Connected);
                                    ctx.request_repaint();
                                }
                                E::LoginComplete => {
                                    let _ = event_tx.send(SessionEvent::LoginComplete);
                                }
                                E::ConnectionFailure(error) => {
                                    let message = anyhow_error_string(&error);
                                    tracing::error!("connection failed: {message}");
                                    let _ = event_tx.send(SessionEvent::Failed { error: message });
                                    ctx.request_repaint();
                                }
                                E::Terminated(result) => {
                                    let reason = match result {
                                        Ok(reason) => format!("disconnected: {reason:?}"),
                                        Err(error) => anyhow_error_string(&error),
                                    };
                                    tracing::warn!("session ended: {reason}");
                                    let _ = event_tx.send(SessionEvent::Terminated { reason });
                                    ctx.request_repaint();
                                    break;
                                }
                                E::AutoReconnecting {
                                    attempt,
                                    maximum_attempts,
                                    response,
                                    ..
                                } => {
                                    let _ = response.send(
                                        ironrdp::client::rdp::AutoReconnectDecision::Continue,
                                    );
                                    let _ = event_tx.send(SessionEvent::AutoReconnecting {
                                        attempt,
                                        maximum: maximum_attempts,
                                    });
                                }
                                E::AutoReconnected => {
                                    let _ = event_tx.send(SessionEvent::AutoReconnected);
                                    ctx.request_repaint();
                                }
                                E::PostLogonDisplayRedraw => {
                                    tracing::debug!("engine: post-logon display redraw");
                                }
                                E::MalformedBitmapDisplayRedraw => {
                                    tracing::warn!("engine: malformed bitmap → full redraw");
                                }
                                E::DisplayResizeFallback(reason) => {
                                    tracing::debug!("resize fallback: {reason:?}");
                                    let _ = event_tx.send(SessionEvent::ResizeFallback { reason });
                                }
                                // Pointer, RAIL windowing, RDPEI state, redraw notices: pass through.
                                other => {
                                    let _ = event_tx.send(SessionEvent::Pointer(other));
                                }
                            }
                        }
                        // Pump broke (session ended); make sure the engine task is joined.
                        let _ = runner.await;
                    })
                    .await;
            });
        })
        .context("spawn session thread")?;

    Ok(SessionHandle {
        input,
        frame,
        events: event_rx,
        _clipboard: clipboard,
    })
}

fn anyhow_error_string(error: &(dyn std::error::Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

pub fn monitor_layout(
    count: u8,
    width: u32,
    height: u32,
) -> anyhow::Result<ironrdp::displaycontrol::pdu::DisplayControlMonitorLayout> {
    use ironrdp::displaycontrol::pdu::{DisplayControlMonitorLayout, MonitorLayoutEntry};

    let count = count.clamp(1, 4);
    let (width, height) = MonitorLayoutEntry::adjust_display_size(width, height);
    let mut monitors = Vec::with_capacity(usize::from(count));
    for index in 0..count {
        let left = i32::from(index) * i32::try_from(width)?;
        let monitor = if index == 0 {
            MonitorLayoutEntry::new_primary(width, height)?
        } else {
            MonitorLayoutEntry::new_secondary(width, height)?
        }
        .with_position(left, 0)?;
        monitors.push(monitor);
    }
    Ok(DisplayControlMonitorLayout::new(&monitors)?)
}

pub fn initial_monitor_data(
    count: u8,
    width: u32,
    height: u32,
) -> anyhow::Result<ironrdp::pdu::gcc::ClientMonitorData> {
    use ironrdp::pdu::gcc::{ClientMonitorData, Monitor, MonitorFlags};

    let layout = monitor_layout(count, width, height)?;
    let monitors = layout
        .monitors()
        .iter()
        .map(|monitor| {
            let (left, top) = monitor.position().unwrap_or_default();
            let (width, height) = monitor.dimensions();
            Monitor {
                left,
                top,
                right: left
                    .saturating_add(i32::try_from(width).unwrap_or(i32::MAX))
                    .saturating_sub(1),
                bottom: top
                    .saturating_add(i32::try_from(height).unwrap_or(i32::MAX))
                    .saturating_sub(1),
                flags: if monitor.is_primary() {
                    MonitorFlags::PRIMARY
                } else {
                    MonitorFlags::empty()
                },
            }
        })
        .collect();
    Ok(ClientMonitorData { monitors })
}

/// `Profile` → engine `ConfigBuilder`.
pub fn desktop_size(profile: &Profile) -> (u16, u16) {
    let count = profile.monitor_count.clamp(1, 4);
    let (mut width, height) = match profile.desktop {
        crate::profile::DesktopMode::FitWindow => (1600u16, 900u16),
        crate::profile::DesktopMode::Fixed { width, height } => (width, height),
    };
    width = width.min(u16::MAX / u16::from(count));
    (width * u16::from(count), height)
}

pub fn build_config(profile: &Profile) -> anyhow::Result<ironrdp::client::config::Config> {
    use ironrdp::client::config::{ConfigBuilder, Destination, TransportKind};

    let destination = Destination::new(&profile.address)
        .with_context(|| format!("invalid address: {}", profile.address))?;

    let (username, domain) = split_username(&profile.username);

    let (width, height) = desktop_size(profile);

    let mut builder = ConfigBuilder::new()
        .with_destination(destination)
        .with_username(username.clone())
        .with_password(profile.password.clone())
        .with_color_depth(profile.color_depth)
        .with_desktop_width(width)
        .with_desktop_height(height)
        .with_monitor_layout(initial_monitor_data(
            profile.monitor_count,
            u32::from(width) / u32::from(profile.monitor_count.clamp(1, 4)),
            u32::from(height),
        )?)
        .with_compression(profile.compression)
        .with_lossy_compression(true)
        .with_performance_flags(perf_flags(&profile.perf))
        // mstsc-style client identification (required by the engine).
        .with_client_build(22621)
        .with_client_name(client_name())
        .with_client_dir("C:\\Windows\\System32\\mstscax.dll".to_owned())
        .with_platform(ironrdp::pdu::rdp::capability_sets::MajorPlatformType::UNIX)
        .with_graphics_pipeline(false);

    builder = builder.with_clipboard(if profile.clipboard {
        ironrdp::client::config::ClipboardType::Enable
    } else {
        ironrdp::client::config::ClipboardType::Disable
    });

    tracing::debug!(
        "NLA credentials: user={username:?} domain={domain:?} password={} chars",
        profile.password.chars().count()
    );

    if !domain.is_empty() {
        builder = builder.with_domain(domain);
    }

    if profile.validate_tls {
        builder = builder.with_certificate_validation(ironrdp_tls::CertificateValidation::Strict);
    }

    #[cfg(feature = "audio")]
    {
        builder = builder
            .with_sound(profile.audio)
            .with_audio_capture(profile.audio_capture);
    }

    if let Some(gateway) = &profile.gateway {
        builder = builder
            .with_transport(TransportKind::Gateway {
                endpoint: gateway.address.clone(),
                prefer_direct: false,
            })
            .with_gateway_username(gateway.username.clone())
            .with_gateway_password(gateway.password.clone());
    }

    if !profile.remote_app.is_empty() {
        builder = builder
            .with_remote_application_mode(true)
            .with_alternate_shell(profile.remote_app.clone());
        if !profile.remote_app_dir.is_empty() {
            builder = builder.with_work_dir(profile.remote_app_dir.clone());
        }
    }

    if profile.administrative_session {
        builder = builder.with_administrative_session(true);
    }

    if share_folder(profile).is_some() {
        builder = builder.with_rdpdr(true);
    }

    let config = builder.build()?;
    Ok(config)
}

/// `DOMAIN\user` → (user, domain). UPN form (`user@domain`) is passed through as-is.
fn split_username(username: &str) -> (String, String) {
    match username.rsplit_once('\\') {
        Some((domain, user)) if !domain.is_empty() && !user.is_empty() => {
            (user.to_owned(), domain.to_owned())
        }
        _ => (username.to_owned(), String::new()),
    }
}

/// The local folder to redirect, if configured (empty string → none).
fn share_folder(profile: &Profile) -> Option<String> {
    profile
        .share_folder
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .map(|f| {
            if let Some(rest) = f.strip_prefix('~') {
                let rest = rest.strip_prefix('/').unwrap_or(rest);
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(rest)
                    .to_string_lossy()
                    .into_owned()
            } else {
                f.to_owned()
            }
        })
}

/// Local hostname for the RDP client-name field (mstsc sends its host name).
fn client_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "rstsc".to_owned())
}

/// Profile booleans → PDU performance flags. Most bits *disable* a feature when
/// set, while font smoothing has an *enable* bit.
fn perf_flags(
    perf: &crate::profile::PerfFlags,
) -> ironrdp::pdu::rdp::client_info::PerformanceFlags {
    use ironrdp::pdu::rdp::client_info::PerformanceFlags as F;
    let mut flags = F::empty();
    if !perf.wallpaper {
        flags |= F::DISABLE_WALLPAPER;
    }
    if perf.font_smoothing {
        flags |= F::ENABLE_FONT_SMOOTHING;
    }
    if !perf.window_dragging {
        flags |= F::DISABLE_FULLWINDOWDRAG;
    }
    if !perf.menu_animations {
        flags |= F::DISABLE_MENUANIMATIONS;
    }
    if !perf.themes {
        flags |= F::DISABLE_THEMING;
    }
    if !perf.cursor_settings {
        flags |= F::DISABLE_CURSORSETTINGS;
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full-frame update must not index out of bounds (regression: `..=` range
    /// panicked on the last row while holding the frame lock, which poisoned the
    /// mutex and aborted the UI thread → silent process exit after login).
    #[test]
    fn blit_full_frame() {
        let frame = SharedFrame::new();
        let w = 3u16;
        let h = 2u16;
        // 0x00RRGGBB packed: row 0 = red/green/blue, row 1 = white/black/gray.
        let buffer = vec![0xFF0000, 0x00FF00, 0x0000FF, 0xFFFFFF, 0x000000, 0x808080];
        let region = ironrdp::pdu::geometry::InclusiveRectangle {
            left: 0,
            top: 0,
            right: w - 1,
            bottom: h - 1,
        };
        let update = ironrdp::client::rdp::DesktopUpdate::new(
            buffer,
            w.try_into().unwrap(),
            h.try_into().unwrap(),
            region,
        )
        .expect("valid update");
        frame.blit(update);

        assert_eq!(frame.inner_size(), (w, h));
        let buf = frame.inner.lock().unwrap();
        assert_eq!(buf.pixels[0], egui::Color32::from_rgb(0xFF, 0x00, 0x00));
        assert_eq!(buf.pixels[5], egui::Color32::from_rgb(0x80, 0x80, 0x80));
    }

    /// A partial update must land at the right offset in the framebuffer.
    #[test]
    fn blit_partial_region() {
        let frame = SharedFrame::new();
        let (w, h) = (4u16, 4u16);
        let full = ironrdp::pdu::geometry::InclusiveRectangle {
            left: 0,
            top: 0,
            right: w - 1,
            bottom: h - 1,
        };
        frame.blit(
            ironrdp::client::rdp::DesktopUpdate::new(
                vec![0x112233; usize::from(w) * usize::from(h)],
                w.try_into().unwrap(),
                h.try_into().unwrap(),
                full,
            )
            .unwrap(),
        );
        let patch = ironrdp::pdu::geometry::InclusiveRectangle {
            left: 1,
            top: 2,
            right: 2,
            bottom: 2,
        };
        frame.blit(
            ironrdp::client::rdp::DesktopUpdate::new(
                vec![0xABCDEF; 2],
                w.try_into().unwrap(),
                h.try_into().unwrap(),
                patch,
            )
            .unwrap(),
        );
        let buf = frame.inner.lock().unwrap();
        assert_eq!(
            buf.pixels[2 * 4 + 1],
            egui::Color32::from_rgb(0xAB, 0xCD, 0xEF)
        );
        assert_eq!(
            buf.pixels[2 * 4 + 2],
            egui::Color32::from_rgb(0xAB, 0xCD, 0xEF)
        );
        assert_eq!(
            buf.pixels[2 * 4 + 3],
            egui::Color32::from_rgb(0x11, 0x22, 0x33)
        );
        assert_eq!(
            buf.pixels[1 * 4 + 1],
            egui::Color32::from_rgb(0x11, 0x22, 0x33)
        );
    }
}
#[test]
fn snapshot_preserves_buffer() {
    let frame = SharedFrame::new();
    let (w, h) = (2u16, 2u16);
    let full = ironrdp::pdu::geometry::InclusiveRectangle {
        left: 0,
        top: 0,
        right: w - 1,
        bottom: h - 1,
    };
    frame.blit(
        ironrdp::client::rdp::DesktopUpdate::new(
            vec![0x111111; 4],
            w.try_into().unwrap(),
            h.try_into().unwrap(),
            full,
        )
        .unwrap(),
    );
    let first = frame.snapshot_if_dirty().unwrap();
    assert_eq!(
        first.pixels,
        vec![egui::Color32::from_rgb(0x11, 0x11, 0x11); 4]
    );
    // No new updates → no snapshot, and the shared buffer must survive the
    // previous snapshot (regression: `take` reset it to empty, so the next
    // blit reallocated an all-black framebuffer → screen flicker).
    assert!(frame.snapshot_if_dirty().is_none());
    let patch = ironrdp::pdu::geometry::InclusiveRectangle {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    frame.blit(
        ironrdp::client::rdp::DesktopUpdate::new(
            vec![0xFFFFFF],
            w.try_into().unwrap(),
            h.try_into().unwrap(),
            patch,
        )
        .unwrap(),
    );
    let second = frame.snapshot_if_dirty().unwrap();
    assert_eq!(second.pixels.len(), 4);
    assert_eq!(second.pixels[0], egui::Color32::from_rgb(0xFF, 0xFF, 0xFF));
    assert_eq!(second.pixels[1], egui::Color32::from_rgb(0x11, 0x11, 0x11));
}
#[cfg(test)]
mod monitor_tests {
    use super::*;

    #[test]
    fn monitor_layout_is_horizontal_and_clamped_to_supported_count() {
        let layout = monitor_layout(99, 1280, 720).unwrap();
        assert_eq!(layout.monitors().len(), 4);
        assert!(layout.monitors()[0].is_primary());
        assert_eq!(layout.monitors()[0].position(), Some((0, 0)));
        assert_eq!(layout.monitors()[1].position(), Some((1280, 0)));
        assert_eq!(layout.desktop_size(), (5120, 720));
    }
}
#[cfg(test)]
mod config_monitor_tests {
    use super::*;

    #[test]
    fn multi_monitor_config_uses_combined_width() {
        let mut profile = Profile::new("host");
        profile.monitor_count = 2;
        let config = build_config(&profile).unwrap();
        assert_eq!(desktop_size(&profile), (3200, 900));
    }
}
