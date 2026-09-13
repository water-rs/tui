//! Terminal theme: installs WaterUI's theme slots into an [`Environment`].
//!
//! Components resolve colors through `Store<SomeColor, Computed<ResolvedColor>>`
//! and fonts through `Store<SomeFontSlot, Computed<ResolvedFont>>`. The terminal
//! has no system palette to bridge, so this module installs a fixed dark
//! palette mapped from the usual WaterUI defaults; applications can install a
//! custom `waterui` theme on top afterwards — the slots are ordinary
//! environment entries.

use nami::Computed;
use waterui_core::Environment;
use waterui_core::env::Store;
use waterui_graphics::color::{
    AccentColor, AccentContainerColor, AccentForegroundColor, BackgroundColor, BorderColor,
    ColorScheme, ForegroundColor, MutedForegroundColor, ResolvedColor, SelectionContainerColor,
    SelectionForegroundColor, Srgb, SurfaceColor, SurfaceVariantColor, TertiaryColor,
    TertiaryContainerColor,
};
use waterui_text::font::{Body, Caption, FontSlot, Footnote, Headline, Subheadline, Title};

fn color(red: u8, green: u8, blue: u8) -> Computed<ResolvedColor> {
    Computed::constant(ResolvedColor::from_srgb(Srgb::new(
        f32::from(red) / 255.0,
        f32::from(green) / 255.0,
        f32::from(blue) / 255.0,
    )))
}

/// Installs the default terminal palette and font slots into `env`.
///
/// The palette is a dark scheme: terminals are overwhelmingly dark-mode and a
/// cell grid cannot express subtle elevation anyway, so the defaults bias
/// toward legible foreground/accent contrast.
pub fn install_terminal_theme(env: &mut Environment) {
    env.insert(Store::<BackgroundColor, _>::new(color(24, 24, 28)));
    env.insert(Store::<SurfaceColor, _>::new(color(38, 38, 44)));
    env.insert(Store::<SurfaceVariantColor, _>::new(color(50, 50, 58)));
    env.insert(Store::<BorderColor, _>::new(color(90, 90, 100)));
    env.insert(Store::<ForegroundColor, _>::new(color(235, 235, 240)));
    env.insert(Store::<MutedForegroundColor, _>::new(color(150, 150, 160)));
    env.insert(Store::<AccentColor, _>::new(color(90, 160, 250)));
    env.insert(Store::<AccentContainerColor, _>::new(color(40, 80, 140)));
    env.insert(Store::<AccentForegroundColor, _>::new(color(10, 14, 20)));
    env.insert(Store::<TertiaryColor, _>::new(color(180, 140, 250)));
    env.insert(Store::<TertiaryContainerColor, _>::new(color(70, 50, 100)));
    env.insert(Store::<SelectionContainerColor, _>::new(color(
        60, 110, 180,
    )));
    env.insert(Store::<SelectionForegroundColor, _>::new(color(
        245, 245, 250,
    )));

    env.insert(Store::<ColorScheme, _>::new(Computed::constant(
        ColorScheme::Dark,
    )));

    // Terminals render every face in the same cell grid; the type scale still
    // matters because weight is the one axis a terminal can express.
    env.insert(Store::<Body, _>::new(Computed::constant(Body::DEFAULT)));
    env.insert(Store::<Title, _>::new(Computed::constant(Title::DEFAULT)));
    env.insert(Store::<Headline, _>::new(Computed::constant(
        Headline::DEFAULT,
    )));
    env.insert(Store::<Subheadline, _>::new(Computed::constant(
        Subheadline::DEFAULT,
    )));
    env.insert(Store::<Caption, _>::new(Computed::constant(
        Caption::DEFAULT,
    )));
    env.insert(Store::<Footnote, _>::new(Computed::constant(
        Footnote::DEFAULT,
    )));
}
