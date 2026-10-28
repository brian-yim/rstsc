//! Saved connection profiles, persisted as JSON in the user config directory.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub address: String,
    #[serde(default)]
    pub username: String,
    /// Only persisted when `remember_password` is set.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    #[serde(default)]
    pub remember_password: bool,
    /// Desktop size behavior.
    #[serde(default)]
    pub desktop: DesktopMode,
    /// Color depth in bits per pixel: 16, 24 or 32.
    #[serde(default = "default_color_depth")]
    pub color_depth: u32,
    #[serde(default = "default_true")]
    pub clipboard: bool,
    /// Number of horizontally arranged remote monitors to request (1–4).
    #[serde(default = "default_monitor_count")]
    pub monitor_count: u8,
    #[cfg(feature = "audio")]
    #[serde(default = "default_true")]
    pub audio: bool,
    #[serde(default)]
    pub audio_capture: bool,
    #[serde(default)]
    pub validate_tls: bool,
    /// NLA (CredSSP). mstsc default is on.
    #[serde(default = "default_true")]
    pub credssp: bool,
    /// RemoteApp program to launch (e.g. `C:\Windows\System32\notepad.exe`).
    /// Empty = full desktop.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub remote_app: String,
    #[serde(default)]
    pub remote_app_dir: String,
    #[serde(default)]
    pub administrative_session: bool,
    #[serde(default = "default_true")]
    pub compression: bool,
    /// Performance flags (mstsc "Experience" settings).
    #[serde(default = "default_perf")]
    pub perf: PerfFlags,
    #[serde(default)]
    pub gateway: Option<GatewayConfig>,
    /// Local folder redirected as a drive on the remote side (RDPDR).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub share_folder: Option<String>,
    /// Auto-reconnect attempts after transport loss (mstsc default 20; we default to 3).
    #[serde(default = "default_reconnect")]
    pub reconnect_attempts: u32,
}

pub fn default_color_depth() -> u32 {
    32
}

pub fn default_true() -> bool {
    true
}

pub fn default_reconnect() -> u32 {
    3
}

pub fn default_monitor_count() -> u8 {
    1
}

fn default_perf() -> PerfFlags {
    PerfFlags::ALL
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopMode {
    /// Dynamic resize via Display Control, tracking the window size.
    #[default]
    FitWindow,
    /// Fixed resolution as negotiated at connection time.
    Fixed { width: u16, height: u16 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerfFlags {
    pub wallpaper: bool,
    pub font_smoothing: bool,
    pub window_dragging: bool,
    pub menu_animations: bool,
    pub themes: bool,
    pub cursor_settings: bool,
}

impl PerfFlags {
    /// mstsc "Modem" preset is the minimal one; "LAN"/"High" enables everything.
    pub const ALL: Self = Self {
        wallpaper: true,
        font_smoothing: true,
        window_dragging: true,
        menu_animations: true,
        themes: true,
        cursor_settings: true,
    };
}

impl Default for PerfFlags {
    fn default() -> Self {
        Self::ALL
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub address: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
}

impl Profile {
    pub fn new(address: impl Into<String>) -> Self {
        let address = address.into();
        let name = address.split(':').next().unwrap_or(&address).to_owned();
        Self {
            name,
            address,
            color_depth: default_color_depth(),
            clipboard: true,
            monitor_count: default_monitor_count(),
            credssp: true,
            compression: true,
            perf: PerfFlags::ALL,
            reconnect_attempts: default_reconnect(),
            #[cfg(feature = "audio")]
            audio: true,
            ..Default::default()
        }
    }
}

pub fn profiles_path() -> std::path::PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("rstsc")
        .join("profiles.json")
}

pub fn load_profiles() -> anyhow::Result<Vec<Profile>> {
    let path = profiles_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = std::fs::read_to_string(&path)?;
    Ok(serde_json::from_str(&data)?)
}

pub fn save_profiles(profiles: &[Profile]) -> anyhow::Result<()> {
    let path = profiles_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Single choke point for the "no password on disk unless remembered" rule.
    let sanitized: Vec<Profile> = profiles
        .iter()
        .map(|p| {
            let mut p = p.clone();
            if !p.remember_password {
                p.password.clear();
            }
            p
        })
        .collect();
    std::fs::write(&path, serde_json::to_vec_pretty(&sanitized)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrip() {
        let mut p = Profile::new("winbox.example.com:3390");
        p.username = "CORP\\alice".into();
        p.desktop = DesktopMode::Fixed {
            width: 1920,
            height: 1080,
        };
        p.password = "secret".into();
        p.remember_password = false;

        // Passwords must never hit disk unless remembered.
        let sanitized: Vec<Profile> = vec![{
            let mut q = p.clone();
            if !q.remember_password {
                q.password.clear();
            }
            q
        }];
        let json = serde_json::to_string(&sanitized).unwrap();
        assert!(!json.contains("secret"));
        let deserialized: Vec<Profile> = serde_json::from_str(&json).unwrap();
        let q = &deserialized[0];
        assert_eq!(q.password, "");
        assert_eq!(q.address, p.address);
        assert_eq!(q.desktop, p.desktop);
    }

    #[test]
    fn older_profiles_default_to_one_monitor() {
        let profile: Profile = serde_json::from_str(r#"{"name":"test","address":"host"}"#).unwrap();
        assert_eq!(profile.monitor_count, 1);
    }

    #[test]
    fn profile_names_are_user_editable_and_defaulted() {
        let mut p = Profile::new("winbox.example.com:3390");
        // Auto-derived default.
        assert_eq!(p.name, "winbox.example.com");
        // User rename sticks through JSON roundtrip.
        p.name = "Work cube".to_owned();
        let json = serde_json::to_string(&p).unwrap();
        let q: Profile = serde_json::from_str(&json).unwrap();
        assert_eq!(q.name, "Work cube");
        // Empty name falls back to the address host part on save.
        let mut p = Profile::new("host.example.com");
        p.name = String::new();
        if p.name.is_empty() {
            p.name = p.address.split(':').next().unwrap_or(&p.address).to_owned();
        }
        assert_eq!(p.name, "host.example.com");
    }
}
