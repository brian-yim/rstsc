//! Dark theme for the app, tuned for long remote-desktop sessions:
//! near-black surfaces, one blue accent, restrained states.

pub const BG: egui::Color32 = egui::Color32::from_rgb(0x14, 0x16, 0x1A);
pub const BG_PANEL: egui::Color32 = egui::Color32::from_rgb(0x1B, 0x1E, 0x24);
pub const BG_WIDGET: egui::Color32 = egui::Color32::from_rgb(0x24, 0x28, 0x30);
pub const BG_HOVER: egui::Color32 = egui::Color32::from_rgb(0x2C, 0x31, 0x3A);
pub const STROKE: egui::Color32 = egui::Color32::from_rgb(0x2B, 0x30, 0x38);
pub const TEXT: egui::Color32 = egui::Color32::from_rgb(0xE9, 0xEB, 0xEE);
pub const TEXT_MUTED: egui::Color32 = egui::Color32::from_rgb(0x9A, 0xA1, 0xAB);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x4F, 0x8C, 0xFF);
pub const ACCENT_HOVER: egui::Color32 = egui::Color32::from_rgb(0x67, 0x9D, 0xFF);
pub const DANGER: egui::Color32 = egui::Color32::from_rgb(0xE5, 0x5B, 0x5B);

pub fn apply(ctx: &egui::Context) {
    ctx.all_styles_mut(|style| {
        let v = &mut style.visuals;
        *v = egui::Visuals::dark();
        v.panel_fill = BG;
        v.extreme_bg_color = egui::Color32::from_rgb(0x0E, 0x10, 0x13); // text fields
        v.faint_bg_color = BG_PANEL; // stripes/hover fills
        v.window_fill = BG_PANEL;
        v.override_text_color = Some(TEXT);
        v.window_corner_radius = egui::CornerRadius::same(8);
        v.menu_corner_radius = egui::CornerRadius::same(6);

        let base = egui::style::WidgetVisuals {
            bg_fill: BG_WIDGET,
            weak_bg_fill: BG_WIDGET,
            bg_stroke: egui::Stroke::new(1.0, STROKE),
            fg_stroke: egui::Stroke::new(1.0, TEXT),
            corner_radius: egui::CornerRadius::same(6),
            expansion: 0.0,
        };
        v.widgets.noninteractive = base;
        v.widgets.inactive = base;
        v.widgets.hovered = egui::style::WidgetVisuals {
            bg_fill: BG_HOVER,
            weak_bg_fill: BG_HOVER,
            bg_stroke: egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.4)),
            fg_stroke: egui::Stroke::new(1.0, TEXT),
            corner_radius: egui::CornerRadius::same(6),
            expansion: 0.0,
        };
        v.widgets.active = egui::style::WidgetVisuals {
            bg_fill: ACCENT.gamma_multiply(0.25),
            weak_bg_fill: ACCENT.gamma_multiply(0.25),
            bg_stroke: egui::Stroke::new(1.0, ACCENT),
            fg_stroke: egui::Stroke::new(1.2, TEXT),
            corner_radius: egui::CornerRadius::same(6),
            expansion: 0.0,
        };
        v.selection.bg_fill = ACCENT.gamma_multiply(0.35);
        v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
        v.hyperlink_color = ACCENT;

        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 6.0);
        style.spacing.menu_margin = egui::Margin::symmetric(6, 6);
    });
}

/// Accent-filled primary button.
pub fn primary_button(text: &str) -> egui::Button {
    egui::Button::new(
        egui::RichText::new(text)
            .color(egui::Color32::WHITE)
            .strong(),
    )
    .fill(ACCENT)
    .corner_radius(egui::CornerRadius::same(6))
    .min_size(egui::vec2(120.0, 32.0))
}

/// Lucide "eye" / "eye-off" (white stroke) for the password reveal toggle.
/// Embedded PNG bytes, cached by egui's bytes loader; tinted at draw.
mod icons {
    pub const EYE: &[u8] = include_bytes!("../assets/icons/eye.png");
    pub const EYE_OFF: &[u8] = include_bytes!("../assets/icons/eye-off.png");
}

/// The password-reveal icon: Lucide eye (password masked) or eye-off
/// (revealed), 16×16, sized for the ghost-square button at the call site.
/// `hovered` requests the accent tint, otherwise muted.
pub fn eye_icon(hidden: bool, hovered: bool) -> egui::Image<'static> {
    let bytes = if hidden { icons::EYE } else { icons::EYE_OFF };
    // Distinct cache URIs: switching icons must not hit the other's cache.
    let uri = if hidden { "bytes://rstsc/eye.png" } else { "bytes://rstsc/eye-off.png" };
    egui::Image::from_bytes(uri, bytes)
        .fit_to_exact_size(egui::Vec2::splat(16.0))
        .tint(tint_for(hovered))
}

fn tint_for(hovered: bool) -> egui::Color32 {
    if hovered { ACCENT_HOVER } else { TEXT_MUTED }
}
