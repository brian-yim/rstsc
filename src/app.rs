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
    /// Vault gate: the master passphrase is entered before anything else.
    Lock(Box<LockState>),
    Manager(Box<ManagerState>),
    Session(Box<SessionState>),
}

/// `LockOutcome` is what [`App::draw_lock`] asks to happen; applied outside
/// the frame's borrows.
enum LockOutcome {
    Unlock { passphrase: String, confirm: String },
    /// First run with no vault: use the app without stored passwords.
    Skip,
}

/// Master passphrase entry on the lock screen.
struct LockState {
    /// True when `secrets.vault` does not exist yet: the screen creates the
    /// master passphrase instead of verifying one, and offers a skip.
    creating: bool,
    passphrase: String,
    confirm: String,
    error: Option<String>,
}

pub struct App {
    screen: Screen,
    profiles: Vec<Profile>,
    /// Deferred CLI launch (`--profile` / `-a` / `-r` / `-f`): held while the
    /// lock screen is up, moved into `pending_launch` once the vault is
    /// unlocked (or immediately when skipping a not-yet-created vault).
    launch_after_unlock: Option<(Profile, bool)>,
    pending_launch: Option<(Profile, bool)>,
    vault: vault::Vault,
    /// Window title last pushed to the compositor (dedup Title commands).
    reported_title: Option<String>,
}

struct ManagerState {
    draft: Profile,
    selected: Option<usize>,
    error: Option<String>,
    /// Password field shows dots until the user asks to review it.
    show_password: bool,
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
    fn hydrate_vault_passwords(vault: &vault::Vault, p: &mut Profile) {
        if let Ok(Some(stored)) = vault.password(&p.name) {
            p.password = stored;
        }
        if let Some(gateway) = &mut p.gateway
            && let Ok(Some(stored)) = vault.password(&format!("{} / gateway", p.name))
        {
            gateway.password = stored;
        }
    }

    /// Profile list for the manager. v2: the vault owns the truth when it is
    /// readable; `profiles.json` is the pre-unlock fallback (CLI launch) and
    /// for locked-vault sessions.
    fn load_profile_list(vault: &vault::Vault) -> (Vec<Profile>, bool) {
        let json_profiles = profile::load_profiles().unwrap_or_default();
        if !vault.is_unlocked() {
            return (json_profiles, false);
        }
        match vault.profiles() {
            Ok(vaulted) if !vaulted.is_empty() => (vaulted, true),
            Ok(_) => {
                // First v2 run over a v1 vault: keep the JSON copy; the
                // seal-on-next-save path fills the vault.
                (json_profiles, false)
            }
            Err(error) => {
                tracing::warn!(%error, "could not read sealed profiles; falling back to profiles.json");
                (json_profiles, false)
            }
        }
    }

    pub fn new(_cc: &eframe::CreationContext<'_>, launch: Launch) -> Self {
        let mut vault = vault::Vault::load().unwrap_or_else(|error| {
            tracing::warn!(%error, "Could not read the password vault; remembered passwords unavailable");
            vault::Vault::fresh()
        });
        // Sealed profiles load only after unlock; pre-unlock the JSON copy
        // is used for CLI address/profile matching.
        let (profiles, _profiles_from_vault) =
            if vault.is_unlocked() {
                Self::load_profile_list(&vault)
            } else {
                (profile::load_profiles().unwrap_or_default(), false)
            };
        // Every launch mode starts at the vault gate; CLI launches resume
        // after the passphrase is in. Refreshing the draft needs the vault
        // (rehydration), so the manager is built after the launch profile.
        let (launch_after_unlock, rdp_error) = match launch {
            Launch::Manager => (None, None),
            Launch::Address(addr, fullscreen) => {
                let mut p = profiles
                    .iter()
                    .find(|p| p.address == addr)
                    .cloned()
                    .unwrap_or_else(|| Profile::new(addr));
                Self::hydrate_vault_passwords(&vault, &mut p);
                (Some((p, fullscreen)), None)
            }
            Launch::Profile(mut p, fullscreen) => {
                Self::hydrate_vault_passwords(&vault, &mut p);
                (Some((p, fullscreen)), None)
            }
            Launch::RdpFile(path, fullscreen) => match load_rdp_profile(&path) {
                Ok(mut p) => {
                    Self::hydrate_vault_passwords(&vault, &mut p);
                    (Some((p, fullscreen)), None)
                }
                Err(error) => (None, Some(format!("{}: {error}", path.display()))),
            },
        };
        let creating = !vault::Vault::exists();
        // Fresh vault, no gate needed: go straight to the manager so the app
        // still works with no vault at all (connect without remembering).
        let screen = if creating {
            let mut draft = launch_after_unlock
                .as_ref()
                .map(|(p, _)| p.clone())
                .unwrap_or_else(|| Profile::new(String::new()));
            draft.password.clear();
            Screen::Manager(Box::new(ManagerState {
                draft,
                selected: None,
                error: rdp_error,
                show_password: false,
            }))
        } else {
            Screen::Lock(Box::new(LockState {
                creating,
                passphrase: String::new(),
                confirm: String::new(),
                error: None,
            }))
        };
        Self {
            screen,
            profiles,
            launch_after_unlock,
            pending_launch: None,
            reported_title: None,
            vault,
        }
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
            _ => self
                .launch_after_unlock
                .take()
                .map(|(p, _)| p)
                .unwrap_or_else(|| Profile::new(String::new())),
        };
        self.screen = Screen::Manager(Box::new(ManagerState {
            draft,
            selected: None,
            error,
            show_password: false,
        }));
    }

    /// Lock-screen action: verify the master passphrase (existing vault) or
    /// create the vault (fresh). On success: scrub any plaintext legacy
    /// passwords, fire the deferred CLI launch, otherwise hand over to the
    /// manager.
    fn apply_unlock(&mut self, passphrase: &str, confirm: &str) -> anyhow::Result<()> {
        if !vault::Vault::exists() && passphrase != confirm {
            anyhow::bail!("passphrases do not match");
        }
        if !vault::Vault::exists() && passphrase.len() < vault::MIN_PASSPHRASE_LEN {
            anyhow::bail!(
                "passphrase must be at least {} characters",
                vault::MIN_PASSPHRASE_LEN
            );
        }
        self.vault.unlock(passphrase)?;
        let migrated = self.vault.migrate_plaintext(&mut self.profiles);
        if migrated > 0 {
            tracing::info!(profiles = migrated, "moved plaintext passwords into the vault");
            if let Err(error) = profile::save_profiles(&self.profiles) {
                tracing::warn!(%error, "could not rewrite profiles.json after migration");
            }
        }
        // v2: sealed profiles are the manager's list once the vault is open
        // (passwords merged by `Vault::profiles`).
        if let Ok(vaulted) = self.vault.profiles() {
            if !vaulted.is_empty() {
                self.profiles = vaulted;
            }
        } else {
            tracing::warn!("vault open but sealed profiles unreadable; keeping profiles.json copy");
        }
        // Move the deferred CLI launch (if any) into the flow, then let the
        // manager take over: it seeds the draft and fires `pending_launch`
        // on the next `logic` tick.
        self.pending_launch = self.launch_after_unlock.take();
        self.back_to_manager(None);
        Ok(())
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
        // egui-winit's clipboard shortcut interception is OFF (see main.rs):
        // forwarded Ctrl+C/X/V reach egui as plain Key events, which TextEdit
        // would type instead of treating as clipboard commands. On the
        // manager screen (no remote surface), synthesize the corresponding
        // egui events. Paste reads the platform clipboard via arboard here —
        // safe because no session mirror can own the clipboard while the
        // manager screen is active.
        if matches!(self.screen, Screen::Manager(_)) {
            let shortcut_keys: Vec<egui::Key> = ctx.input(|i| {
                i.raw
                    .events
                    .iter()
                    .filter_map(|event| {
                        if let egui::Event::Key {
                            key,
                            pressed: true,
                            modifiers:
                                egui::Modifiers {
                                    ctrl: true,
                                    shift: false,
                                    ..
                                },
                            ..
                        } = event
                        {
                            if matches!(key, egui::Key::C | egui::Key::X | egui::Key::V) {
                                Some(*key)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .collect()
            });
            if !shortcut_keys.is_empty() {
                // The plain keys must not also type 'c'/'x'/'v' into a
                // focused field: drop them, then push the real clipboard
                // events.
                ctx.input_mut(|i| {
                    i.raw.events.retain(|event| {
                        !matches!(
                            event,
                            egui::Event::Key {
                                key,
                                pressed: true,
                                modifiers: egui::Modifiers { ctrl: true, .. },
                                ..
                            } if shortcut_keys.contains(key)
                        )
                    });
                });
                for key in shortcut_keys {
                    let event = match key {
                        egui::Key::V => {
                            let text = arboard::Clipboard::new()
                                .ok()
                                .and_then(|mut c| c.get_text().ok())
                                .unwrap_or_default();
                            egui::Event::Paste(text)
                        }
                        egui::Key::X => egui::Event::Cut,
                        _ => egui::Event::Copy,
                    };
                    ctx.input_mut(|i| i.raw.events.push(event));
                }
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let mut back: Option<Option<String>> = None;
        let mut restart: Option<Profile> = None;
        let mut leave_fullscreen = false;

        // Window title: "rstsc — locked" on the vault gate, "<profile or
        // host> — rstsc" in a session, plain "rstsc" in the manager.
        let title = match &self.screen {
            Screen::Lock(_) => "rstsc — locked".to_owned(),
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

        if let Screen::Lock(lock) = &mut self.screen {
            let outcome = App::draw_lock(lock, ui);
            match outcome {
                Some(LockOutcome::Unlock {
                    passphrase,
                    confirm,
                }) => {
                    match self.apply_unlock(&passphrase, &confirm) {
                        Ok(()) => {}
                        Err(error) => {
                            if let Screen::Lock(lock) = &mut self.screen {
                                lock.error = Some(error.to_string());
                                lock.passphrase.clear();
                                lock.confirm.clear();
                            }
                        }
                    }
                    // Zeroize the passphrases no matter how the unlock goes.
                }
                Some(LockOutcome::Skip) => {
                    // Bypass: no vault access at all. The deferred CLI
                    // launch (if any) proceeds, but any vault-rehydrated
                    // password is stripped so nothing decrypts on skip.
                    if let Some((p, _)) = self.launch_after_unlock.as_mut() {
                        p.password.clear();
                        if let Some(gateway) = &mut p.gateway {
                            gateway.password.clear();
                        }
                    }
                    self.back_to_manager(None);
                    // Manager works from the non-vault copy (profiles.json /
                    // CLI arg); no vault reads, no rehydration.
                    for p in self.profiles.iter_mut() {
                        p.password.clear();
                    }
                }
                None => {}
            }
        }
        let mut vault_flow = None;
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
                vault_flow = App::draw_manager(vault, ui, m, profiles, pending_launch);
            });
            if let Some(VaultFlow::SignIn { creating }) = vault_flow {
                self.screen = Screen::Lock(Box::new(LockState {
                    creating,
                    passphrase: String::new(),
                    confirm: String::new(),
                    error: None,
                }));
            }
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

/// Manager-driven vault transitions: the frame collects the request, the
/// caller switches screens after the borrows end.
enum VaultFlow {
    SignIn { creating: bool },
}

impl App {
    /// Vault gate. Returns an outcome for [`App::ui`] to apply.
    fn draw_lock(state: &mut LockState, ui: &mut egui::Ui) -> Option<LockOutcome> {
        let mut outcome = None;
        let available = ui.available_size();
        ui.allocate_ui_with_layout(
            available,
            egui::Layout::top_down(egui::Align::Center),
            |ui| {
                ui.set_min_size(available);
                ui.add_space(ui.available_height() * 0.25);
                let heading = if state.creating {
                    "Set up your vault"
                } else {
                    "rstsc is locked"
                };
                ui.label(egui::RichText::new(heading).strong().size(22.0));
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(if state.creating {
                        "Choose a master passphrase — it protects every saved \
                         password in the vault. Enter it once per app start."
                    } else {
                        "Enter the master passphrase to unlock saved passwords. \
                         You can also skip and connect without one."
                    })
                    .weak(),
                );
                ui.add_space(16.0);

                let wide = (ui.available_width() * 0.5).max(280.0).min(420.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(wide, 0.0),
                    egui::Layout::top_down(egui::Align::Center),
                    |ui| {
                        ui.set_width(wide);
                        ui.label("Master passphrase:");
                        ui.add(
                            egui::TextEdit::singleline(&mut state.passphrase)
                                .password(true)
                                .hint_text(if state.creating {
                                    "new passphrase (min 8 chars)"
                                } else {
                                    "passphrase"
                                })
                                .desired_width(f32::INFINITY),
                        );
                        if state.creating {
                            ui.add_space(4.0);
                            ui.label("Repeat passphrase:");
                            ui.add(
                                egui::TextEdit::singleline(&mut state.confirm)
                                    .password(true)
                                    .hint_text("repeat passphrase")
                                    .desired_width(f32::INFINITY),
                            );
                        }
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            let label = if state.creating { "Create vault" } else { "Unlock" };
                            let submitted = ui
                                .add_sized(
                                    [ui.available_width(), 34.0],
                                    crate::theme::primary_button(label),
                                )
                                .clicked();
                            if submitted {
                                outcome = Some(LockOutcome::Unlock {
                                    passphrase: state.passphrase.clone(),
                                    confirm: state.confirm.clone(),
                                });
                            }
                        });
                        if state.creating {
                            ui.add_space(4.0);
                        }
                        // Skip works in both modes: create (no vault yet) or
                        // unlock (locked vault). Bypassing never touches the
                        // vault or imported profiles.
                        if ui
                            .add_sized(
                                [ui.available_width(), 28.0],
                                egui::Button::new(egui::RichText::new("Skip").weak()),
                            )
                            .on_hover_text(if state.creating {
                                "Use the app without stored passwords"
                            } else {
                                "Continue without decrypting the vault; \
                                 saved passwords stay locked"
                            })
                            .clicked()
                        {
                            outcome = Some(LockOutcome::Skip);
                        }
                        // Enter in either field submits (mstsc-style login).
                        let want_submit = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if want_submit {
                            outcome = Some(LockOutcome::Unlock {
                                passphrase: state.passphrase.clone(),
                                confirm: state.confirm.clone(),
                            });
                        }
                        if let Some(error) = &state.error {
                            ui.add_space(8.0);
                            ui.label(egui::RichText::new(error).color(crate::theme::DANGER));
                        }
                    },
                );
            },
        );
        outcome
    }
}

impl App {
    fn draw_manager(
        vault: &mut vault::Vault,
        ui: &mut egui::Ui,
        state: &mut ManagerState,
        profiles: &mut Vec<Profile>,
        pending: &mut Option<(Profile, bool)>,
    ) -> Option<VaultFlow> {
        // Vault actions gathered from the frame; applied after drawing.
        let mut vault_sign_in = false;
        let mut vault_kill = false;
        let mut new_profile = false;
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
                                ui.horizontal(|ui| {
                                    ui.label(egui::RichText::new("rstsc").strong().size(17.0));
                                    // Ghost-style sign out pinned to the
                                    // header's right edge.
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            if vault.is_unlocked()
                                                && ui
                                                    .add(egui::Button::new(
                                                        egui::RichText::new("Sign out")
                                                            .size(11.0)
                                                            .color(crate::theme::TEXT_MUTED),
                                                    ))
                                                    .on_hover_text(
                                                        "Lock the vault and forget the \
                                                         master passphrase until next sign-in",
                                                    )
                                                    .clicked()
                                            {
                                                vault_kill = true;
                                            }
                                        },
                                    );
                                });
                                ui.add_space(6.0);
                                ui.label(egui::RichText::new("Saved connections").weak().small());
                                ui.add_space(2.0);
                                // Vault state lives with the saved list it
                                // protects: locked → sign-in affordance.
                                if vault.is_unlocked() {
                                    let stored = vault.stored_names().count();
                                    ui.label(
                                        egui::RichText::new(match stored {
                                            0 => "vault unlocked · no passwords".to_owned(),
                                            1 => "vault unlocked · 1 password".to_owned(),
                                            n => format!("vault unlocked · {n} passwords"),
                                        })
                                        .weak()
                                        .small(),
                                    );
                                } else if vault::Vault::exists() {
                                    if ui
                                        .link("🔒 vault locked — sign in")
                                        .clicked()
                                    {
                                        vault_sign_in = true;
                                    }
                                } else if ui.link("🔒 set up vault").clicked() {
                                    vault_sign_in = true;
                                }
                                ui.add_space(6.0);
                                // Explicit "start a fresh profile" affordance:
                                // clears the selection and the draft form.
                                if ui
                                    .add_sized(
                                        [ui.available_width(), 24.0],
                                        egui::Button::new(egui::RichText::new("＋ New").weak()),
                                    )
                                    .clicked()
                                {
                                    new_profile = true;
                                }
                                ui.add_space(6.0);

                                let mut picked = None;
                                let mut connect_now = None;
                                let list_height = (ui.available_height() - 124.0).max(80.0);
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
                                        let old_name = state
                                            .selected
                                            .map(|i| profiles[i].name.clone());
                                        let renaming = old_name.is_some()
                                            && old_name.as_ref() != Some(&p.name);
                                        // Renaming a profile must carry its
                                        // stored password to the new key.
                                        if renaming
                                            && let Some(old) = old_name.clone()
                                        {
                                            match vault.password(&old) {
                                                Ok(Some(pwd)) => {
                                                    let _ = vault.set_password(&p.name, &pwd);
                                                    let _ = vault.remove_password(&old);
                                                }
                                                Ok(None) => {}
                                                Err(error) => {
                                                    // Locked vault: keep the
                                                    // plaintext-profile path
                                                    // nothing to migrate.
                                                    tracing::warn!(
                                                        %error,
                                                        profile = %old,
                                                        "could not read the vault \
                                                         entry while renaming"
                                                    );
                                                }
                                            }
                                        }
                                        // The vault owns passwords. Locked
                                        // vault = no remember, no error:
                                        // the UI already keeps the checkbox
                                        // disabled, so just forget the flag.
                                        if p.remember_password && !vault.is_unlocked() {
                                            p.remember_password = false;
                                        }
                                        // Passwords live in the encrypted
                                        // vault when "remember" is on;
                                        // profiles.json never sees them.
                                        if p.remember_password && !p.password.is_empty() {
                                            if let Err(error) =
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
                                        if let Some(gateway) = &mut p.gateway
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
                                        let password = std::mem::take(&mut p.password);
                                        p.remember_password =
                                            p.remember_password && vault.has_password(&p.name);
                                        if vault.is_unlocked() {
                                            // v2: seal the full settings-only
                                            // profile in the vault.
                                            if let Err(error) = vault.set_profile(&p.name, &p) {
                                                state.error = Some(error.to_string());
                                            }
                                            if renaming
                                                && let Some(old) = old_name
                                            {
                                                let _ = vault.remove_profile(&old);
                                            }
                                        }
                                        let mut p = p;
                                        if vault.has_password(&p.name) {
                                            // Re-fetch the password so the
                                            // pending draft/launch has it.
                                            p.password = password;
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
                                                "could not remove the vault password entry"
                                            );
                                        }
                                        if let Err(error) = vault.remove_profile(&removed.name) {
                                            tracing::warn!(
                                                %error,
                                                profile = %removed.name,
                                                "could not remove the sealed profile entry"
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
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 6.0;
                                            // Masked until the user asks to
                                            // review it (eye toggle),
                                            // regardless of Remember.
                                            ui.add(
                                                egui::TextEdit::singleline(
                                                    &mut state.draft.password,
                                                )
                                                .password(!state.show_password)
                                                .desired_width((form_width - 196.0).max(80.0)),
                                            );
                                            let (glyph, tip, color) = if state.show_password {
                                                (
                                                    "◆",
                                                    "Hide password",
                                                    crate::theme::ACCENT_HOVER,
                                                )
                                            } else {
                                                ("◇", "Show password", crate::theme::TEXT_MUTED)
                                            };
                                            let toggle = ui
                                                .add(crate::theme::icon_button(glyph, color))
                                                .on_hover_text(tip);
                                            if toggle.clicked() {
                                                state.show_password = !state.show_password;
                                            }
                                            toggle
                                        });
                                        ui.end_row();

                                        ui.label("Remember:");
                                        ui.horizontal(|ui| {
                                            let vault_open = vault.is_unlocked();
                                            let remember_row = ui.add_enabled(
                                                vault_open,
                                                egui::Checkbox::new(
                                                    &mut state.draft.remember_password,
                                                    "save password (encrypted vault)",
                                                ),
                                            );
                                            if !vault_open {
                                                remember_row.on_disabled_hover_text(
                                                    "Vault is locked — sign in on the \
                                                     lock screen to remember passwords",
                                                );
                                                // Turning the vault off must
                                                // not leave the flag set.
                                                state.draft.remember_password = false;
                                            }
                                        });
                                        ui.end_row();
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

        // "+ New": clear the selection and reset the draft form so the next
        // Save appends. Applied after the frame so a same-click list pick
        // cannot re-fill the draft.
        if new_profile {
            state.selected = None;
            state.error = None;
            state.draft = Profile::new(String::new());
        }

        // Vault sign-in/sign-out declared in the frame, applied here outside
        // the borrows. Both hand over to the lock screen; sign-out drops the
        // key and any decrypted data before it does.
        if vault_sign_in {
            return Some(VaultFlow::SignIn { creating: false });
        }
        if vault_kill {
            vault.lock();
            // The in-memory profile list may carry decrypted passwords from
            // rehydration; drop them before the vault goes dark. The lock
            // screen (not the manager) asks for the master passphrase next.
            for p in profiles.iter_mut() {
                p.password.clear();
            }
            if let Some(gateway) = &mut state.draft.gateway {
                gateway.password.clear();
            }
            state.draft.password.clear();
            return Some(VaultFlow::SignIn { creating: false });
        }
        None
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
        tracing::debug!(events = batch.len(), "Sending input batch to remote");
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
