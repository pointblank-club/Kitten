//! The design tokens: colours, type and sizes. Colour carries meaning here, so it is never
//! decoration: amber is the one interactive colour, green means verified, red means stop, and
//! the partition colours avoid all three.

use std::sync::Arc;

use bootshift_core::layout::Content;
use eframe::egui::{
    self, Color32, FontData, FontDefinitions, FontFamily, FontId, FontTweak,
    epaint::text::VariationCoords,
};

// A warm light palette. Surfaces get lighter as they rise: the work area ground is the greyest,
// panels sit a shade lighter, and cards are white. Amber still carries interaction, green means
// verified, red means stop.
pub const VOID: Color32 = Color32::from_rgb(0xec, 0xea, 0xe4);
pub const DECK: Color32 = Color32::from_rgb(0xf6, 0xf4, 0xf0);
pub const DECK_HIGH: Color32 = Color32::from_rgb(0xff, 0xff, 0xff);
pub const RULE: Color32 = Color32::from_rgb(0xd8, 0xd5, 0xcd);
pub const RULE_SOFT: Color32 = Color32::from_rgb(0xe6, 0xe3, 0xdc);
pub const RULE_HOVER: Color32 = Color32::from_rgb(0xb9, 0xb5, 0xab);

pub const INK: Color32 = Color32::from_rgb(0x21, 0x1f, 0x1b);
pub const DIM: Color32 = Color32::from_rgb(0x63, 0x60, 0x59);
pub const FAINT: Color32 = Color32::from_rgb(0x95, 0x92, 0x8a);

pub const SIGNAL: Color32 = Color32::from_rgb(0xd9, 0x82, 0x0a);
pub const SIGNAL_HOVER: Color32 = Color32::from_rgb(0xf0, 0x94, 0x12);
pub const SIGNAL_DIM: Color32 = Color32::from_rgb(0xa3, 0x5f, 0x08);
pub const SIGNAL_INK: Color32 = Color32::from_rgb(0x2a, 0x1c, 0x06);
pub const VERIFIED: Color32 = Color32::from_rgb(0x15, 0x8f, 0x56);
pub const STOP: Color32 = Color32::from_rgb(0xc9, 0x30, 0x2a);

/// The disk bar's ground, which is also the free-space colour.
pub const BAR_GROUND: Color32 = Color32::from_rgb(0xe4, 0xe2, 0xdb);
/// Block labels keep a light fill because they are drawn over a dark halo, so they stay legible
/// on both the pale block backgrounds and the stronger used-space fills.
pub const BLOCK_TEXT: Color32 = Color32::from_rgb(0xfb, 0xfa, 0xf8);
pub const LEADER: Color32 = Color32::from_rgb(0xc2, 0xbf, 0xb6);

pub fn alpha(colour: Color32, share: f32) -> Color32 {
    let [r, g, b, _] = colour.to_array();
    Color32::from_rgba_unmultiplied(r, g, b, (share * 255.0).round() as u8)
}

/// `colour` composited over `ground` at `share`, like CSS `color-mix`.
pub fn mix(colour: Color32, share: f32, ground: Color32) -> Color32 {
    let blend = |a: u8, b: u8| (a as f32 * share + b as f32 * (1.0 - share)).round() as u8;
    Color32::from_rgb(
        blend(colour.r(), ground.r()),
        blend(colour.g(), ground.g()),
        blend(colour.b(), ground.b()),
    )
}

/// A partition's full-strength colour, used for its used-space fill and its swatches.
pub fn content_colour(content: Content) -> Color32 {
    match content {
        Content::Ntfs => Color32::from_rgb(0x5b, 0x6e, 0xe1),
        Content::Ext4 => Color32::from_rgb(0x22, 0xb8, 0xc7),
        Content::Btrfs => Color32::from_rgb(0xc8, 0x6b, 0xd6),
        Content::Xfs => Color32::from_rgb(0xe0, 0x60, 0x9a),
        Content::Swap => Color32::from_rgb(0xa6, 0x89, 0x6b),
        Content::EfiSystem | Content::BitLocker => Color32::from_rgb(0x8b, 0x8b, 0x95),
        Content::Opaque => Color32::from_rgb(0x9a, 0x98, 0xa2),
    }
}

/// A partition block's background: its colour mixed into the ground, so the used fill stands
/// out against the block's own free space. Locked system partitions stay at full strength.
pub fn block_colour(content: Content) -> Color32 {
    let share = match content {
        Content::Ext4 | Content::Btrfs | Content::Xfs => 0.22,
        Content::Ntfs | Content::Swap => 0.26,
        Content::Opaque => 0.42,
        Content::EfiSystem | Content::BitLocker => return content_colour(content),
    };
    mix(content_colour(content), share, VOID)
}

/// The three typefaces and the weights the design uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Face {
    /// Archivo: prose, headings, labels, buttons.
    Sans(Weight),
    /// JetBrains Mono: every number, size, sector and device path.
    Mono(Weight),
    /// Ubuntu Mono: the recovery console only, because it mirrors a real Linux console.
    Console { bold: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Medium,
    SemiBold,
    Bold,
}

impl Weight {
    fn value(self) -> f32 {
        match self {
            Weight::Regular => 400.0,
            Weight::Medium => 500.0,
            Weight::SemiBold => 600.0,
            Weight::Bold => 700.0,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Weight::Regular => "400",
            Weight::Medium => "500",
            Weight::SemiBold => "600",
            Weight::Bold => "700",
        }
    }
}

const WEIGHTS: [Weight; 4] = [
    Weight::Regular,
    Weight::Medium,
    Weight::SemiBold,
    Weight::Bold,
];

impl Face {
    pub fn family(self) -> FontFamily {
        FontFamily::Name(self.family_name().into())
    }

    fn family_name(self) -> String {
        match self {
            Face::Sans(weight) => format!("sans-{}", weight.key()),
            Face::Mono(weight) => format!("mono-{}", weight.key()),
            Face::Console { bold: false } => "console".to_string(),
            Face::Console { bold: true } => "console-bold".to_string(),
        }
    }

    pub fn at(self, size: f32) -> FontId {
        FontId::new(size, self.family())
    }
}

pub const SANS: Face = Face::Sans(Weight::Regular);
pub const MONO: Face = Face::Mono(Weight::Regular);

static ARCHIVO: &[u8] = include_bytes!("../assets/fonts/Archivo.ttf");
static JETBRAINS_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono.ttf");
static UBUNTU_MONO: &[u8] = include_bytes!("../assets/fonts/UbuntuMono-Regular.ttf");
static UBUNTU_MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/UbuntuMono-Bold.ttf");

/// Registers one family per face. Archivo and JetBrains Mono are variable fonts, so each
/// weight is the same file with its `wght` axis set; glyphs a face lacks fall back to egui's
/// built-in fonts.
pub fn fonts() -> FontDefinitions {
    let mut definitions = FontDefinitions::default();
    let fallbacks = definitions
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();

    let mut add = |face: Face, bytes: &'static [u8], coords: VariationCoords| {
        let key = face.family_name();
        let tweak = FontTweak {
            coords,
            ..FontTweak::default()
        };
        definitions.font_data.insert(
            key.clone(),
            Arc::new(FontData::from_static(bytes).tweak(tweak)),
        );
        let mut chain = vec![key, "mono-400".to_string()];
        chain.extend(fallbacks.iter().cloned());
        definitions.families.insert(face.family(), chain);
    };
    for weight in WEIGHTS {
        let wght = weight.value();
        add(
            Face::Sans(weight),
            ARCHIVO,
            VariationCoords::new([(b"wght", wght), (b"wdth", 100.0)]),
        );
        add(
            Face::Mono(weight),
            JETBRAINS_MONO,
            VariationCoords::new([(b"wght", wght.min(800.0))]),
        );
    }
    add(
        Face::Console { bold: false },
        UBUNTU_MONO,
        VariationCoords::default(),
    );
    add(
        Face::Console { bold: true },
        UBUNTU_MONO_BOLD,
        VariationCoords::default(),
    );

    // egui's own widgets (the disk picker's list, tooltips) use the default families.
    for (family, face) in [
        (FontFamily::Proportional, "sans-400"),
        (FontFamily::Monospace, "mono-400"),
    ] {
        let mut chain = vec![face.to_string()];
        chain.extend(fallbacks.iter().cloned());
        definitions.families.insert(family, chain);
    }
    definitions
}

/// Sizes from the design, in pixels at its 15px root.
pub mod size {
    pub const TITLE: f32 = 25.5;
    pub const TITLE_NARROW: f32 = 20.0;
    pub const LEDE: f32 = 14.0;
    pub const HEADING: f32 = 12.0;
    pub const BUTTON: f32 = 13.0;
    pub const DATA: f32 = 11.5;
    pub const STAT: f32 = 22.5;
}

pub mod layout {
    pub const RAIL: f32 = 202.0;
    pub const READOUT: f32 = 262.0;
    /// Below this the readout moves under the work area.
    pub const STACK_READOUT_BELOW: f32 = 1180.0;
    /// Below this the rail becomes a strip across the top.
    pub const NARROW_BELOW: f32 = 820.0;
    pub const LEDE_MAX: f32 = 660.0;
}

/// egui's own widgets in the app's colours, for the few places they show (the disk picker).
pub fn visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::light();
    visuals.panel_fill = VOID;
    visuals.window_fill = DECK;
    visuals.extreme_bg_color = VOID;
    visuals.faint_bg_color = DECK;
    visuals.override_text_color = Some(INK);
    visuals.selection.bg_fill = alpha(SIGNAL, 0.25);
    visuals.selection.stroke = egui::Stroke::new(1.0, SIGNAL);
    visuals.hyperlink_color = SIGNAL;
    visuals.window_stroke = egui::Stroke::new(1.0, RULE);
    for (state, fill, stroke) in [
        (&mut visuals.widgets.inactive, DECK_HIGH, RULE),
        (&mut visuals.widgets.hovered, DECK_HIGH, RULE_HOVER),
        (&mut visuals.widgets.active, DECK_HIGH, SIGNAL),
        (&mut visuals.widgets.open, DECK_HIGH, SIGNAL),
        (&mut visuals.widgets.noninteractive, DECK, RULE),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = egui::Stroke::new(1.0, stroke);
        state.fg_stroke = egui::Stroke::new(1.0, INK);
        state.corner_radius = egui::CornerRadius::same(3);
    }
    visuals
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_partition_is_drawn_in_a_colour_that_means_something() {
        for content in [
            Content::EfiSystem,
            Content::Ext4,
            Content::Ntfs,
            Content::Btrfs,
            Content::Xfs,
            Content::Swap,
            Content::BitLocker,
            Content::Opaque,
        ] {
            let colour = content_colour(content);
            for reserved in [SIGNAL, VERIFIED, STOP] {
                assert_ne!(colour, reserved, "{content:?}");
            }
        }
    }

    #[test]
    fn a_block_is_its_colour_mixed_into_the_void() {
        assert_eq!(
            block_colour(Content::Ext4),
            mix(content_colour(Content::Ext4), 0.22, VOID)
        );
        assert_eq!(
            block_colour(Content::EfiSystem),
            content_colour(Content::EfiSystem)
        );
    }
}
