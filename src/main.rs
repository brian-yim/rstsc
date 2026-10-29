//! rstsc: a Linux RDP client with an mstsc-style GUI.
//!
//! Protocol stack: IronRDP (`ironrdp` facade, engine `ironrdp-client`).
//! GUI: egui/eframe rendering the engine framebuffer as a texture.

mod app;
mod clipboard;
mod cliprdr;
mod keymap;
mod profile;
mod session;
mod theme;
mod vault;

use std::str::FromStr as _;

use anyhow::bail;

/// Cheap UTC timestamp for the panic log (no chrono dependency).
fn chrono_like_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("epoch+{}s", now.as_secs())
}

/// App icon for the window/taskbar, embedded at compile time from
/// `assets/rstsc.png` (512×512 RGBA, transparent outside the rounded mark).
fn icon_data() -> egui::IconData {
    const ICON_PNG: &[u8] = include_bytes!("../assets/rstsc.png");
    let img = image::load_from_memory(ICON_PNG).expect("embedded app icon is a valid PNG");
    let rgba = img.to_rgba8();
    egui::IconData {
        width: rgba.width(),
        height: rgba.height(),
        rgba: rgba.into_raw(),
    }
}

fn main() -> anyhow::Result<()> {
    // Crash evidence: Rust panics anywhere (engine thread, UI) land in a log
    // file, not just a terminal that may not exist.
    let log_dir = dirs::state_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("rstsc");
    let _ = std::fs::create_dir_all(&log_dir);
    let panic_log = log_dir.join("panic.log");
    std::panic::set_hook(Box::new(move |info| {
        let message = format!(
            "{} UTC panic in {}:{}: {info}\n",
            chrono_like_timestamp(),
            std::thread::current().name().unwrap_or("<unnamed>"),
            info.location().map(|l| l.to_string()).unwrap_or_default(),
        );
        eprintln!("{message}");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_log)
            .and_then(|mut f| std::io::Write::write_all(&mut f, message.as_bytes()));
    }));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,rdp_client=debug")),
        )
        .init();

    let mut address: Option<String> = None;
    let mut rdp_file: Option<String> = None;
    let mut profile_name: Option<String> = None;
    let mut fullscreen = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--address" | "-a" => address = Some(args.next().unwrap_or_default()),
            "--rdp" | "-r" => rdp_file = Some(args.next().unwrap_or_default()),
            "--profile" | "-p" => profile_name = Some(args.next().unwrap_or_default()),
            "--fullscreen" | "-f" => fullscreen = true,
            "--help" | "-h" => {
                println!(
                    "Usage: rstsc [OPTIONS]\n\
                     \n\
                     With no options, opens the connection manager GUI.\n\
                     \n\
                     Options:\n\
                     \x20 -a, --address <host[:port]>  Connect immediately\n\
                     \x20 -p, --profile <name>         Connect using a saved profile\n\
                     \x20 -r, --rdp <file.rdp>         Load settings from an .rdp file\n\
                     \x20 -f, --fullscreen             Start fullscreen\n\
                     \x20 -h, --help                   Show this help"
                );
                return Ok(());
            }
            other => {
                if other.starts_with('-') {
                    bail!("unknown option: {other} (see --help)");
                }
                // Bare argument: treat as address (mstsc `mstsc host` convenience).
                if address.is_none() {
                    address = Some(other.to_owned());
                }
            }
        }
    }

    let profiles = profile::load_profiles()?;

    let initial = if let Some(name) = profile_name {
        match profiles.iter().find(|p| p.name == name) {
            Some(p) => app::Launch::Profile(p.clone(), fullscreen),
            None => bail!("profile not found: {name}"),
        }
    } else if let Some(path) = rdp_file {
        app::Launch::RdpFile(std::path::PathBuf::from_str(&path)?, fullscreen)
    } else if let Some(addr) = address {
        app::Launch::Address(addr, fullscreen)
    } else {
        app::Launch::Manager
    };

    // AutoNoVsync: one less frame of queueing on the input→wire→screen path;
    // remote-desktop content updates arrive at ~10-30 Hz, not monitor Hz.
    let wgpu_options = eframe::egui_wgpu::WgpuConfiguration {
        surface: eframe::egui_wgpu::SurfaceConfig {
            // AutoNoVsync + latency 1: one less frame of queueing on the
            // input→wire→screen path; remote content updates arrive at
            // ~10-30 Hz, not monitor Hz.
            present_mode: eframe::egui_wgpu::wgpu::PresentMode::AutoNoVsync,
            desired_maximum_frame_latency: Some(1),
        },
        ..Default::default()
    };
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 640.0])
            .with_min_inner_size([640.0, 480.0])
            .with_drag_and_drop(true)
            .with_title("rstsc")
            // Taskbar/alt-tab icon: the crab-over-monitor mark. 128 px is the
            // largest size Wayland compositors reliably take from an app;
            // the X11 property path also accepts it.
            .with_icon(icon_data()),
        // egui-winit's built-in copy/cut/paste handling must stay OFF: on
        // Ctrl+V it synchronously reads the OS clipboard (arboard), and when
        // the session's file mirror owns the clipboard with only URI formats
        // that read returns empty — then egui-winit DROPS the keystroke
        // entirely (no Paste event AND no Key event), so a Ctrl+V aimed at
        // the remote never reaches Windows. With the interception off, the
        // keys flow as plain Key events: `drain_input` re-injects them over
        // the remote surface, and the manager screen synthesizes local
        // clipboard commands itself.
        clipboard_shortcuts: false,
        wgpu_options,
        ..Default::default()
    };

    eframe::run_native(
        "rstsc",
        native_options,
        Box::new(move |cc| {
            theme::apply(&cc.egui_ctx);
            let app = app::App::new(cc, initial);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))
}
