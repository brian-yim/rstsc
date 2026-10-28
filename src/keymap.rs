//! egui key events → RDP set-1 scancodes (PC/AT), adapted from IronRDP's winit keymap.
//!
//! RDP expects positional (US-layout) scancodes; egui's `physical_key` carries exactly that.
//! egui has no separate numpad keys, so numpad input maps to top-row digit scancodes.

use ironrdp::input::Scancode;

/// Returns `(scancode, needs_make_synthesis)`.
/// `Pause` is deliberately unmapped: MS-RDPBCGR requires a four-event sequence the
/// scancode model can't express (same as IronRDP's own viewer).
pub fn map_key(key: egui::Key) -> Option<Scancode> {
    let scancode = match key {
        egui::Key::Escape => (false, 0x01),
        egui::Key::Num1 => (false, 0x02),
        egui::Key::Num2 => (false, 0x03),
        egui::Key::Num3 => (false, 0x04),
        egui::Key::Num4 => (false, 0x05),
        egui::Key::Num5 => (false, 0x06),
        egui::Key::Num6 => (false, 0x07),
        egui::Key::Num7 => (false, 0x08),
        egui::Key::Num8 => (false, 0x09),
        egui::Key::Num9 => (false, 0x0A),
        egui::Key::Num0 => (false, 0x0B),
        egui::Key::Minus => (false, 0x0C),
        egui::Key::Equals => (false, 0x0D),
        egui::Key::Backspace => (false, 0x0E),
        egui::Key::Tab => (false, 0x0F),
        egui::Key::Q => (false, 0x10),
        egui::Key::W => (false, 0x11),
        egui::Key::E => (false, 0x12),
        egui::Key::R => (false, 0x13),
        egui::Key::T => (false, 0x14),
        egui::Key::Y => (false, 0x15),
        egui::Key::U => (false, 0x16),
        egui::Key::I => (false, 0x17),
        egui::Key::O => (false, 0x18),
        egui::Key::P => (false, 0x19),
        egui::Key::OpenBracket => (false, 0x1A),
        egui::Key::CloseBracket => (false, 0x1B),
        egui::Key::Enter => (false, 0x1C),
        egui::Key::A => (false, 0x1E),
        egui::Key::S => (false, 0x1F),
        egui::Key::D => (false, 0x20),
        egui::Key::F => (false, 0x21),
        egui::Key::G => (false, 0x22),
        egui::Key::H => (false, 0x23),
        egui::Key::J => (false, 0x24),
        egui::Key::K => (false, 0x25),
        egui::Key::L => (false, 0x26),
        egui::Key::Semicolon => (false, 0x27),
        egui::Key::Quote => (false, 0x28),
        egui::Key::Backtick => (false, 0x29),
        egui::Key::Backslash => (false, 0x2B),
        egui::Key::Z => (false, 0x2C),
        egui::Key::X => (false, 0x2D),
        egui::Key::C => (false, 0x2E),
        egui::Key::V => (false, 0x2F),
        egui::Key::B => (false, 0x30),
        egui::Key::N => (false, 0x31),
        egui::Key::M => (false, 0x32),
        egui::Key::Comma => (false, 0x33),
        egui::Key::Period => (false, 0x34),
        egui::Key::Slash => (false, 0x35),
        egui::Key::Space => (false, 0x39),
        egui::Key::CapsLock => (false, 0x3A),
        egui::Key::F1 => (false, 0x3B),
        egui::Key::F2 => (false, 0x3C),
        egui::Key::F3 => (false, 0x3D),
        egui::Key::F4 => (false, 0x3E),
        egui::Key::F5 => (false, 0x3F),
        egui::Key::F6 => (false, 0x40),
        egui::Key::F7 => (false, 0x41),
        egui::Key::F8 => (false, 0x42),
        egui::Key::F9 => (false, 0x43),
        egui::Key::F10 => (false, 0x44),
        egui::Key::NumLock => (false, 0x45),
        egui::Key::ScrollLock => (false, 0x46),
        egui::Key::F11 => (false, 0x57),
        egui::Key::F12 => (false, 0x58),
        egui::Key::ShiftLeft => (false, 0x2A),
        egui::Key::ShiftRight => (false, 0x36),
        egui::Key::ControlLeft => (false, 0x1D),
        egui::Key::ControlRight => (true, 0x1D),
        egui::Key::AltLeft => (false, 0x38),
        egui::Key::AltRight => (true, 0x38),
        egui::Key::SuperLeft => (true, 0x5B),
        egui::Key::SuperRight => (true, 0x5C),
        egui::Key::Menu => (true, 0x5D),
        egui::Key::Home => (true, 0x47),
        egui::Key::ArrowUp => (true, 0x48),
        egui::Key::PageUp => (true, 0x49),
        egui::Key::ArrowLeft => (true, 0x4B),
        egui::Key::ArrowRight => (true, 0x4D),
        egui::Key::End => (true, 0x4F),
        egui::Key::ArrowDown => (true, 0x50),
        egui::Key::PageDown => (true, 0x51),
        egui::Key::Insert => (true, 0x52),
        egui::Key::Delete => (true, 0x53),
        egui::Key::PrintScreen => (true, 0x37),
        _ => return None,
    };
    Some(Scancode::from_u8(scancode.0, scancode.1))
}

/// Modifier synthesis: egui reports modifiers only as state, while RDP needs
/// make/break scancodes. We diff the modifier set per event and emit the delta
/// before the key event that carried it. Right-side keys are sent as left-side
/// codes when egui cannot distinguish them (merged modifiers).
#[derive(Default)]
pub struct ModifierTracker {
    last: egui::Modifiers,
}

impl ModifierTracker {
    /// Returns the most recent modifier state (for synthesizing a
    /// press/release pair when the integration consumed the key event).
    pub fn last(&self) -> Option<egui::Modifiers> {
        Some(self.last)
    }

    /// Emits scancode operations for modifiers gained/lost between `last` and `mods`.
    pub fn diff(&mut self, mods: egui::Modifiers) -> Vec<(Scancode, bool)> {
        let mut ops = Vec::new();

        let pairs = [
            (self.last.shift, mods.shift, Scancode::from_u8(false, 0x2A)),
            (self.last.ctrl, mods.ctrl, Scancode::from_u8(false, 0x1D)),
            (self.last.alt, mods.alt, Scancode::from_u8(false, 0x38)),
            (
                self.last.mac_cmd,
                mods.mac_cmd,
                Scancode::from_u8(true, 0x5B),
            ),
        ];
        for (was, is, sc) in pairs {
            if !was && is {
                ops.push((sc, true));
            } else if was && !is {
                ops.push((sc, false));
            }
        }
        self.last = mods;
        ops
    }

    /// Releases everything (called when the session surface loses focus).
    pub fn release_all(&mut self) -> Vec<(Scancode, bool)> {
        self.diff(egui::Modifiers::default())
            .into_iter()
            .map(|(sc, _)| (sc, false))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_scancodes() {
        assert_eq!(map_key(egui::Key::A).map(|s| s.as_u16()), Some(0x001E));
        assert_eq!(map_key(egui::Key::Num7).map(|s| s.as_u16()), Some(0x0008));
        assert_eq!(map_key(egui::Key::F1).map(|s| s.as_u16()), Some(0x003B));
        assert_eq!(
            map_key(egui::Key::ArrowRight).map(|s| s.as_u16()),
            Some(0xE04D)
        );
        assert_eq!(
            map_key(egui::Key::ControlRight).map(|s| s.as_u16()),
            Some(0xE01D)
        );
        assert_eq!(map_key(egui::Key::Pause), None);
    }
}
