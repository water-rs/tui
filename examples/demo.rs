//! A small counter/settings demo for the TUI backend.
//!
//! Run with `cargo run --example demo`. `Tab`/`Shift-Tab` move focus,
//! `Enter`/`Space` activate, `Esc`/`Ctrl-C` quit.

use std::io;

use nami::{Binding, SignalExt, binding};
use waterui_controls::{button, field, toggle};
use waterui_core::Str;
use waterui_graphics::color::{Color, MutedForegroundColor, ResolvedColor, Srgb};
use waterui_graphics::gradient_renderer::Gradient;
use waterui_layout::divider::Divider;
use waterui_layout::frame::Frame;
use waterui_layout::padding::{EdgeInsets, Padding};
use waterui_layout::spacer::spacer;
use waterui_layout::stack::{hstack, vstack};
use waterui_text::styled::StyledStr;
use waterui_text::text::text;

fn rgb(r: u8, g: u8, b: u8) -> ResolvedColor {
    ResolvedColor::from_srgb(Srgb::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
    ))
}

fn main() -> io::Result<()> {
    let counter: Binding<i32> = binding(0);
    let enabled = binding(true);
    let name = binding(Str::from_static(""));

    let decrement = {
        let counter = counter.clone();
        button("Decrement").action(move || counter.set(counter.get() - 1))
    };
    let increment = {
        let counter = counter.clone();
        button("Increment").action(move || counter.set(counter.get() + 1))
    };

    let counter_text = counter
        .clone()
        .map(|value| format!("Count: {value}"))
        .computed();

    let greeting = name.clone().map(|name: Str| {
        if name.is_empty() {
            "Hello!".to_owned()
        } else {
            format!("Hello, {name}!")
        }
    });

    let banner = Gradient::linear(
        vec![(0.0, rgb(90, 160, 250)), (1.0, rgb(180, 140, 250))],
        [0.0, 0.5],
        [1.0, 0.5],
    );
    let mesh = Gradient::mesh(
        3,
        2,
        vec![
            ([0.0, 0.0], rgb(255, 90, 90)),
            ([0.5, 0.0], rgb(255, 200, 80)),
            ([1.0, 0.0], rgb(120, 220, 120)),
            ([0.0, 1.0], rgb(80, 140, 255)),
            ([0.5, 1.0], rgb(200, 120, 255)),
            ([1.0, 1.0], rgb(90, 220, 220)),
        ],
        true,
    );

    let view = Padding::new(
        EdgeInsets::all(8.0),
        vstack((
            text(StyledStr::plain("WaterUI terminal demo").bold()),
            Frame::new(banner).height(16.0),
            text(counter_text),
            Divider,
            hstack((decrement, spacer(), increment)).spacing(2.0),
            toggle("Enable counting", &enabled),
            field("Name", &name),
            text(greeting.computed()),
            Frame::new(mesh).height(24.0),
            spacer(),
            text(
                StyledStr::plain("Tab moves focus · Enter/Space activates · Esc quits")
                    .foreground(Color::new(MutedForegroundColor)),
            ),
        ))
        .spacing(10.0),
    );

    waterui_tui::run(view)
}
