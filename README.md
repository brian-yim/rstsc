# rstsc

A Linux Remote Desktop client for Windows hosts, written in Rust, replicating
the look and behavior of Microsoft's mstsc (Remote Desktop Connection).

Built on the [IronRDP](https://github.com/Devolutions/IronRDP) protocol stack —
the same pure-Rust engine Devolutions ships in production — so protocol
correctness (NLA, TLS, capabilities, codecs, virtual channels) is inherited
from the engine. This repo contributes the egui GUI, session embedding,
connection manager, and Linux integration (X11/Wayland clipboard and input).

## Build & run

```sh
./install.sh                              # build + install to /usr/local (sudo) or ~/.local
./install.sh --user                       # force ~/.local, no root
./install.sh --uninstall                  # remove installed files
cargo build --release                     # plain build, no install
./target/release/rstsc                    # connection manager GUI
./target/release/rstsc host[:port]        # quick connect
./target/release/rstsc -p myprofile -f    # saved profile, fullscreen
./target/release/rstsc -r ~/some.rdp      # mstsc .rdp file
```

`install.sh` builds from source (no RPM needed), installs the binary, the
desktop entry and all hicolor icons, registers the `rdp://` URL handler, and
installs Rust via rustup if the distro toolchain is older than 1.94. With ALSA
headers present it enables the `audio` feature automatically.

Requirements: Rust ≥ 1.94, X11 or Wayland. Network access to fetch the pinned
IronRDP git deps on first build.

Audio (remote playback + microphone capture) requires ALSA headers:

```sh
sudo dnf install alsa-lib-devel   # or libasound2-dev on Debian/Ubuntu
cargo build --release --features audio
```

## mstsc feature parity

Legend: ✅ works · ⚠️ partial (protocol supported, UX incomplete) · ❌ not yet · ➖ N/A on Linux.

| mstsc feature | Status | Notes |
|---|---|---|
| TLS / CredSSP (NLA) login | ✅ | NTLM via Rust SSPI; Kerberos available through engine config |
| Color depth 16/24/32 | ✅ | Display settings |
| Resolution: fixed | ✅ | |
| Dynamic resolution resize | ✅ | MS-RDPEDISP Display Control, tracks window size |
| Smart sizing (fit to window) | ✅ | Toolbar "Fit" toggle |
| Fullscreen | ✅ | Toolbar button |
| Connection bar | ✅ | Title (profile/host), per-screen picker, Fit, Fullscreen, Ctrl+Alt+Del, Disconnect |
| Server-supplied cursors (bitmaps) | ✅ | |
| Clipboard: text + images | ✅ | X11/Wayland via `ironrdp-cliprdr-native` |
| Clipboard: keyboard shortcuts | ✅ | Ctrl+C/X/V route by pointer position: remote surface gets the keystrokes directly (no local clipboard read), local text fields get egui's clipboard |
| Clipboard: file copy/paste | ✅ | Copy/paste in both directions; dropping local files on the client surface offers them to the remote. Remote file lists are mirrored lazily: downloads start on the first local paste (or after a 10 s grace), so the remote's rdpclip stream state isn't churned by eager requests |
| Drive redirection (RDPDR) | ✅ | Share a local folder as a drive (Advanced → Shared folder); full read/write via upstream `NixRdpdrBackend` |
| Printer / port / smartcard / PnP redirect | ❌ | Needs backend implementations (drive redirection is done; the same trait surface covers the rest) |
| Remote audio playback | ✅ | `--features audio` (cpal/ALSA) |
| Microphone redirection | ✅ | `--features audio` |
| RD Gateway (MS-TSGU HTTPS) | ✅ | Advanced → RD Gateway |
| RemoteApp (RAIL) launch | ⚠️ | Launches remotely; remote windows are not yet torn out into local windows |
| H.264/AVC420/AVC444 graphics (EGFX) | ✅ | Uses IronRDP's H.264 decoder (OpenH264) |
| H.265/HEVC graphics | ❌ | Pinned IronRDP revision has no HEVC negotiation or EGFX decoder path |
| RDP6 planar / RFX progressive / NSCodec / ClearCodec | ✅ | Engine codecs |
| RDP-UDP2 multitransport | ❌ | Engine supports it; not enabled in this build (aws-lc toolchain) |
| Auto-reconnect | ✅ | Cookie-based, configurable attempts |
| Compression (bulk + lossy) | ✅ | |
| Experience/performance flags | ✅ | Wallpaper, font smoothing, drag, menus, themes, cursor |
| /admin (console) session | ✅ | |
| TLS certificate validation | ⚠️ | Strict on/off toggle; mstsc shows an interactive prompt instead |
| Interactive credential prompt | ❌ | Credentials come from the manager UI / CLI; failed NLA returns an error |
| Multi-monitor | ✅ | 1–4 horizontal remote screens; the connection bar picks **All screens** or a single screen to view (mouse coordinates remapped; view-only, no reconnect) |
| Window title | ✅ | `<profile name> — rstsc` in a session, `rstsc` in the manager, `rstsc — locked` on the lock screen |
| Bitmap cache / orders / caps | ✅ | Engine-internal |
| Save/load connections | ✅ | Sealed in the vault; imports mstsc `.rdp` files |
| Location redirection | ➖ | Engine supports it (feature `client-location` enabled); needs OS location source |
| WebAuthn redirection | ➖ | Upstream path is Windows-hosted; N/A on Linux |
| Hyper-V VM Connect | ➖ | Engine feature exists (`vmconnect`); not wired in this build |
| Password vault | ✅ | Master passphrase on a lock screen; Skip = no vault access; password masked in the form with a reveal toggle |

## Connection manager

- Lock screen first: enter the master passphrase (Unlock), create one on
  first run (Create vault), or press **Skip** to use the app without stored
  passwords — skipping never reads the vault; saved passwords stay locked.
  Sign out returns here.
- Left rail: app title with a **Sign out** button (visible while unlocked),
  vault status line ("vault unlocked · N passwords" or a clickable
  🔒 sign-in link when locked), saved profiles (single click = select,
  double click = connect), and a **＋ New** button that clears the form for
  a fresh profile, plus Save/Delete.
- Right column: new/edit form (Profile name, Computer, User name, Password,
  Remember, Display, Advanced), with the **Connect** / **Open .rdp…** action
  row pinned at the bottom of the column — the form scrolls, the buttons
  don't. The password field is masked; a small 👁/‥ button beside it reveals
  it until toggled back. *Remember* is only enabled while the vault is
  unlocked; with the vault skipped or signed out the checkbox is disabled
  and the saved flag is dropped.
- Profile naming is free-form; empty falls back to the host part of the
  address. Renaming a profile moves its stored password **and** sealed
  settings with it.

## Password vault

Remembered passwords (RDP + optional RD Gateway) and the profile settings
themselves never hit disk unencrypted:

- Storage: `${XDG_CONFIG_HOME:-~/.config}/rstsc/secrets.vault`, mode
  0600, atomically written via `rename`. Each entry is an independent AEAD
  ciphertext (ChaCha20-Poly1305):
  - `<name>` — the profile's password,
  - `profile/<name>` — the full connection settings as sealed JSON.
- The vault key is derived from a master passphrase with Argon2id (19 MiB
  memory cost, 2 passes). The passphrase is asked on the lock screen once
  per app start and lives only in memory; zeroized on sign out or exit.
- After unlock, the connection manager lists profiles from the vault;
  `profiles.json` is only a fallback (pre-unlock CLI lookups) and the
  migration source. New saves go to the vault whenever it is unlocked.
- Profiles from older versions carrying plaintext passwords are migrated
  into the vault (password + sealed settings) and scrubbed from disk the
  first time the vault is unlocked.
- Bare-address quick-connect (`rstsc host`) connects without stored
  credentials; use `-p <profile>` for remembered passwords, or type them in
  the form. With the vault skipped/locked, `-p` still launches (settings
  from `profiles.json`) but sends no password.

## Security notes

- TLS certificate validation defaults to **permissive** (matches common
  self-signed lab deployments; mstsc prompts interactively). Enable
  *Advanced → Validate TLS certificate* for strict validation.
- The master passphrase is the whole secret: there is no recovery. Losing it
  means the vault entries are unreadable (delete `secrets.vault` to start
  over and re-enter passwords).
- Crash logs go to `~/.local/state/rstsc/panic.log`; they may contain
  session metadata (host address, profile name, credential lengths) but
  never passwords.

## Troubleshooting & diagnostics

- Crashes (Rust panics anywhere, including engine threads) are logged to
  `~/.local/state/rstsc/panic.log`.
- `RUST_LOG=rdp_client=debug` shows the exact NLA credentials sent
  (`user=`/`domain=`/password length only) and each dynamic resize.
- `RDP_DUMP_FRAME=<dir>` dumps the composited framebuffer as PPM images
  (1/s, capped) — verifies whether remote content reaches the client when
  the window looks wrong.

## IronRDP version policy

Pinned to IronRDP `master@a0c8c0dc1e1296f428092d3158585cfc7d395efb`
(2026-10-22) rather than a crates.io release **on purpose**. The published
releases (`ironrdp` 0.17 / `ironrdp-client` 0.1, July 2026) predate engine
features this client is built on:

- `RdpOutputEvent::DesktopUpdate` dirty-region blits (`with_desktop_updates`)
- `MonitorLayout` input events with dynamic-resize reactivation fallbacks
  (`DisplayResizeFallbackReason`, config-driven resize)
- Interactive auto-reconnect via `AutoReconnectDecision`
- `output_channel` drop policies (LatestOnly for high-frequency frames)
- Pointer `DecodedPointer` bitmaps (custom cursors)

Once a crates.io release contains these (`ironrdp-client` beyond 0.1),
switch the `[dependencies]` entries from `git`/`rev` back to plain versions
and delete the pin comment in `Cargo.toml`.

## Architecture

- `ironrdp` facade (engine `ironrdp-client`, git-pinned): connection sequence
  (X.224 → TLS/CredSSP → MCS → capabilities → activation), session loop,
  codecs, virtual channels. Events flow out on a bounded channel with
  per-event drop policy; input flows in as fast-path PDUs built by
  `ironrdp::input::Database`.
- `src/session.rs` — config build from `Profile`, engine thread,
  framebuffer blit (`DesktopUpdate` regions → shared RGBA buffer),
  monitor-layout helpers, clipboard bridge.
- `src/app.rs` — egui screens: connection manager (profiles form, vault,
  action footer) and session surface (texture, input routing, pointer
  mapping, per-screen views, dynamic-resize debounce, connection bar,
  overlays, window title).
- `src/keymap.rs` — egui physical keys → PC/AT set-1 scancodes (positional,
  US layout); numpad maps to top-row digits; `Pause` unmapped (protocol
  requires a four-event sequence scancodes can't express); modifier
  make/break are synthesized by diffing modifier state.
- `src/vault.rs` — encrypted password store (ChaCha20-Poly1305 + Argon2id).
- `src/clipboard.rs`, `src/cliprdr/` — clipboard forwarder, CLIPRDR worker
  state machine (init ordering, file transfer staging/locking), X11/Wayland
  backends.
- `src/profile.rs`, `src/theme.rs` — profile store, dark theme.

## License

MIT OR Apache-2.0 (matching IronRDP).
