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

/// Lucide "eye" / "eye-off" (white strokes) for the password reveal toggle,
/// decoded eagerly into egui textures at startup. No async bytes loader, no
/// load failure path — the ⚠️-placeholder bug is impossible with textures.
mod icons {
    pub const EYE: &[u8] = include_bytes!("../assets/icons/eye.png");
    pub const EYE_OFF: &[u8] = include_bytes!("../assets/icons/eye-off.png");

    fn decode(bytes: &[u8]) -> egui::ColorImage {
        let img = image::load_from_memory(bytes).expect("embedded eye icon is a valid PNG");
        egui::ColorImage::from_rgba_unmultiplied(
            [img.width() as usize, img.height() as usize],
            img.to_rgba8().as_raw(),
        )
    }
    pub fn eye() -> egui::ColorImage { decode(EYE) }
    pub fn eye_off() -> egui::ColorImage { decode(EYE_OFF) }
}

/// Handles to both preloaded icon textures.
pub struct EyeIcons {
    pub masked: egui::TextureHandle,    // eye (password hidden)
    pub revealed: egui::TextureHandle,  // eye-off (password visible)
}

pub fn load_eye_icons(ctx: &egui::Context) -> EyeIcons {
    EyeIcons {
        masked: ctx.load_texture("rstsc-eye", icons::eye(), egui::TextureOptions::LINEAR),
        revealed: ctx.load_texture("rstsc-eye-off", icons::eye_off(), egui::TextureOptions::LINEAR),
    }
}

/// The password-reveal image: Lucide eye (password masked) or eye-off
/// (revealed), drawn 16×16 with muted tint, accent while hovered.
pub fn eye_icon(icons: &EyeIcons, hidden: bool, hovered: bool) -> egui::Image<'static> {
    egui::Image::from_texture(egui::load::SizedTexture::new(
        if hidden { &icons.masked } else { &icons.revealed },
        egui::Vec2::splat(16.0),
    ))
    .tint(if hovered { ACCENT_HOVER } else { TEXT_MUTED })
}
