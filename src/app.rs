//! GUI: connection manager (launcher) + session view (framebuffer, input, toolbar).

use crate::keymap::ModifierTracker;
use crate::profile::{self, DesktopMode, Profile};
use crate::session::{self, SessionEvent, SessionHandle};
use crate::vault;

pub enum Launch {
    Manager,
    Address(String, bool),
    Profile(Profile, bool),
    RdpFile(std::path::PathBuf, bool),
}

enum Screen {
    Manager(Box<ManagerState>),
    Session(Box<SessionState>),
}

pub struct App {
    screen: Screen,
    profiles: Vec<Profile>,
    pending_launch: Option<(Profile, bool)>,
    vault: vault::Vault,
    /// Window title last pushed to the compositor (dedup Title commands).
    reported_title: Option<String>,
}

struct ManagerState {
    draft: Profile,
    selected: Option<usize>,
    error: Option<String>,
    /// Master passphrase entry (first = passphrase, second = repeat when
    /// creating a fresh vault).
    master_passphrase: (String, String),
    /// Last vault unlock/set error for display.
    vault_error: Option<String>,
}
struct SessionState {
    profile: Profile,
    handle: SessionHandle,
    texture: Option<egui::TextureHandle>,
    status: Status,
    error: Option<String>,
    note: Option<String>,
    fit: bool,
    fullscreen: bool,
    pointer: PointerView,
    input_db: ironrdp::input::Database,
    mods: ModifierTracker,
    /// Last desktop size sent for dynamic resize (debounced).
    last_resize: Option<(u16, u16, std::time::Instant)>,
    /// Surface rect from the previous frame; used to map input coordinates.
    surface_rect: Option<egui::Rect>,
    /// Last known pointer position from events (wheel gating).
    last_pos: Option<egui::Pos2>,
    /// Discriminant of the cursor shape last pushed to the platform.
    applied_pointer: Option<std::mem::Discriminant<PointerView>>,
    /// Window area available for the surface this frame (drives desired resolution).
    last_available: Option<egui::Vec2>,
    /// When to re-send the current window size after a resize fallback.
    resize_retry_at: Option<std::time::Instant>,
    resize_failures: u32,
    /// `-f`/`--fullscreen`: command not yet sent (self.fullscreen is pre-set).
    fullscreen_pending: bool,
    /// Fullscreen state last reported by the compositor (for change logging).
    reported_fullscreen: Option<bool>,
    /// When the session logged in / connected; dynamic resize waits for the
    /// desktop to settle (a resize reactivation during first-logon setup
    /// freezes host output for minutes).
    session_up_since: Option<std::time::Instant>,
    /// Monitor shown in `None` = all configured monitors side by side;
    /// `Some(i)` views only remote monitor `i`. View-only: no protocol
    /// change, the framebuffer keeps its full multi-monitor geometry.
    viewed_monitor: Option<u8>,
}
enum Status {
    Connecting,
    Connected,
    Failed,
    Terminated,
}

enum PointerView {
    Default,
    Hidden,
    Custom(Arc<egui::CustomCursorImage>),
}

use std::sync::Arc;

impl App {
    /// Rehydrates `p.password` (+ gateway password) from the vault for the
    /// profiles that remember one. Call on every launch path once the vault
    /// is unlocked.
    fn hydrate_vault_passwords(&mut self, p: &mut Profile) {
        if let Ok(Some(stored)) = self.vault.password(&p.name) {
            p.password = stored;
        }
        if let Some(gateway) = &mut p.gateway
            && let Ok(Some(stored)) = self.vault.password(&format!("{} / gateway", p.name))
        {
            gateway.password = stored;
        }
    }

    /// Moves plaintext passwords from profiles.json into the vault and clears
    /// them from the on-disk JSON.
    fn scrub_plaintext_passwords(&mut self) {
        let migrated = self.vault.migrate_plaintext(&mut self.profiles);
        if migrated > 0 {
            tracing::info!(
                profiles = migrated,
                "moved plaintext passwords into the vault"
            );
            if let Err(error) = profile::save_profiles(&self.profiles) {
                tracing::warn!(%error, "could not rewrite profiles.json after migration");
            }
        }
    }

    pub fn new(_cc: &eframe::CreationContext<'_>, launch: Launch) -> Self {
        let profiles = profile::load_profiles().unwrap_or_default();
        let vault = vault::Vault::load().unwrap_or_else(|error| {
            tracing::warn!(%error, "Could not read the password vault; remembered passwords unavailable");
            vault::Vault::fresh()
        });
        let mut app = Self {
            screen: Screen::Manager(Box::new(ManagerState {
                draft: Profile::new(String::new()),
                selected: None,
                error: None,
                master_passphrase: (String::new(), String::new()),
                vault_error: None,
            })),
            profiles,
            pending_launch: None,
            reported_title: None,
            vault,
        };
        // profiles.json may still hold plaintext passwords from an older
        // version. Scrub them into the vault as soon as it is unlocked (an
        // unlock happens lazily; a locked vault keeps the plaintext until
        // the user unlocks, then the scrub re-runs from the UI).
        if app.vault.is_unlocked() {
            app.scrub_plaintext_passwords();
        }
        match launch {
            Launch::Manager => {}
            Launch::Address(addr, fullscreen) => {
                let mut p = app
                    .profiles
                    .iter()
                    .find(|p| p.address == addr)
                    .cloned()
                    .unwrap_or_else(|| Profile::new(addr));
                p.password.clear();
                app.hydrate_vault_passwords(&mut p);
                app.pending_launch = Some((p, fullscreen));
            }
            Launch::Profile(mut p, fullscreen) => {
                p.password.clear();
                app.hydrate_vault_passwords(&mut p);
                app.pending_launch = Some((p, fullscreen));
            }
            Launch::RdpFile(path, fullscreen) => match load_rdp_profile(&path) {
                Ok(mut p) => {
                    p.password.clear();
                    app.hydrate_vault_passwords(&mut p);
                    app.pending_launch = Some((p, fullscreen));
                }
                Err(error) => {
                    if let Screen::Manager(m) = &mut app.screen {
                        m.error = Some(format!("{}: {error}", path.display()));
                    }
                }
            },
        }
        if let Some((p, _)) = &app.pending_launch {
            if let Screen::Manager(m) = &mut app.screen {
                m.draft = p.clone();
            }
        }
        app
    }

    fn start_session(&mut self, ctx: &egui::Context, profile: Profile, fullscreen: bool) {
        match session::start(&profile, ctx.clone()) {
            Ok(handle) => {
                self.screen = Screen::Session(Box::new(SessionState {
                    profile,
                    handle,
                    texture: None,
                    status: Status::Connecting,
                    error: None,
                    note: None,
                    fit: true,
                    fullscreen,
                    pointer: PointerView::Default,
                    input_db: ironrdp::input::Database::new(),
                    mods: ModifierTracker::default(),
                    last_resize: None,
                    surface_rect: None,
                    last_pos: None,
                    applied_pointer: None,
                    resize_retry_at: None,
                    resize_failures: 0,
                    last_available: None,
                    fullscreen_pending: fullscreen,
                    reported_fullscreen: None,
                    session_up_since: None,
                    viewed_monitor: None,
                }));
            }
            Err(error) => {
                tracing::error!("could not start session: {error:#}");
                if let Screen::Manager(m) = &mut self.screen {
                    m.error = Some(format!("connect: {error:#}"));
                }
            }
        }
    }

    fn back_to_manager(&mut self, error: Option<String>) {
        let draft = match &self.screen {
            Screen::Session(s) => {
                let mut d = s.profile.clone();
                d.password.clear();
                d
            }
            _ => Profile::new(String::new()),
        };
        self.screen = Screen::Manager(Box::new(ManagerState {
            draft,
            selected: None,
            error,
            master_passphrase: (String::new(), String::new()),
            vault_error: None,
        }));
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some((profile, fullscreen)) = self.pending_launch.take() {
            self.start_session(ctx, profile, fullscreen);
        }
        if let Screen::Session(s) = &mut self.screen {
            s.drain_events(ctx);
            if s.connected() {
                s.drain_input(ctx);
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut back: Option<Option<String>> = None;
        let mut restart: Option<Profile> = None;
        let mut leave_fullscreen = false;

        // Window title: "<profile or host> — rstsc" in a session,
        // plain "rstsc" in the connection manager.
        let title = match &self.screen {
            Screen::Manager(_) => "rstsc".to_owned(),
            Screen::Session(s) => {
                let p = &s.profile;
                if p.name.trim().is_empty() || p.name == p.address {
                    format!("{} — rstsc", p.address)
                } else {
                    format!("{} — rstsc", p.name)
                }
            }
        };
        if self.reported_title.as_deref() != Some(title.as_str()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.reported_title = Some(title);
        }

        if matches!(self.screen, Screen::Manager(_)) {
            let available = ui.available_size();
            ui.allocate_ui_with_layout(available, egui::Layout::top_down(egui::Align::Min), |ui| {
                ui.set_min_size(available);
                // Split the borrows: manager state, profile list, launch
                // pipeline and vault are disjoint fields.
                let Screen::Manager(m) = &mut self.screen else {
                    unreachable!()
                };
                let Self {
                    profiles,
                    pending_launch,
                    vault,
                    ..
                } = self;
                let m = &mut **m;
                App::draw_manager(vault, ui, m, profiles, pending_launch);
            });
        } else if let Screen::Session(s) = &mut self.screen {
            s.draw(&ctx, ui, &mut back, &mut restart, &mut leave_fullscreen);
        }

        if let Some(error) = back {
            // Leaving the session must also leave fullscreen, or the manager
            // renders fullscreen with no decorations and the app can't be
            // closed (reported as "close button never shows again").
            if leave_fullscreen {
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
            self.back_to_manager(error);
        }
        if let Some(profile) = restart {
            self.start_session(&ctx, profile, false);
        }
    }
}

impl App {
    fn draw_manager(
        vault: &mut vault::Vault,
        ui: &mut egui::Ui,
        state: &mut ManagerState,
        profiles: &mut Vec<Profile>,
        pending: &mut Option<(Profile, bool)>,
    ) {
        // Vault actions gathered from the frame; applied after drawing.
        let mut vault_unlock: Option<String> = None;
        ui.allocate_ui_with_layout(
            ui.available_size(),
            egui::Layout::left_to_right(egui::Align::Min),
            |ui| {
                // ── Left rail: app identity + saved connections ──
                ui.allocate_ui_with_layout(
                    egui::vec2(280.0, ui.available_height()),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.set_width(280.0);
                        egui::Frame::default()
                            .fill(crate::theme::BG_PANEL)
                            .inner_margin(egui::Margin::symmetric(12, 12))
                            .show(ui, |ui| {
                                ui.set_width(256.0);
                                ui.set_min_height(ui.available_height());
                                ui.label(egui::RichText::new("rstsc").strong().size(17.0));
                                ui.add_space(2.0);
                                ui.label(egui::RichText::new("Saved connections").weak().small());
                                ui.add_space(6.0);

                                let mut picked = None;
                                let mut connect_now = None;
                                let list_height = (ui.available_height() - 96.0).max(80.0);
                                egui::ScrollArea::vertical()
                                    .max_height(list_height)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        for (i, p) in profiles.iter().enumerate() {
                                            let label = ui.selectable_label(
                                                state.selected == Some(i),
                                                egui::RichText::new(format!(
                                                    "{}\n{}",
                                                    p.name, p.address
                                                )),
                                            );
                                            if label.clicked() {
                                                picked = Some(i);
                                            }
                                            if label.double_clicked() {
                                                connect_now = Some(i);
                                            }
                                            ui.add_space(4.0);
                                        }
                                    });
                                if let Some(i) = picked {
                                    state.selected = Some(i);
                                    state.draft = profiles[i].clone();
                                }
                                if let Some(i) = connect_now {
                                    let mut p = profiles[i].clone();
                                    let has_stored = vault.has_password(&p.name);
                                    if !p.remember_password && !has_stored {
                                        p.password.clear();
                                    }
                                    if (p.remember_password || has_stored)
                                        && p.password.is_empty()
                                        && let Ok(Some(stored)) = vault.password(&p.name)
                                    {
                                        p.password = stored;
                                    }
                                    if let Some(gateway) = &mut p.gateway
                                        && let Ok(Some(stored)) =
                                            vault.password(&format!("{} / gateway", p.name))
                                    {
                                        gateway.password = stored;
                                    }
                                    *pending = Some((p, false));
                                }
                                // Save/Delete: in flow after the list, not a
                                // bottom-up pinned row (overlaps the list
                                // when the rail runs out of height).
                                ui.add_space(8.0);
                                ui.separator();
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    if ui.button("Save").clicked() {
                                        let mut p = state.draft.clone();
                                        if p.name.is_empty() {
                                            p.name = p
                                                .address
                                                .split(':')
                                                .next()
                                                .unwrap_or(&p.address)
                                                .to_owned();
                                        }
                                        // Renaming a profile must carry its
                                        // stored password to the new key.
                                        if let Some(i) = state.selected
                                            && profiles[i].name != p.name
                                        {
                                            let old = profiles[i].name.clone();
                                            match vault.password(&old) {
                                                Ok(Some(pwd)) => {
                                                    let _ = vault.set_password(&p.name, &pwd);
                                                    let _ = vault.remove_password(&old);
                                                }
                                                Ok(None) => {}
                                                Err(error) => {
                                                    // Locked vault: keep the
                                                    // plaintext-profile path
                                                    // (nothing to migrate).
                                                    tracing::warn!(
                                                        %error,
                                                        profile = %old,
                                                        "could not read the vault \
                                                         entry while renaming"
                                                    );
                                                }
                                            }
                                        }
                                        // Passwords live in the encrypted
                                        // vault when "remember" is on;
                                        // profiles.json never sees them.
                                        if p.remember_password && !p.password.is_empty() {
                                            if !vault.is_unlocked() {
                                                state.error = Some(
                                                    "Password vault is locked — \
                                                         set the master passphrase below first"
                                                        .to_owned(),
                                                );
                                            } else if let Err(error) =
                                                vault.set_password(&p.name, &p.password)
                                            {
                                                state.error = Some(error.to_string());
                                            }
                                        } else if !p.remember_password {
                                            let _ = vault.remove_password(&p.name);
                                        }
                                        // Gateway password rides along in
                                        // the vault when a remember flag is
                                        // set and a gateway is configured.
                                        if p.remember_password && !vault.is_unlocked() {
                                            state.error = Some(
                                                "Password vault is locked — \
                                                     set the master passphrase below first"
                                                    .to_owned(),
                                            );
                                        } else if let Some(gateway) = &mut p.gateway
                                            && p.remember_password
                                        {
                                            if gateway.password.is_empty() {
                                                let _ = vault.remove_password(&format!(
                                                    "{} / gateway",
                                                    p.name
                                                ));
                                            } else if let Err(error) = vault.set_password(
                                                &format!("{} / gateway", p.name),
                                                &gateway.password,
                                            ) {
                                                state.error = Some(error.to_string());
                                            }
                                            gateway.password.clear();
                                        }
                                        // The profile on disk intentionally
                                        // carries no password.
                                        p.password.clear();
                                        p.remember_password =
                                            p.remember_password && vault.has_password(&p.name);
                                        let mut p = p;
                                        if vault.has_password(&p.name) {
                                            // Re-fetch the password so the
                                            // pending draft/launch has it.
                                            if let Ok(Some(pwd)) = vault.password(&p.name) {
                                                p.password = pwd;
                                            }
                                        }
                                        if let Some(i) = state.selected {
                                            profiles[i] = p.clone();
                                        } else {
                                            state.selected = Some(profiles.len());
                                            profiles.push(p.clone());
                                        }
                                        state.draft = profiles[state.selected.unwrap()].clone();
                                        state.error = profile::save_profiles(profiles)
                                            .err()
                                            .map(|e| e.to_string());
                                        // The in-memory list may carry the
                                        // password only transiently; the
                                        // vault rehydrates on connect.
                                        profiles[state.selected.unwrap()].password.clear();
                                    }
                                    if ui
                                        .add_enabled(
                                            state.selected.is_some(),
                                            egui::Button::new("Delete"),
                                        )
                                        .clicked()
                                        && let Some(i) = state.selected.take()
                                    {
                                        let removed = profiles.remove(i);
                                        if let Err(error) = vault.remove_password(&removed.name) {
                                            tracing::warn!(
                                                %error,
                                                profile = %removed.name,
                                                "could not remove the vault entry"
                                            );
                                        }
                                        state.error = profile::save_profiles(profiles)
                                            .err()
                                            .map(|e| e.to_string());
                                    }
                                });
                            });
                    },
                );
                // Right column is one vertical stack: the scrolling form plus
                // the fixed action footer. The ScrollArea height is capped to
                // (available - footer) explicitly: an unbounded ScrollArea
                // swallows the whole column height and pushes the footer
                // off the window.
                const FOOTER_H: f32 = 92.0;
                let column_height = ui.available_height();
                let scroll_height = (column_height - FOOTER_H).max(80.0);
                ui.vertical(|ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .max_height(scroll_height)
                        .show(ui, |ui| {
                            ui.vertical(|ui| {
                                ui.set_width(ui.available_width().min(900.0));
                                ui.label(egui::RichText::new("New connection").strong().size(17.0));
                                ui.add_space(8.0);
                                let form_width = ui.available_width();
                                egui::Grid::new("conn")
                                    .num_columns(2)
                                    .spacing([10.0, 8.0])
                                    .show(ui, |ui| {
                                        ui.label("Profile name:");
                                        ui.add(
                                            egui::TextEdit::singleline(&mut state.draft.name)
                                                .hint_text("(defaults to the host name)")
                                                .desired_width((form_width - 160.0).max(80.0)),
                                        );
                                        ui.end_row();

                                        ui.label("Computer:");
                                        ui.add(
                                            egui::TextEdit::singleline(&mut state.draft.address)
                                                .hint_text("host.example.com[:port]")
                                                .desired_width((form_width - 160.0).max(80.0)),
                                        );
                                        ui.end_row();

                                        ui.label("User name:");
                                        ui.add(
                                            egui::TextEdit::singleline(&mut state.draft.username)
                                                .hint_text("DOMAIN\\user or user@domain")
                                                .desired_width((form_width - 160.0).max(80.0)),
                                        );
                                        ui.end_row();

                                        ui.label("Password:");
                                        ui.add(
                                            egui::TextEdit::singleline(&mut state.draft.password)
                                                .password(!state.draft.remember_password)
                                                .desired_width((form_width - 160.0).max(80.0)),
                                        );
                                        ui.end_row();

                                        ui.label("Remember:");
                                        ui.horizontal(|ui| {
                                            ui.checkbox(
                                                &mut state.draft.remember_password,
                                                "save password (encrypted vault)",
                                            );
                                        });
                                        ui.end_row();

                                        ui.label("Vault:");
                                        ui.horizontal(|ui| {
                                            if vault.is_unlocked() {
                                                let stored = vault.stored_names().count();
                                                ui.label(
                                                    egui::RichText::new(match stored {
                                                        0 => "unlocked, no passwords stored"
                                                            .to_owned(),
                                                        1 => {
                                                            "unlocked, 1 password stored".to_owned()
                                                        }
                                                        n => format!(
                                                            "unlocked, {n} passwords stored"
                                                        ),
                                                    })
                                                    .weak(),
                                                );
                                                if ui
                                                    .button("Lock")
                                                    .on_hover_text(
                                                        "Forget the unlocked passphrase \
                                                     until you re-enter it",
                                                    )
                                                    .clicked()
                                                {
                                                    vault.lock();
                                                }
                                            } else if vault::Vault::exists() {
                                                ui.label(egui::RichText::new("locked").weak());
                                            } else {
                                                ui.label(egui::RichText::new("not set up").weak());
                                            }
                                        });
                                        ui.end_row();
                                    });
                                ui.collapsing("Master passphrase", |ui| {
                                    ui.label(
                                        egui::RichText::new(
                                            "Protects remembered passwords at rest. \
                                         Enter it once per app start.",
                                        )
                                        .weak()
                                        .small(),
                                    );
                                    if !vault.is_unlocked() {
                                        ui.horizontal(|ui| {
                                            ui.label("Passphrase:");
                                            ui.add(
                                                egui::TextEdit::singleline(
                                                    &mut state.master_passphrase.0,
                                                )
                                                .password(true)
                                                .hint_text(if vault::Vault::exists() {
                                                    "existing passphrase"
                                                } else {
                                                    "new passphrase (min 8 chars)"
                                                })
                                                .desired_width(
                                                    (ui.available_width() - 150.0).max(80.0),
                                                ),
                                            );
                                            let label = if vault::Vault::exists() {
                                                "Unlock"
                                            } else {
                                                "Create"
                                            };
                                            if ui.button(label).clicked() {
                                                vault_unlock =
                                                    Some(state.master_passphrase.0.clone());
                                            }
                                        });
                                        if !vault::Vault::exists() {
                                            ui.horizontal(|ui| {
                                                ui.label("Repeat:");
                                                ui.add(
                                                    egui::TextEdit::singleline(
                                                        &mut state.master_passphrase.1,
                                                    )
                                                    .password(true)
                                                    .desired_width(
                                                        (ui.available_width() - 150.0).max(80.0),
                                                    ),
                                                );
                                            });
                                        } else if vault.stored_names().count() > 0 {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "{} password(s) in the vault, locked",
                                                    vault.stored_names().count()
                                                ))
                                                .weak()
                                                .small(),
                                            );
                                        }
                                    } else if ui.button("Lock vault").clicked() {
                                        vault.lock();
                                        state.master_passphrase.0.clear();
                                        state.master_passphrase.1.clear();
                                    }
                                    if let Some(error) = &state.vault_error {
                                        ui.label(
                                            egui::RichText::new(error)
                                                .color(crate::theme::DANGER)
                                                .small(),
                                        );
                                    }
                                });

                                ui.add_space(4.0);
                                ui.collapsing("Display", |ui| {
                                    ui.horizontal(|ui| {
                                        let mut fit =
                                            matches!(state.draft.desktop, DesktopMode::FitWindow);
                                        ui.checkbox(&mut fit, "Fit window (dynamic resize)");
                                        state.draft.desktop = if fit {
                                            DesktopMode::FitWindow
                                        } else {
                                            match state.draft.desktop {
                                                DesktopMode::Fixed { width, height } => {
                                                    DesktopMode::Fixed { width, height }
                                                }
                                                DesktopMode::FitWindow => DesktopMode::Fixed {
                                                    width: 1920,
                                                    height: 1080,
                                                },
                                            }
                                        };
                                        if let DesktopMode::Fixed { width, height } =
                                            &mut state.draft.desktop
                                        {
                                            ui.add(
                                                egui::DragValue::new(width)
                                                    .range(640..=7680)
                                                    .suffix(" ×"),
                                            );
                                            ui.add(egui::DragValue::new(height).range(480..=4320));
                                        }
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Color depth:");
                                        let depth = &mut state.draft.color_depth;
                                        egui::ComboBox::from_id_salt("depth")
                                            .selected_text(format!("{depth} bpp"))
                                            .show_ui(ui, |ui| {
                                                ui.selectable_value(
                                                    depth,
                                                    16,
                                                    "16 bpp (High Color)",
                                                );
                                                ui.selectable_value(
                                                    depth,
                                                    24,
                                                    "24 bpp (True Color)",
                                                );
                                                ui.selectable_value(depth, 32, "32 bpp (Highest)");
                                            });
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label("Remote monitors:");
                                        ui.add(
                                            egui::Slider::new(
                                                &mut state.draft.monitor_count,
                                                1..=4,
                                            )
                                            .text("screens"),
                                        );
                                    });
                                });

                                ui.collapsing("Advanced", |ui| {
                                    ui.checkbox(
                                        &mut state.draft.credssp,
                                        "Network Level Authentication (NLA / CredSSP)",
                                    );
                                    ui.checkbox(
                                        &mut state.draft.clipboard,
                                        "Clipboard redirection",
                                    );
                                    ui.checkbox(
                                        &mut state.draft.validate_tls,
                                        "Validate TLS certificate (strict)",
                                    );
                                    ui.checkbox(&mut state.draft.compression, "Compression");
                                    ui.checkbox(
                                        &mut state.draft.administrative_session,
                                        "Connect to admin session (/admin)",
                                    );
                                    ui.horizontal(|ui| {
                                        ui.label("Shared folder:");
                                        let mut share =
                                            state.draft.share_folder.clone().unwrap_or_default();
                                        ui.add(
                                            egui::TextEdit::singleline(&mut share)
                                                .hint_text("$HOME (empty = disabled)")
                                                .desired_width(
                                                    (ui.available_width() - 130.0).max(100.0),
                                                ),
                                        );
                                        state.draft.share_folder =
                                            (!share.trim().is_empty()).then_some(share);
                                    });
                                    #[cfg(feature = "audio")]
                                    {
                                        ui.checkbox(
                                            &mut state.draft.audio,
                                            "Remote audio playback",
                                        );
                                        ui.checkbox(
                                            &mut state.draft.audio_capture,
                                            "Audio capture (microphone)",
                                        );
                                    }
                                    #[cfg(not(feature = "audio"))]
                                    {
                                        ui.weak(
                                    "Audio: disabled in this build (rebuild with --features audio)",
                                );
                                    }
                                    ui.add_space(4.0);
                                    ui.label("RemoteApp (empty = full desktop):");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut state.draft.remote_app)
                                            .hint_text(r"C:\Windows\System32\notepad.exe")
                                            .desired_width(ui.available_width().min(380.0)),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut state.draft.remote_app_dir)
                                            .hint_text("Working directory (optional)")
                                            .desired_width(ui.available_width().min(380.0)),
                                    );
                                    ui.add_space(4.0);
                                    ui.label("Experience:");
                                    let perf = &mut state.draft.perf;
                                    ui.checkbox(&mut perf.wallpaper, "Desktop wallpaper");
                                    ui.checkbox(&mut perf.font_smoothing, "Font smoothing");
                                    ui.checkbox(
                                        &mut perf.window_dragging,
                                        "Show window contents while dragging",
                                    );
                                    ui.checkbox(&mut perf.menu_animations, "Menu animations");
                                    ui.checkbox(&mut perf.themes, "Visual styles (themes)");
                                    ui.checkbox(&mut perf.cursor_settings, "Cursor shadows");
                                    ui.add_space(4.0);
                                    ui.label("RD Gateway (optional):");
                                    let gw =
                                        state.draft.gateway.get_or_insert_with(Default::default);
                                    ui.add(
                                        egui::TextEdit::singleline(&mut gw.address)
                                            .hint_text("rdg.example.com:443 (empty = disabled)")
                                            .desired_width(ui.available_width().min(380.0)),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut gw.username)
                                            .hint_text("gateway user")
                                            .desired_width(ui.available_width().min(380.0)),
                                    );
                                    ui.add(
                                        egui::TextEdit::singleline(&mut gw.password)
                                            .password(true)
                                            .hint_text("gateway password")
                                            .desired_width(ui.available_width().min(380.0)),
                                    );
                                    if gw.address.is_empty() && gw.username.is_empty() {
                                        state.draft.gateway = None;
                                    }
                                    ui.add_space(4.0);
                                    ui.horizontal(|ui| {
                                        ui.label("Auto-reconnect attempts:");
                                        ui.add(
                                            egui::DragValue::new(
                                                &mut state.draft.reconnect_attempts,
                                            )
                                            .range(0..=20),
                                        );
                                    });
                                });
                                ui.add_space(8.0);
                            });
                        });

                    // Fixed footer: rendered *after* the scroll region, in a
                    // bounded strip so it always stays inside the window.
                    if let Some(error) = &state.error {
                        ui.colored_label(crate::theme::DANGER, error);
                        ui.add_space(4.0);
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 56.0),
                        egui::Layout::left_to_right(egui::Align::Min),
                        |ui| {
                            ui.horizontal(|ui| {
                                if ui.add(crate::theme::primary_button("Connect")).clicked()
                                    && !state.draft.address.trim().is_empty()
                                {
                                    let mut p = state.draft.clone();
                                    let remembered = p.remember_password;
                                    if p.name.is_empty() {
                                        p.name = p
                                            .address
                                            .split(':')
                                            .next()
                                            .unwrap_or(&p.address)
                                            .to_owned();
                                    }
                                    // Rehydrate remembered passwords from the
                                    // vault (in-memory draft stays clear across
                                    // frames by design).
                                    if remembered && let Ok(Some(stored)) = vault.password(&p.name)
                                    {
                                        p.password = stored;
                                    }
                                    if remembered
                                        && let Some(gateway) = &mut p.gateway
                                        && let Ok(Some(stored)) =
                                            vault.password(&format!("{} / gateway", p.name))
                                    {
                                        gateway.password = stored;
                                    }
                                    if !p.remember_password && p.password.is_empty() {
                                        p.password.clear();
                                    }
                                    *pending = Some((p, false));
                                }
                                if ui.button("Open .rdp…").clicked()
                                    && let Some(path) = rfd::FileDialog::new()
                                        .add_filter("RDP files", &["rdp"])
                                        .pick_file()
                                {
                                    match load_rdp_profile(&path) {
                                        Ok(mut p) => {
                                            p.password.clear();
                                            if p.name.is_empty() {
                                                p.name = p
                                                    .address
                                                    .split(':')
                                                    .next()
                                                    .unwrap_or(&p.address)
                                                    .to_owned();
                                            }
                                            if p.remember_password
                                                && let Ok(Some(stored)) = vault.password(&p.name)
                                            {
                                                p.password = stored;
                                            }
                                            *pending = Some((p, false));
                                        }
                                        Err(error) => state.error = Some(error.to_string()),
                                    }
                                }
                            });
                        },
                    );
                });
            },
        );

        // Apply vault unlock/create outside the borrow of `profiles`.
        if let Some(passphrase) = vault_unlock {
            let exists = vault::Vault::exists();
            let result = if !exists && state.master_passphrase.0 != state.master_passphrase.1 {
                Err(anyhow::anyhow!("passphrases do not match"))
            } else if !exists && state.master_passphrase.0.len() < vault::MIN_PASSPHRASE_LEN {
                Err(anyhow::anyhow!(
                    "passphrase must be at least {} characters",
                    vault::MIN_PASSPHRASE_LEN
                ))
            } else {
                vault.unlock(&passphrase)
            };
            match result {
                Ok(()) => {
                    state.vault_error = None;
                    state.master_passphrase.0.clear();
                    state.master_passphrase.1.clear();
                    // Migrate any plaintext leftovers now that the key works.
                    let migrated = vault.migrate_plaintext(profiles);
                    if migrated > 0 {
                        tracing::info!(
                            profiles = migrated,
                            "moved plaintext passwords into the vault"
                        );
                        if let Err(error) = crate::profile::save_profiles(profiles) {
                            tracing::warn!(
                                %error,
                                "could not rewrite profiles.json after migration"
                            );
                        }
                    }
                }
                Err(error) => state.vault_error = Some(error.to_string()),
            }
        }
    }
}

impl SessionState {
    fn connected(&self) -> bool {
        matches!(self.status, Status::Connected)
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.handle.events.try_recv() {
            match event {
                SessionEvent::Connected => {
                    self.status = Status::Connected;
                    if self.session_up_since.is_none() {
                        self.session_up_since = Some(std::time::Instant::now());
                    }
                }
                SessionEvent::LoginComplete => {
                    self.note = Some("Logged in".into());
                    self.session_up_since = Some(std::time::Instant::now());
                }
                SessionEvent::Note { text } => self.note = Some(text),
                SessionEvent::Failed { error } => {
                    self.status = Status::Failed;
                    self.error = Some(error);
                }
                SessionEvent::Terminated { reason } => {
                    self.status = Status::Terminated;
                    self.error = Some(reason);
                }
                SessionEvent::AutoReconnecting { attempt, maximum } => {
                    self.note = Some(format!("Reconnecting ({attempt}/{maximum})…"));
                }
                SessionEvent::AutoReconnected => {
                    self.note = Some("Reconnected".into());
                    self.status = Status::Connected;
                }
                SessionEvent::ResizeFallback { reason } => {
                    // Reactivation timeouts are transient (3s deadline while the
                    // host is busy); retry a few times before alarming the user.
                    self.resize_failures += 1;
                    if matches!(
                        reason,
                        ironrdp::client::rdp::DisplayResizeFallbackReason::ReactivationTimedOut
                    ) && self.resize_failures < 3
                    {
                        self.resize_retry_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_secs(4));
                    } else if self.resize_failures >= 3 {
                        self.note = Some(format!("Dynamic resize unavailable: {reason:?}"));
                    }
                }
                SessionEvent::Pointer(event) => match event {
                    ironrdp::client::rdp::RdpOutputEvent::PointerDefault => {
                        self.pointer = PointerView::Default;
                    }
                    ironrdp::client::rdp::RdpOutputEvent::PointerHidden => {
                        self.pointer = PointerView::Hidden;
                    }
                    ironrdp::client::rdp::RdpOutputEvent::PointerBitmap(pointer) => {
                        // Engine decodes to premultiplied RGBA bytes.
                        self.pointer = PointerView::Custom(Arc::new(egui::CustomCursorImage {
                            size: [pointer.width, pointer.height],
                            hotspot: [pointer.hotspot_x, pointer.hotspot_y],
                            rgba: Arc::from(pointer.bitmap_data.clone()),
                        }));
                    }
                    _ => {}
                },
            }
        }

        if let Some(frame) = self.handle.frame.snapshot_if_dirty() {
            if frame.width > 0 && frame.height > 0 {
                if let Ok(dir) = std::env::var("RDP_DUMP_FRAME") {
                    dump_frame(&dir, &frame);
                }
                let image = egui::ColorImage {
                    size: [usize::from(frame.width), usize::from(frame.height)],
                    source_size: egui::Vec2::new(f32::from(frame.width), f32::from(frame.height)),
                    pixels: frame.pixels,
                };
                match self.texture.as_mut() {
                    Some(texture) => texture.set(image, egui::TextureOptions::LINEAR),
                    None => {
                        self.texture = Some(ctx.load_texture(
                            "rdp-frame",
                            image,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
            }
            ctx.request_repaint();
        }

        // Apply cursor shape platform-wide, only when it changed (a per-frame
        // set_cursor_image(None) makes winit recreate the cursor every frame).
        let current = std::mem::discriminant(&self.pointer);
        let applied = self.applied_pointer != Some(current);
        if applied {
            match &self.pointer {
                PointerView::Custom(image) => ctx.set_cursor_image(Some((**image).clone())),
                PointerView::Default | PointerView::Hidden => ctx.set_cursor_image(None),
            }
            self.applied_pointer = Some(current);
        }
    }

    fn drain_input(&mut self, ctx: &egui::Context) {
        // Dynamic resize (FitWindow): debounce 250ms after the last size change.
        if matches!(self.profile.desktop, DesktopMode::FitWindow)
            && let Some(avail) = self.last_available
            && self
                .session_up_since
                .is_some_and(|t| t.elapsed() >= std::time::Duration::from_secs(5))
            && self.resize_retry_at.take().is_none_or(|at| {
                if at.elapsed() >= std::time::Duration::ZERO {
                    self.last_resize = None; // force a fresh resize send
                }
                true
            })
        {
            let desired = if self.profile.monitor_count > 1 {
                session::desktop_size(&self.profile)
            } else {
                (
                    avail.x.round().clamp(320.0, 8192.0) as u16,
                    avail.y.round().clamp(240.0, 8192.0) as u16,
                )
            };
            match self.last_resize {
                Some((w, h, at))
                    if (w, h) == desired
                        && at.elapsed() < std::time::Duration::from_millis(250) =>
                {
                    ctx.request_repaint_after(std::time::Duration::from_millis(100));
                }
                Some((w, h, at))
                    if (w, h) == desired
                        && at.elapsed() >= std::time::Duration::from_millis(250) =>
                {
                    if self.profile.monitor_count > 1 {
                        let count = u32::from(self.profile.monitor_count.clamp(1, 4));
                        if let Ok(layout) = session::monitor_layout(
                            self.profile.monitor_count,
                            u32::from(w) / count,
                            u32::from(h),
                        ) {
                            let _ = self.handle.input.try_send(
                                ironrdp::client::rdp::RdpInputEvent::MonitorLayout(layout),
                            );
                        }
                    } else {
                        let _ = self.handle.input.try_send(
                            ironrdp::client::rdp::RdpInputEvent::Resize {
                                width: w,
                                height: h,
                                scale_factor: 100,
                                physical_size: None,
                            },
                        );
                    }
                    // Mark as sent so we don't re-send until the size changes again.
                    self.last_resize = Some((w, h, at + std::time::Duration::from_secs(3600)));
                }
                _ => {
                    self.last_resize = Some((desired.0, desired.1, std::time::Instant::now()));
                }
            }
        }

        let (events, dropped_files) =
            ctx.input(|i| (i.raw.events.clone(), i.raw.dropped_files.clone()));
        if !dropped_files.is_empty() {
            if let Some(clipboard) = &self.handle._clipboard {
                clipboard.offer_files(
                    dropped_files
                        .iter()
                        .map(|file| file.path().to_owned())
                        .collect(),
                );
            } else {
                tracing::debug!("Ignoring dropped files because clipboard redirection is disabled");
            }
        }
        if events.is_empty() {
            return;
        }

        let surface = self.surface_rect;
        let frame_size = self.handle.frame.inner_size();
        // Horizontal slice of the framebuffer that the surface shows (full
        // range when viewing all monitors).
        let count = u32::from(self.profile.monitor_count.clamp(1, 4));
        let view = match self.viewed_monitor {
            Some(index) if count > 1 => {
                let w = 1.0 / count as f32;
                (f32::from(index) * w, (f32::from(index) + 1.0) * w)
            }
            _ => (0.0, 1.0),
        };
        let mut ops: Vec<ironrdp::input::Operation> = Vec::new();
        let mut toggle_fullscreen = false;

        for event in events {
            match event {
                // egui-winit's shortcut interception turns Ctrl+C/X/V into
                // commands and drops the key events. When the pointer is over
                // the remote surface, the user meant them for the remote:
                // re-inject the C/X/V scancodes (modifiers are refreshed by
                // the modifiers-diff that precedes the command event).
                egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_)
                    if surface
                        .zip(self.last_pos)
                        .map(|(rect, pos)| rect.contains(pos))
                        .unwrap_or(false) =>
                {
                    if let Some(modifiers) = self.mods.last() {
                        for (sc, down) in self.mods.diff(modifiers) {
                            ops.push(op_key(sc, down));
                        }
                    }
                    let sc = match event {
                        egui::Event::Cut => crate::keymap::map_key(egui::Key::X),
                        egui::Event::Copy => crate::keymap::map_key(egui::Key::C),
                        egui::Event::Paste(_) => crate::keymap::map_key(egui::Key::V),
                        _ => unreachable!(),
                    };
                    // Press + release so the remote sees the full keystroke.
                    if let Some(sc) = sc {
                        ops.push(op_key(sc, true));
                        ops.push(op_key(sc, false));
                    }
                }
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    modifiers,
                    ..
                } => {
                    for (sc, down) in self.mods.diff(modifiers) {
                        ops.push(op_key(sc, down));
                    }
                    // F11 stays local (fullscreen toggle), like mstsc.
                    if key == egui::Key::F11 {
                        if pressed {
                            toggle_fullscreen = true;
                        }
                        continue;
                    }
                    let phys = physical_key.unwrap_or(key);
                    if let Some(sc) = crate::keymap::map_key(phys) {
                        ops.push(op_key(sc, pressed));
                    }
                }
                egui::Event::ModifiersChanged(modifiers) => {
                    for (sc, down) in self.mods.diff(modifiers) {
                        ops.push(op_key(sc, down));
                    }
                }
                egui::Event::PointerMoved(pos) => {
                    self.last_pos = Some(pos);
                    if let (Some(rect), false) = (surface, frame_size == (0, 0))
                        && rect.contains(pos)
                    {
                        ops.push(Self::move_op(pos, rect, frame_size, view));
                    }
                }
                egui::Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } => {
                    self.last_pos = Some(pos);
                    for (sc, down) in self.mods.diff(modifiers) {
                        ops.push(op_key(sc, down));
                    }
                    if let Some(rect) = surface
                        && rect.contains(pos)
                    {
                        let remote = match button {
                            egui::PointerButton::Primary => ironrdp::input::MouseButton::Left,
                            egui::PointerButton::Secondary => ironrdp::input::MouseButton::Right,
                            egui::PointerButton::Middle => ironrdp::input::MouseButton::Middle,
                            _ => continue,
                        };
                        if frame_size != (0, 0) {
                            ops.push(Self::move_op(pos, rect, frame_size, view));
                        }
                        ops.push(if pressed {
                            ironrdp::input::Operation::MouseButtonPressed(remote)
                        } else {
                            ironrdp::input::Operation::MouseButtonReleased(remote)
                        });
                    }
                }
                egui::Event::MouseWheel {
                    unit,
                    delta,
                    modifiers,
                    ..
                } => {
                    for (sc, down) in self.mods.diff(modifiers) {
                        ops.push(op_key(sc, down));
                    }
                    let over_surface = self
                        .last_pos
                        .zip(surface)
                        .map(|(p, r)| r.contains(p))
                        .unwrap_or(false);
                    if over_surface {
                        // RDP wheel unit = 1/120 detent; egui point ≈ 1/2.4 detent, line = 1 detent.
                        let factor = match unit {
                            egui::MouseWheelUnit::Point => 2.4,
                            egui::MouseWheelUnit::Line => 120.0,
                            egui::MouseWheelUnit::Page => 2880.0,
                        };
                        let vertical = clamp_i16(delta.y * factor);
                        let horizontal = clamp_i16(delta.x * factor);
                        if vertical != 0 {
                            ops.push(ironrdp::input::Operation::WheelRotations(
                                ironrdp::input::WheelRotations {
                                    is_vertical: true,
                                    rotation_units: vertical,
                                },
                            ));
                        }
                        if horizontal != 0 {
                            ops.push(ironrdp::input::Operation::WheelRotations(
                                ironrdp::input::WheelRotations {
                                    is_vertical: false,
                                    rotation_units: horizontal,
                                },
                            ));
                        }
                    }
                }
                egui::Event::PointerGone => {
                    // Pointer left the window: release remote buttons to avoid stuck clicks.
                    for button in [
                        ironrdp::input::MouseButton::Left,
                        ironrdp::input::MouseButton::Middle,
                        ironrdp::input::MouseButton::Right,
                    ] {
                        ops.push(ironrdp::input::Operation::MouseButtonReleased(button));
                    }
                }
                egui::Event::WindowFocused(false) => {
                    // Release everything remote-side to avoid stuck modifiers/buttons.
                    for (sc, _) in self.mods.release_all() {
                        ops.push(op_key(sc, false));
                    }
                    for button in [
                        ironrdp::input::MouseButton::Left,
                        ironrdp::input::MouseButton::Middle,
                        ironrdp::input::MouseButton::Right,
                    ] {
                        ops.push(ironrdp::input::Operation::MouseButtonReleased(button));
                    }
                }
                _ => {}
            }
        }

        if toggle_fullscreen {
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!self.fullscreen));
            self.fullscreen = !self.fullscreen;
        }

        if ops.is_empty() {
            return;
        }
        let batch = self.input_db.apply(ops);
        let _ = self
            .handle
            .input
            .try_send(ironrdp::client::rdp::RdpInputEvent::FastPath(batch));
    }

    fn move_op(
        pos: egui::Pos2,
        rect: egui::Rect,
        (w, h): (u16, u16),
        (view_start, view_end): (f32, f32),
    ) -> ironrdp::input::Operation {
        let local = pos - rect.min;
        // The surface shows the framebuffer slice [view_start, view_end];
        // remap the click position into full-framebuffer coordinates.
        let x = (view_start + local.x / rect.width() * (view_end - view_start)) * f32::from(w);
        ironrdp::input::Operation::MouseMove(ironrdp::input::MousePosition {
            x: x.round().clamp(0.0, f32::from(w) - 1.0) as u16,
            y: (local.y * f32::from(h) / rect.height())
                .round()
                .clamp(0.0, f32::from(h) - 1.0) as u16,
        })
    }

    fn draw(
        &mut self,
        ctx: &egui::Context,
        ui: &mut egui::Ui,
        back: &mut Option<Option<String>>,
        restart: &mut Option<Profile>,
        leave_fullscreen: &mut bool,
    ) {
        let mut want_fullscreen = self.fullscreen;
        let mut disconnect = false;
        if self.fullscreen_pending {
            self.fullscreen_pending = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
        }
        let reported = ctx.input(|i| i.viewport().fullscreen);
        if self.reported_fullscreen != reported {
            tracing::debug!("compositor fullscreen state: {reported:?}");
            self.reported_fullscreen = reported;
        }

        // Surface fills the entire area; the connection bar overlays it and
        // must not affect layout (show/hide would otherwise re-trigger
        // dynamic resize).
        let available = ui.available_size();
        self.last_available = Some(available);
        match &self.texture {
            Some(texture) => {
                let tex_size = egui::Vec2::new(texture.size()[0] as f32, texture.size()[1] as f32);
                if tex_size.x > 0.0 && tex_size.y > 0.0 {
                    // Which slice of the framebuffer this surface shows: all
                    // monitors side by side, or only the selected monitor.
                    // Framebuffer geometry is count * monitor width when a
                    // multi-monitor layout was requested.
                    let count = u32::from(self.profile.monitor_count.clamp(1, 4));
                    let uv = match self.viewed_monitor {
                        Some(index) if count > 1 => {
                            let w = 1.0 / count as f32;
                            (
                                egui::Rect::from_min_max(
                                    egui::Pos2::new(f32::from(index) * w, 0.0),
                                    egui::Pos2::new((f32::from(index) + 1.0) * w, 1.0),
                                ),
                                tex_size.x / count as f32,
                                tex_size.y,
                            )
                        }
                        _ => (
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                            tex_size.x,
                            tex_size.y,
                        ),
                    };
                    let view_size = egui::Vec2::new(uv.1, uv.2);
                    let scale = if self.fit {
                        (available.x / view_size.x)
                            .min(available.y / view_size.y)
                            .min(1.0)
                    } else {
                        1.0
                    };
                    let size = view_size * scale;
                    let x_off = ((available.x - size.x) / 2.0).max(0.0);
                    let y_off = ((available.y - size.y) / 2.0).max(0.0);
                    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(
                        egui::Rect::from_min_size(ui.cursor().min + egui::vec2(x_off, y_off), size),
                    ));
                    let (rect, response) =
                        child.allocate_exact_size(size, egui::Sense::click_and_drag());
                    ui.painter().image(
                        texture.id(),
                        rect,
                        // Passing pixel extents instead of the normalized UV
                        // window clamps sampling to the edge pixel and the
                        // surface renders as one flat color.
                        uv.0,
                        egui::Color32::WHITE,
                    );
                    if matches!(self.pointer, PointerView::Hidden) {
                        response.on_hover_cursor(egui::CursorIcon::None);
                    }
                    self.surface_rect = Some(rect);
                }
            }
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("Waiting for first frame…").weak());
                });
            }
        }

        // Floating connection bar: overlays the surface, doesn't affect layout.
        // Bar appears while the pointer is near the top edge (mstsc behavior).
        let pointer_y = ctx.input(|i| i.pointer.latest_pos().map(|p| p.y).unwrap_or(f32::MAX));
        let show_bar = !self.fullscreen || pointer_y < 48.0;
        if show_bar {
            egui::Area::new(egui::Id::unique("connbar"))
                .anchor(egui::Align2::CENTER_TOP, [0.0, 0.0])
                .order(egui::Order::Foreground)
                .show(ctx, |ui| {
                    egui::Frame::default()
                        .fill(crate::theme::BG_PANEL.gamma_multiply(0.93))
                        .stroke(egui::Stroke::new(1.0, crate::theme::STROKE))
                        .inner_margin(egui::Margin::symmetric(8, 6))
                        .corner_radius(egui::CornerRadius {
                            nw: 0,
                            ne: 0,
                            sw: 8,
                            se: 8,
                        })
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (w, h) = self.handle.frame.inner_size();
                                ui.label(
                                    egui::RichText::new(format!("{} — {w}×{h}", self.profile.name))
                                        .strong(),
                                );
                                if let Some(note) = &self.note {
                                    ui.weak(note);
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("Disconnect").clicked() {
                                            disconnect = true;
                                        }
                                        if ui.button("Ctrl+Alt+Del").clicked() {
                                            self.send_ctrl_alt_del();
                                        }
                                        if ui
                                            .selectable_label(self.fullscreen, "Fullscreen")
                                            .clicked()
                                        {
                                            want_fullscreen = !want_fullscreen;
                                        }
                                        if ui.selectable_label(self.fit, "Fit").clicked() {
                                            self.fit = !self.fit;
                                        }
                                        // Monitor picker: one tile per configured
                                        // remote monitor plus "All". Only relevant
                                        // when a multi-monitor layout exists.
                                        if self.profile.monitor_count > 1 {
                                            ui.separator();
                                            if ui
                                                .selectable_label(
                                                    self.viewed_monitor.is_none(),
                                                    "All screens",
                                                )
                                                .clicked()
                                            {
                                                self.viewed_monitor = None;
                                            }
                                            for index in 0..self.profile.monitor_count.min(4) {
                                                let selected = self.viewed_monitor == Some(index);
                                                if ui
                                                    .selectable_label(
                                                        selected,
                                                        format!("Screen {}", index + 1),
                                                    )
                                                    .clicked()
                                                {
                                                    self.viewed_monitor = Some(index);
                                                }
                                            }
                                        }
                                    },
                                );
                            });
                        });
                });
        }

        if disconnect {
            self.handle.input.request_graceful_close();
            *leave_fullscreen = self.fullscreen;
            *back = Some(Some(String::new()));
        }
        if want_fullscreen != self.fullscreen {
            self.fullscreen = want_fullscreen;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        }

        // Status overlay while not connected.
        if !self.connected() {
            egui::Area::new(egui::Id::unique("rdp-overlay"))
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| match &self.status {
                        Status::Connecting => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.label("Connecting…");
                            });
                        }
                        Status::Connected => {}
                        Status::Failed | Status::Terminated => {
                            ui.label(
                                egui::RichText::new(if matches!(self.status, Status::Failed) {
                                    "Connection failed"
                                } else {
                                    "Session ended"
                                })
                                .strong(),
                            );
                            if let Some(error) = &self.error {
                                ui.label(error);
                            }
                            ui.horizontal(|ui| {
                                if ui.button("Retry").clicked() {
                                    *restart = Some(self.profile.clone());
                                }
                                if ui.button("Back").clicked() {
                                    *leave_fullscreen = self.fullscreen;
                                    *back = Some(None);
                                }
                                if ui.button("Exit").clicked() {
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            });
                        }
                    });
                });
        }
    }

    fn send_ctrl_alt_del(&mut self) {
        use ironrdp::input::{Operation, Scancode};
        let ctrl = Scancode::from_u8(false, 0x1D);
        let alt = Scancode::from_u8(false, 0x38);
        let del = Scancode::from_u8(true, 0x53);
        let ops = [
            Operation::KeyPressed(ctrl),
            Operation::KeyPressed(alt),
            Operation::KeyPressed(del),
            Operation::KeyReleased(del),
            Operation::KeyReleased(alt),
            Operation::KeyReleased(ctrl),
        ];
        let batch = self.input_db.apply(ops);
        let _ = self
            .handle
            .input
            .try_send(ironrdp::client::rdp::RdpInputEvent::FastPath(batch));
    }
}

fn op_key(sc: ironrdp::input::Scancode, down: bool) -> ironrdp::input::Operation {
    if down {
        ironrdp::input::Operation::KeyPressed(sc)
    } else {
        ironrdp::input::Operation::KeyReleased(sc)
    }
}

fn clamp_i16(value: f32) -> i16 {
    value
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// Diagnostic: dump framebuffer as binary PPM (P6), throttled to ~1/s, keeping 5.
fn dump_frame(dir: &str, frame: &crate::session::FrameBuf) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LAST_DUMP_MS: AtomicU64 = AtomicU64::new(0);
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    if now_ms.saturating_sub(LAST_DUMP_MS.load(Ordering::Relaxed)) < 1000 {
        return;
    }
    LAST_DUMP_MS.store(now_ms, Ordering::Relaxed);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    if seq >= 12 {
        return; // cap total dumps per run
    }
    let path = format!(
        "{dir}/frame_{seq:03}_{:>4}x{:<4}.ppm",
        frame.width, frame.height
    );
    let mut bytes = format!("P6\n{} {}\n255\n", frame.width, frame.height).into_bytes();
    for px in &frame.pixels {
        bytes.extend_from_slice(&[px.r(), px.g(), px.b()]);
    }
    let _ = std::fs::write(&path, &bytes);
}

/// Loads an .rdp file into a Profile (UTF-16LE with BOM or UTF-8).
fn load_rdp_profile(path: &std::path::Path) -> anyhow::Result<Profile> {
    let bytes = std::fs::read(path)?;
    let text = if bytes.starts_with(&[0xFF, 0xFE]) {
        let utf16: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16_lossy(&utf16)
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };

    let parsed = ironrdp_rdpfile::parse(&text);
    let properties = parsed.properties;

    let mut p = Profile::new(String::new());
    if let Some(addr) = properties.get::<&str>("full address") {
        if !addr.is_empty() {
            p.address = addr.to_owned();
            p.name = p.address.split(':').next().unwrap_or(&p.address).to_owned();
        }
    }
    if p.address.is_empty() {
        anyhow::bail!("no 'full address' in file");
    }
    if let Some(w) = properties.get::<i32>("desktopwidth")
        && let Ok(w) = u16::try_from(w)
        && let DesktopMode::Fixed { width, .. } = &mut p.desktop
    {
        *width = w;
    }
    if let Some(h) = properties.get::<i32>("desktopheight")
        && let Ok(h) = u16::try_from(h)
        && let DesktopMode::Fixed { height, .. } = &mut p.desktop
    {
        *height = h;
    }
    let user_owned = properties
        .get::<&str>("username")
        .unwrap_or_default()
        .to_owned();
    let mut user = user_owned;
    if let Some(domain) = properties.get::<&str>("domain")
        && !domain.is_empty()
        && !user.contains('\\')
    {
        user = format!("{domain}\\{user}");
    }
    p.username = user;
    if let Some(shell) = properties.get::<&str>("alternate shell") {
        p.remote_app = shell.to_owned();
    }
    if let Some(dir) = properties.get::<&str>("shell working directory") {
        p.remote_app_dir = dir.to_owned();
    }
    if properties.get::<i32>("administrative session") == Some(1) {
        p.administrative_session = true;
    }
    Ok(p)
}
