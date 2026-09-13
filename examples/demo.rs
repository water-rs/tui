//! A small counter/settings demo for the TUI backend.
//!
//! Run with `cargo run --example demo`. `Tab`/`Shift-Tab` move focus,
//! `Enter`/`Space` activate, `Esc`/`Ctrl-C` quit.

use std::io;

use nami::{Binding, SignalExt, binding};
use waterui_controls::{button, field, slider, stepper, toggle};
use waterui_core::Str;
use waterui_form::secure::{Secure, secure};
use waterui_graphics::color::{Color, MutedForegroundColor, ResolvedColor, Srgb};
use waterui_graphics::gradient_renderer::Gradient;
use waterui_internal::component::progress::progress;
use waterui_layout::divider::Divider;
use waterui_layout::frame::Frame;
use waterui_layout::padding::{EdgeInsets, Padding};
use waterui_layout::scroll::scroll;
use waterui_layout::spacer::spacer;
use waterui_layout::stack::{hstack, vstack};
use waterui_navigation::{NavigationView, Tab, Tabs};
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

    let banner = Gradient::linear(
        vec![(0.0, rgb(90, 160, 250)), (1.0, rgb(180, 140, 250))],
        [0.0, 0.5],
        [1.0, 0.5],
    );

    let volume = binding(0.4f64);
    let quantity = binding(2i32);
    let password = binding(Secure::new(String::new()));
    let tab = binding(0i32);

    let controls_tab = {
        let counter = counter.clone();
        let enabled = enabled.clone();
        let name = name.clone();
        let volume = volume.clone();
        let quantity = quantity.clone();
        let password = password.clone();
        move || {
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
            NavigationView::new(
                "Controls",
                vstack((
                    text(counter_text),
                    Divider,
                    hstack((decrement, spacer(), increment)).spacing(2.0),
                    toggle("Enable counting", &enabled),
                    slider("Volume", &volume)
                        .min_value_label("0")
                        .max_value_label("1"),
                    stepper("Quantity", &quantity).range(0..=99),
                    field("Name", &name),
                    secure("Password", &password),
                    text(greeting.computed()),
                    progress(volume.clone().map(|v| v).computed()),
                    Frame::new(mesh).height(24.0),
                ))
                .spacing(10.0),
            )
        }
    };

    let view = Padding::new(
        EdgeInsets::all(8.0),
        vstack((
            text(StyledStr::plain("WaterUI terminal demo").bold()),
            Frame::new(banner).height(16.0),
            Tabs::new(
                &tab,
                vec![
                    Tab::new(0, "Controls", controls_tab),
                    Tab::new(1, "Log", || {
                        let log_lines: Vec<_> = (1..=30)
                            .map(|i| {
                                text(Str::from(format!("log line {i} — wheel or ↑↓ to scroll")))
                            })
                            .collect();
                        NavigationView::new("Log", scroll(vstack(log_lines).spacing(0.0)))
                    }),
                ],
            ),
            spacer(),
            text(
                StyledStr::plain("Tab moves focus · ←→ adjusts · wheel scrolls · Esc quits")
                    .foreground(Color::new(MutedForegroundColor)),
            ),
        ))
        .spacing(10.0),
    );

    waterui_tui::run(view)
}
