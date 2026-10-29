//! File-based encrypted password vault.
//!
//! Remembered RDP passwords never touch disk in plaintext. Each profile's
//! password is stored as an independent AES-256-GCM (ChaCha20-Poly1305)
//! ciphertext in `secrets.vault` (chmod 0600), keyed by the profile name.
//! The vault key is derived from a master passphrase with Argon2id; the
//! passphrase lives only in memory for the app session.
//!
//! Non-blocking use: connect still works with no vault at all (empty or
//! missing passphrase), it just won't have remembered passwords.

use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, KeyInit},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

///How long a passphrase must be to be accepted. mstsc-quality minimum
/// (same check applies to RDP passwords themselves).
pub const MIN_PASSPHRASE_LEN: usize = 8;

const KDF_M_COST_KB: u32 = 19 * 1024; // ~19 MiB
const KDF_T_COST: u32 = 2;
const KDF_PARALLELISM: u8 = 1;
const SALT_LEN: usize = 16;
const VERSION: u8 = 1;

/// `entries` key prefix marking a sealed full [`crate::profile::Profile`]
/// blob. A bare (unprefixed) key seals that profile's password only, the v1
/// layout kept for compatibility.
const PROFILE_KEY_PREFIX: &str = "profile/";

/// Strips the [`PROFILE_KEY_PREFIX`] and returns the profile name.
fn profile_key_name(key: &str) -> Option<&str> {
    key.strip_prefix(PROFILE_KEY_PREFIX)
}

/// Splits raw entry keys into (password-only names, full-profile names).
fn split_entry_names<'a, I>(keys: I) -> (std::collections::BTreeSet<String>, std::collections::BTreeSet<String>)
where
    I: IntoIterator<Item = &'a String>,
{
    let mut names = std::collections::BTreeSet::new();
    let mut profile_names = std::collections::BTreeSet::new();
    for key in keys {
        match profile_key_name(key) {
            Some(name) => {
                profile_names.insert(name.to_owned());
            }
            None => {
                names.insert(key.clone());
            }
        }
    }
    (names, profile_names)
}

#[derive(Serialize, Deserialize)]
struct VaultFile {
    version: u8,
    /// Base64 of KDF salt.
    kdf_salt: String,
    /// Base64 of ChaCha20-Poly1305 key produced by the KDF; itself encrypted
    /// so that rotating the passphrase doesn't re-encrypt every entry.
    /// WE DO NOT DO THIS: all entries share the KDF output key directly and
    /// passphrase rotation re-encrypts everything in one pass.
    entries: std::collections::BTreeMap<String, String>,
}

pub struct Vault {
    path: std::path::PathBuf,
    /// Derived key; present once the passphrase is supplied and validated.
    key: Option<Key>,
    salt: [u8; SALT_LEN],
    /// Cache of the entries so the UI can show which profiles have a stored
    /// password without decrypting anything.
    names: std::collections::BTreeSet<String>,
    /// Names with a sealed full-profile blob (`profile/<name>` key). Empty in
    /// v1 vaults until a save or migration seals them.
    profile_names: std::collections::BTreeSet<String>,
}

fn b64(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn unb64(text: &str) -> anyhow::Result<Vec<u8>> {
    use base64::Engine as _;
    Ok(base64::engine::general_purpose::STANDARD.decode(text)?)
}

fn derive_key(passphrase: &str, salt: &[u8]) -> anyhow::Result<Key> {
    use argon2::Argon2;
    let mut key = Key::default();
    Argon2::new(
        argon2::Algorithm::Argon2id,
        argon2::Version::V0x13,
        argon2::Params::new(
            KDF_M_COST_KB,
            KDF_T_COST,
            KDF_PARALLELISM.into(),
            Some(key.len()),
        )
        .map_err(|e| anyhow::anyhow!("argon2 params: {e}"))?,
    )
    .hash_password_into(passphrase.as_bytes(), salt, key.as_mut_slice())
    .map_err(|e| anyhow::anyhow!("argon2 kdf: {e}"))?;
    Ok(key)
}

fn seal(key: &Key, plaintext: &[u8]) -> anyhow::Result<String> {
    let cipher = ChaCha20Poly1305::new(key);
    let mut nonce_bytes = [0_u8; 12];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher
        .encrypt(&nonce, plaintext)
        .map_err(|e| anyhow::anyhow!("encrypt: {e}"))?;
    Ok(format!("{}:{}", b64(nonce.as_slice()), b64(&ct)))
}

fn open(key: &Key, blob: &str) -> anyhow::Result<Vec<u8>> {
    let (nonce_b64, ct_b64) = blob
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("vault entry is malformed"))?;
    let cipher = ChaCha20Poly1305::new(key);
    let nonce_bytes = unb64(nonce_b64)?;
    if nonce_bytes.len() != 12 {
        anyhow::bail!("vault entry has a malformed nonce");
    }
    let ct = unb64(ct_b64)?;
    cipher
        .decrypt(Nonce::from_slice(&nonce_bytes), ct.as_slice())
        .map_err(|_| anyhow::anyhow!("decryption failed (wrong passphrase?)"))
}

impl Vault {
    /// An empty, unlocked in-memory vault (no file on disk yet).
    pub fn fresh() -> Self {
        let mut salt = [0_u8; SALT_LEN];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
        Self {
            path: Self::path(),
            key: Some(Key::default()),
            salt,
            names: Default::default(),
            profile_names: Default::default(),
        }
    }
    pub fn path() -> std::path::PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("rstsc")
            .join("secrets.vault")
    }

    /// Loads the vault metadata. The `key` stays unset until
    /// [`Vault::unlock`] succeeds; entries are only names at this point.
    pub fn load() -> anyhow::Result<Self> {
        Self::load_with_path(&Self::path())
    }

    pub fn load_with_path(path: &std::path::Path) -> anyhow::Result<Self> {
        let mut salt = [0_u8; SALT_LEN];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
        if !path.exists() {
            return Ok(Self {
                path: path.to_path_buf(),
                key: None,
                salt,
                names: Default::default(),
            profile_names: Default::default(),
            });
        }
        let data = std::fs::read(&path)?;
        // Try to read existing KDF params.
        let parsed: VaultFile = serde_json::from_slice(&data)?;
        if parsed.version != VERSION {
            anyhow::bail!("secrets.vault version {} is not supported", parsed.version);
        }
        let salt_vec = unb64(&parsed.kdf_salt)?;
        if salt_vec.len() != SALT_LEN {
            anyhow::bail!("vault KDF salt has the wrong length");
        }
        salt.copy_from_slice(&salt_vec);
        let (names, profile_names) = split_entry_names(parsed.entries.keys());
        Ok(Self {
            path: path.to_path_buf(),
            key: None,
            salt,
            names,
            profile_names,
        })
    }

    /// True if the vault file exists on disk (even if locked).
    pub fn exists() -> bool {
        Self::path().exists()
    }

    /// true when the vault contains a (possibly undecryptable) entry for
    /// `profile`.
    pub fn has_password(&self, profile: &str) -> bool {
        self.key.is_some() && self.names.contains(profile)
    }

    /// True when the passphrase has been verified this session.
    pub fn is_unlocked(&self) -> bool {
        self.key.is_some()
    }

    /// Validates the passphrase by checking its derived key against every
    /// stored entry (the first decrypt success wins). Empty passphrase with
    /// an empty (fresh) vault is an instant success.
    pub fn unlock(&mut self, passphrase: &str) -> anyhow::Result<()> {
        let key = derive_key(passphrase, &self.salt)?;
        let data = std::fs::read(&self.path).unwrap_or_default();
        if data.is_empty() {
            self.key = Some(key);
            return Ok(());
        }
        let parsed: VaultFile = serde_json::from_slice(&data)?;
        for blob in parsed.entries.values() {
            if open(&key, blob).is_ok() {
                self.key = Some(key);
                self.salt = unb64(&parsed.kdf_salt)?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("vault KDF salt has the wrong length"))?;
                let (names, profile_names) = split_entry_names(parsed.entries.keys());
                self.names = names;
                self.profile_names = profile_names;
                return Ok(());
            }
        }
        // Wrong passphrase: verify with a canary so we do not silently create
        // a "valid but unreadable" vault on save.
        if !parsed.entries.is_empty() {
            anyhow::bail!("wrong passphrase");
        }
        self.key = Some(key);
        Ok(())
    }

    fn lock_write(&self, mut entries: VaultFile) -> anyhow::Result<()> {
        use base64::Engine as _;
        entries.version = VERSION;
        entries.kdf_salt = base64::engine::general_purpose::STANDARD.encode(self.salt);
        let tmp = self.path.with_extension("tmp");
        let bytes = serde_json::to_vec_pretty(&entries)?;
        std::fs::write(&tmp, &bytes)?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Stores (or overwrites) `password` for `profile`. Requires a previous
    /// successful [`Vault::unlock`].
    pub fn set_password(&mut self, profile: &str, password: &str) -> anyhow::Result<()> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vault is locked; set the master passphrase first"))?;
        let mut entries = self.read_entries()?;
        if entries.is_empty() && !self.path.exists() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let blob = seal(key, password.as_bytes())?;
        entries.insert(profile.to_owned(), blob);
        self.lock_write(VaultFile {
            version: VERSION,
            kdf_salt: b64(&self.salt),
            entries,
        })?;
        self.names.insert(profile.to_owned());
        Ok(())
    }

    /// Returns the stored password for `profile`, if any.
    pub fn password(&self, profile: &str) -> anyhow::Result<Option<String>> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vault is locked; set the master passphrase first"))?;
        let entries = self.read_entries()?;
        let Some(blob) = entries.get(profile) else {
            return Ok(None);
        };
        let pt = open(key, blob)?;
        String::from_utf8(pt)
            .map(Some)
            .map_err(|_| anyhow::anyhow!("stored password is not UTF-8"))
    }

    /// Removes the entry for `profile` (no error when absent).
    pub fn remove_password(&mut self, profile: &str) -> anyhow::Result<()> {
        let mut entries = self.read_entries()?;
        if entries.remove(profile).is_some() {
            self.lock_write(VaultFile {
                version: VERSION,
                kdf_salt: b64(&self.salt),
                entries,
            })?;
        }
        self.names.remove(profile);
        Ok(())
    }

    /// Profile names with stored passwords (metadata only, no decryption).
    pub fn stored_names(&self) -> impl Iterator<Item = &str> {
        self.names.iter().map(String::as_str)
    }

    /// Names with a sealed full profile (v2). Metadata only, no decryption.
    pub fn stored_profile_names(&self) -> impl Iterator<Item = &str> {
        self.profile_names.iter().map(String::as_str)
    }

    /// Seals the full `profile` under `profile/<name>`. The profile JSON may
    /// contain a password field; the seal covers it, so nothing lends itself
    /// to plaintext on disk. Requires an unlocked vault.
    pub fn set_profile(&mut self, name: &str, profile: &crate::profile::Profile) -> anyhow::Result<()> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vault is locked; set the master passphrase first"))?;
        let mut entries = self.read_entries()?;
        if entries.is_empty() && !self.path.exists() {
            if let Some(parent) = self.path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let blob = seal(key, serde_json::to_vec(profile)?.as_slice())?;
        entries.insert(format!("{PROFILE_KEY_PREFIX}{name}"), blob);
        self.lock_write(VaultFile {
            version: VERSION,
            kdf_salt: b64(&self.salt),
            entries,
        })?;
        self.profile_names.insert(name.to_owned());
        Ok(())
    }

    /// Unseals and deserializes the stored full profile for `name`.
    /// Returns `None` when only the v1 password entry exists.
    pub fn profile(&self, name: &str) -> anyhow::Result<Option<crate::profile::Profile>> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vault is locked; set the master passphrase first"))?;
        let entries = self.read_entries()?;
        let Some(blob) = entries.get(&format!("{PROFILE_KEY_PREFIX}{name}")) else {
            return Ok(None);
        };
        let pt = open(key, blob)?;
        serde_json::from_slice(&pt)
            .map(Some)
            .map_err(|e| anyhow::anyhow!("stored profile is corrupt: {e}"))
    }

    /// Removes the sealed profile entry for `name` (no error when absent).
    /// The password entry (if any) stays; removal is keyed independently.
    pub fn remove_profile(&mut self, name: &str) -> anyhow::Result<()> {
        let mut entries = self.read_entries()?;
        if entries.remove(&format!("{PROFILE_KEY_PREFIX}{name}")).is_some() {
            self.lock_write(VaultFile {
                version: VERSION,
                kdf_salt: b64(&self.salt),
                entries,
            })?;
        }
        self.profile_names.remove(name);
        Ok(())
    }

    /// Unseals every sealed profile. Order: sorted by name (entries map is a
    /// BTreeMap, so iteration is already name-keyed). Passwords are NOT
    /// rehydrated here — the app layer merges them from the entries.
    pub fn profiles(&self) -> anyhow::Result<Vec<crate::profile::Profile>> {
        let key = self
            .key
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("vault is locked; set the master passphrase first"))?;
        let entries = self.read_entries()?;
        let mut out = Vec::new();
        for (name, blob) in &entries {
            let Some(profile_name) = profile_key_name(name) else {
                continue;
            };
            match open(key, blob) {
                Ok(pt) => match serde_json::from_slice::<crate::profile::Profile>(&pt) {
                    Ok(mut p) => {
                        // The blob is settings-only; merge the password entry
                        // (if any) so callers get launch-ready profiles.
                        if let Ok(Some(pwd)) = self.password(profile_name) {
                            p.password = pwd;
                        }
                        out.push(p);
                    }
                    Err(error) => {
                        tracing::warn!(%error, profile = %profile_name, "sealed profile unreadable; skipping");
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, profile = %profile_name, "sealed profile decrypted badly; skipping");
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn read_entries(&self) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
        if !self.path.exists() {
            return Ok(Default::default());
        }
        let data = std::fs::read(&self.path)?;
        if data.is_empty() {
            return Ok(Default::default());
        }
        let parsed: VaultFile = serde_json::from_slice(&data)?;
        Ok(parsed.entries)
    }

    /// Writes plaintext `legacy profiles.json` passwords into the vault and
    /// scrubs them from the JSON. Also seals the full profile (v2) so the
    /// vault owns the whole record. Idempotent: only profiles without a
    /// vault entry are migrated.
    pub fn migrate_plaintext(&mut self, profiles: &mut [crate::profile::Profile]) -> usize {
        let mut migrated = 0usize;
        for p in profiles {
            if p.password.is_empty() || !p.remember_password {
                continue;
            }
            if self.names.contains(&p.name) {
                continue;
            }
            let mut sealed = p.clone();
            let password = std::mem::take(&mut sealed.password);
            // The sealed profile carries settings only; the password lives in
            // its own entry (one ciphertext of the secret, not two) and
            // rehydration merges them on launch.
            if self.set_profile(&p.name, &sealed).is_err() {
                continue;
            }
            if self.set_password(&p.name, &password).is_err() {
                continue;
            }
            migrated += 1;
            // The vault now owns the password; scrub the plaintext so the
            // on-disk JSON is clean on the next save. Connect re-hydrates
            // from the vault.
            p.password.clear();
        }
        migrated
    }

    /// Drops the derived key.
    pub fn lock(&mut self) {
        if let Some(mut key) = self.key.take() {
            key.as_mut_slice().zeroize();
        }
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        self.lock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("rstsc-vault-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn set_get_remove_roundtrip() {
        let path = unique_path("roundtrip");
        let mut vault = Vault {
            path: path.clone(),
            key: None,
            salt: [0_u8; SALT_LEN],
            names: Default::default(),
            profile_names: Default::default(),
        };
        assert!(vault.unlock("correct horse battery").is_ok());
        vault.set_password("office", "P@ssw0rd!").unwrap();
        assert_eq!(
            vault.password("office").unwrap().as_deref(),
            Some("P@ssw0rd!")
        );
        // Stored on disk only as ciphertext.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("P@ssw0rd!"));
        assert_eq!(
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777
            },
            0o600
        );
        vault.remove_password("office").unwrap();
        assert_eq!(vault.password("office").unwrap(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn profile_seal_roundtrip() {
        let path = unique_path("profilert");
        let mut vault = Vault {
            path: path.clone(),
            key: None,
            salt: [0_u8; SALT_LEN],
            names: Default::default(),
            profile_names: Default::default(),
        };
        vault.unlock("correct horse battery").is_ok();
        let mut p = crate::profile::Profile::new("winbox.example.com:3390");
        p.username = "CORP\\alice".to_owned();
        vault.set_profile(&p.name, &p).unwrap();

        // The profile *name* is the entries key (metadata, same as v1
        // password keys); the sealed blob must not leak settings.
        let raw = std::fs::read_to_string(&path).unwrap();
        // Key names are expected metadata; check the *value* blob alone.
        let parsed: VaultFile = serde_json::from_str(&raw).unwrap();
        let blob = parsed
            .entries
            .get(&format!("{PROFILE_KEY_PREFIX}{}", p.name))
            .unwrap();
        assert!(!blob.contains("winbox"), "address leaked in blob");
        assert!(!blob.contains("CORP"), "username leaked in blob");

        let round = vault.profile(&p.name).unwrap().unwrap();
        assert_eq!(round, p);
        assert!(vault.stored_profile_names().any(|n| n == p.name));

        // Remove.
        vault.remove_profile(&p.name).unwrap();
        assert_eq!(vault.profile(&p.name).unwrap(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn profile_rename_moves_both_entries() {
        let path = unique_path("profilerename");
        let mut vault = Vault {
            path: path.clone(),
            key: None,
            salt: [0_u8; SALT_LEN],
            names: Default::default(),
            profile_names: Default::default(),
        };
        vault.unlock("k").unwrap();
        let mut p = crate::profile::Profile::new("old-host");
        p.username = "alice".to_owned();
        vault.set_profile("old-host", &p).unwrap();
        vault.set_password("old-host", "pw1").unwrap();

        // The UI's rename sequence: carry the password to the new key, drop
        // the old entries, seal the settings under the new name.
        let mut renamed = p.clone();
        renamed.name = "new-host".to_owned();
        let pwd = vault.password("old-host").unwrap().unwrap();
        vault.set_password("new-host", &pwd).unwrap();
        vault.remove_password("old-host").unwrap();
        vault.set_profile("new-host", &renamed).unwrap();
        vault.remove_profile("old-host").unwrap();

        assert!(vault.profile("new-host").unwrap().is_some());
        assert_eq!(vault.password("new-host").unwrap().as_deref(), Some("pw1"));
        assert_eq!(vault.profile("old-host").unwrap(), None);
        assert_eq!(vault.password("old-host").unwrap(), None);
        // Bulk read sees only the renamed profile.
        let all = vault.profiles().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "new-host");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wrong_passphrase_is_rejected() {
        let path = unique_path("wrongpass");
        let mut vault = Vault {
            path: path.clone(),
            key: None,
            salt: [0_u8; SALT_LEN],
            names: Default::default(),
            profile_names: Default::default(),
        };
        vault.unlock("master-pass-1").unwrap();
        vault.set_password("host-a", "secret").unwrap();
        vault.lock();
        // Reload reads the salt, but the key stays unset.
        let mut reloaded = Vault::load_with_path(&path).unwrap();
        assert!(reloaded.unlock("wrong-pass-1").is_err());
        assert!(reloaded.unlock("master-pass-1").is_ok());
        assert_eq!(
            reloaded.password("host-a").unwrap().as_deref(),
            Some("secret")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn migrate_plaintext_scrubs_profile() {
        let mut profile = crate::profile::Profile::new("legacy-host");
        profile.password = "legacy-secret".to_owned();
        profile.remember_password = true;
        let mut profiles = vec![profile.clone()];
        let path = unique_path("migrate");
        let mut vault = Vault {
            path: path.to_path_buf(),
            key: None,
            salt: [0_u8; SALT_LEN],
            names: Default::default(),
            profile_names: Default::default(),
        };
        vault.unlock("master").unwrap();
        assert_eq!(vault.migrate_plaintext(&mut profiles), 1);
        assert_eq!(
            vault.password("legacy-host").unwrap().as_deref(),
            Some("legacy-secret")
        );
        // Saved JSON must not contain the plaintext.
        crate::profile::save_profiles(&profiles).unwrap();
        let raw = std::fs::read_to_string(crate::profile::profiles_path()).unwrap();
        assert!(!raw.contains("legacy-secret"));
        // Idempotent.
        assert_eq!(vault.migrate_plaintext(&mut profiles), 0);
    }

    /// The exact `apply_unlock` sequence against a real vault file sealed
    /// offline (passphrase "smokepassphraseone"). Skipped when the file is
    /// absent so plain `cargo test` stays hermetic.
    #[test]
    fn real_file_unlock_and_profile_merge() {
        let path = match std::env::var("RSTSC_SMOKE_VAULT") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => return,
        };
        if !path.exists() {
            return;
        }
        let mut v = Vault::load_with_path(&path).expect("load");
        assert!(!v.is_unlocked());
        let passphrase = std::env::var("RSTSC_SMOKE_PASS")
            .unwrap_or_else(|_| "smokepassphraseone".into());
        v.unlock(&passphrase).expect("unlock");
        let profiles = v.profiles().expect("profiles");
        assert_eq!(profiles.len(), 1, "one sealed profile expected");
        let p = &profiles[0];
        assert_eq!(p.name, "legacy-host");
        assert!(!p.password.is_empty(), "password merged from its own entry");
        assert_eq!(p.password, v.password("legacy-host").unwrap().unwrap());
    }
}
