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
