#!/usr/bin/env bash
# rstsc installer — builds from source and installs system- or user-wide.
# RPM-free counterpart to packaging/rpm/rstsc.spec.
#
# Usage:
#   ./install.sh              # build + install to /usr/local (asks sudo when needed)
#   ./install.sh --user       # install to ~/.local (no root)
#   ./install.sh --prefix DIR # install under DIR
#   ./install.sh PREFIX       # same, positional form
#   ./install.sh --uninstall [PREFIX]
#
# Requirements: git, gcc, a C toolchain, (optional) alsa-lib-devel for audio,
# Rust >= 1.94 from rustup if the distro rustc is older.

set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"

prefix=""
uninstall=0
need_sudo=0

die() { printf 'install.sh: error: %s\n' "$*" >&2; exit 1; }
info() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

usage() { sed -n '2,14p' "$0"; exit 0; }

while [ $# -gt 0 ]; do
  case "$1" in
    --user) prefix="${HOME}/.local" ;;
    --prefix) [ $# -ge 2 ] || die "--prefix needs a directory"; prefix="$2"; shift ;;
    --uninstall) uninstall=1 ;;
    --help|-h) usage ;;
    -*) die "unknown option: $1" ;;
    *) [ -z "$prefix" ] || die "prefix given twice"; prefix="$1" ;;
  esac
  shift
done

# --- Uninstall ---------------------------------------------------------------
if [ "$uninstall" = 1 ]; then
  [ -n "$prefix" ] || prefix=/usr/local
  targets=(
    "$prefix/bin/rstsc"
    "$prefix/share/applications/rstsc.desktop"
  )
  for s in 16 24 32 48 64 96 128 256 512; do
    targets+=("$prefix/share/icons/hicolor/${s}x${s}/apps/rstsc.png")
  done
  case "$prefix" in
    "$HOME"*) ;; # user prefix needs no sudo
    *) [ -w "$prefix" ] || need_sudo=1 ;;
  esac
  sudo_cmd=(); [ "$need_sudo" = 1 ] && sudo_cmd=(sudo)
  for f in "${targets[@]}"; do
    if [ -e "$f" ]; then
      info "removing $f"
      "${sudo_cmd[@]}" rm -f "$f"
    fi
  done
  if command -v update-desktop-database >/dev/null 2>&1; then
    "${sudo_cmd[@]}" update-desktop-database "$prefix/share/applications" 2>/dev/null || true
  fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    "${sudo_cmd[@]}" gtk-update-icon-cache -qtf "$prefix/share/icons/hicolor" 2>/dev/null || true
  fi
  info "uninstalled"
  exit 0
fi

cd "$script_dir"

# --- Dependency checks -------------------------------------------------------
missing=()
for cmd in git; do
  command -v "$cmd" >/dev/null 2>&1 || missing+=("$cmd")
done
command -v cc >/dev/null 2>&1 || command -v gcc >/dev/null 2>&1 || \
  command -v clang >/dev/null 2>&1 || missing+=(a-C-compiler)
if [ ${#missing[@]} -ne 0 ]; then
  die "missing required commands: ${missing[*]} (install a C toolchain and git first)"
fi

# Rust >= 1.94: prefer rustup, fall back to distro rustc, install rustup if absent.
rust_ok() { # $1 = path to rustc
  [ -n "$1" ] || return 1
  ver="$("$1" --version 2>/dev/null | sed -E 's/^rustc ([0-9]+\.[0-9]+).*/\1/')" || return 1
  # head -1 of "1.94 <ver>" sorted is the smallest: equals 1.94 only when ver >= 1.94.
  [ "$(printf '1.94\n%s\n' "$ver" | sort -V | head -1)" = "1.94" ]
}
rust_bin=""
for candidate in "$HOME/.cargo/bin/rustc" "$(command -v rustc 2>/dev/null || true)"; do
  if rust_ok "$candidate"; then
    rust_bin="$(command -v "$candidate")"
    break
  fi
done
if [ -z "$rust_bin" ]; then
  info "no rustc >= 1.94 found; installing rustup (minimal profile)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  export PATH="$HOME/.cargo/bin:$PATH"
  rust_bin="$HOME/.cargo/bin/rustc"
fi
export PATH="$(dirname "$rust_bin"):$PATH"
info "using $($rust_bin --version)"

# ALSA headers: optional (--features audio). Build audio when available.
features=""
if pkg-config --exists alsa 2>/dev/null || [ -e /usr/include/alsa/asoundlib.h ]; then
  features="audio"
  info "ALSA headers found: building with audio support"
else
  info "ALSA headers not found: building WITHOUT audio (dnf install alsa-lib-devel or"
  info "apt install libasound2-dev to get it; run the script again afterwards)"
fi

# --- Build -------------------------------------------------------------------
info "building rstsc (release)${features:+ (--features $features)}"
cargo build --release ${features:+--features "$features"} \
  || die "build failed — network access is required on the first build"

# --- Install paths -----------------------------------------------------------
# Default: /usr/local when writable or root; otherwise ~/.local — install without
# prompting.
if [ -z "$prefix" ]; then
  if [ "$(id -u)" = 0 ] || [ -w /usr/local ]; then
    prefix=/usr/local
    need_sudo=0
  else
    prefix="$HOME/.local"
    need_sudo=0
    info "/usr/local is not writable; falling back to $prefix (use a --prefix or run as root to change)"
  fi
fi

# Other non-writable (non-$HOME) prefixes need sudo for the system files.
if [ "$need_sudo" != 1 ]; then
  case "$prefix" in
    "$HOME"*) ;;
    *) [ -w "$prefix" ] || need_sudo=1 ;;
  esac
fi
sudo_cmd=(); [ "$need_sudo" = 1 ] && sudo_cmd=(sudo)

info "installing to $prefix (root files$([ $need_sudo = 1 ] && echo ' via sudo' || echo ''))"

install_bin() { # path mode dest
  "${sudo_cmd[@]}" install -D -m "$2" "$1" "$3"
}

install_bin target/release/rstsc 755 "$prefix/bin/rstsc"
install_bin assets/rstsc.desktop 644 "$prefix/share/applications/rstsc.desktop"
install_bin assets/rstsc.png 644 "$prefix/share/icons/hicolor/512x512/apps/rstsc.png"
for s in 256 128 96 64 48 32 24 16; do
  install_bin "assets/rstsc-${s}.png" 644 \
    "$prefix/share/icons/hicolor/${s}x${s}/apps/rstsc.png"
done

if command -v update-desktop-database >/dev/null 2>&1; then
  "${sudo_cmd[@]}" update-desktop-database "$prefix/share/applications" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
  "${sudo_cmd[@]}" gtk-update-icon-cache -qtf "$prefix/share/icons/hicolor" 2>/dev/null || true
fi

# Make ~/.local/bin visible if it is not on PATH.
case "$prefix" in
  "$HOME/.local"*)
    case ":$PATH:" in
      *":$HOME/.local/bin:"*) ;;
      *)
        info "note: add '$HOME/.local/bin' to PATH to use rstsc"
        for rc in "$HOME/.bashrc" "$HOME/.profile"; do
          if [ -e "$rc" ] && ! grep -qs '\.local/bin' "$rc"; then
            printf 'export PATH="$HOME/.local/bin:$PATH"\n' >> "$rc"
            info "added PATH line to $rc"
            break
          fi
        done
        ;;
    esac
    ;;
esac

info "done. Run 'rstsc' (or *.rdp / rdp:// links) from your applications menu."
