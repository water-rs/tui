//! Projection of WaterUI colors and text styles onto terminal attributes.

use nami::{Computed, Signal};
use ratatui::style::{Color, Modifier, Style};
use waterui_core::Environment;
use waterui_graphics::color::{
    AccentColor, AccentForegroundColor, BackgroundColor, BorderColor, ForegroundColor,
    MutedForegroundColor, ResolvedColor, SelectionContainerColor, SelectionForegroundColor,
    SurfaceColor,
};
use waterui_text::font::FontWeight;

/// Converts a resolved WaterUI color to a 24-bit terminal color.
#[must_use]
pub fn tui_color(color: ResolvedColor) -> Color {
    let srgb = color.to_srgb_with_headroom();
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color::Rgb(channel(srgb.red), channel(srgb.green), channel(srgb.blue))
}

/// Composites `src` (linear space + opacity) over an existing cell color.
///
/// Terminals cannot blend colors; sampled paints such as gradients and image
/// pixels are approximated by mixing in sRGB space over the color already
/// underneath the cell.
#[must_use]
pub fn composite_over(src: ResolvedColor, under: Color) -> Color {
    let srgb = src.to_srgb_with_headroom();
    let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    composite_rgb8(
        channel(srgb.red),
        channel(srgb.green),
        channel(srgb.blue),
        (src.opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
        under,
    )
}

/// Composites an sRGB `rgba8` source over an existing cell color.
#[must_use]
pub fn composite_rgb8(r: u8, g: u8, b: u8, a: u8, under: Color) -> Color {
    if a == u8::MAX {
        return Color::Rgb(r, g, b);
    }
    let Color::Rgb(ur, ug, ub) = under else {
        return Color::Rgb(r, g, b);
    };
    let alpha = f32::from(a) / 255.0;
    let mix = |s: u8, d: u8| (f32::from(s) * alpha + f32::from(d) * (1.0 - alpha)).round() as u8;
    Color::Rgb(mix(r, ur), mix(g, ug), mix(b, ub))
}

/// The color a cell shows through its background slot: its `bg`, falling back
/// to the theme background when the cell carries `Color::Reset`.
#[must_use]
pub fn cell_under(cell_bg: Color, theme_bg: Color) -> Color {
    match (cell_bg, theme_bg) {
        (Color::Reset, Color::Reset) => Color::Black,
        (Color::Reset, theme) => theme,
        (cell, _) => cell,
    }
}

/// The theme slots a frame needs, resolved once per draw.
///
/// The underlying values are `Computed`, so a theme that swaps its signals
/// updates the terminal palette on the next frame without rebuilding views.
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    /// Default text color.
    pub foreground: Color,
    /// De-emphasized text (placeholders, labels).
    pub muted: Color,
    /// Accent color for focused controls and links.
    pub accent: Color,
    /// Text color drawn on accent fills.
    pub accent_foreground: Color,
    /// Terminal background.
    pub background: Color,
    /// Slightly raised surface (field insets, toggles).
    pub surface: Color,
    /// Divider and border color.
    pub border: Color,
    /// Focused/selected fill.
    pub selection: Color,
    /// Text drawn on the selection fill.
    pub selection_foreground: Color,
}

impl Theme {
    /// Resolves every needed slot from the environment.
    #[must_use]
    pub fn resolve(env: &Environment) -> Self {
        fn slot<K: 'static>(env: &Environment) -> Option<Color> {
            env.query::<K, Computed<ResolvedColor>>()
                .map(|signal| tui_color(signal.get()))
        }
        Self {
            foreground: slot::<ForegroundColor>(env).unwrap_or(Color::White),
            muted: slot::<MutedForegroundColor>(env).unwrap_or(Color::Gray),
            accent: slot::<AccentColor>(env).unwrap_or(Color::Cyan),
            accent_foreground: slot::<AccentForegroundColor>(env).unwrap_or(Color::Black),
            background: slot::<BackgroundColor>(env).unwrap_or(Color::Reset),
            surface: slot::<SurfaceColor>(env).unwrap_or(Color::DarkGray),
            border: slot::<BorderColor>(env).unwrap_or(Color::Gray),
            selection: slot::<SelectionContainerColor>(env).unwrap_or(Color::Blue),
            selection_foreground: slot::<SelectionForegroundColor>(env).unwrap_or(Color::White),
        }
    }

    /// The base style for ordinary text.
    #[must_use]
    pub const fn text(&self) -> Style {
        Style::new().fg(self.foreground)
    }
}

/// Resolves a [`waterui_text::styled::Style`] chunk into a terminal style.
#[must_use]
pub fn chunk_style(
    text_style: &waterui_text::styled::Style,
    env: &Environment,
    base: Style,
) -> Style {
    let mut style = base;
    let font = text_style.font.resolve(env).get();
    if matches!(
        font.weight,
        FontWeight::SemiBold | FontWeight::Bold | FontWeight::UltraBold | FontWeight::Black
    ) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if text_style.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if text_style.underline {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if text_style.strikethrough {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }
    if let Some(foreground) = &text_style.foreground {
        style = style.fg(tui_color(foreground.resolve(env).get()));
    }
    if let Some(background) = &text_style.background {
        style = style.bg(tui_color(background.resolve(env).get()));
    }
    style
}
