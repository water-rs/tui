//! A small counter/settings demo for the TUI backend.
//!
//! Run with `cargo run --example demo`. `Tab`/`Shift-Tab` move focus,
//! `Enter`/`Space` activate, `Esc`/`Ctrl-C` quit.

use std::io;

use nami::{Binding, SignalExt, binding};
use waterui_controls::{button, field, toggle};
use waterui_core::Str;
use waterui_graphics::color::{Color, MutedForegroundColor};
use waterui_layout::divider::Divider;
use waterui_layout::padding::{EdgeInsets, Padding};
use waterui_layout::spacer::spacer;
use waterui_layout::stack::{hstack, vstack};
use waterui_text::styled::StyledStr;
use waterui_text::text::text;

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

    let view = Padding::new(
        EdgeInsets::all(8.0),
        vstack((
            text(StyledStr::plain("WaterUI terminal demo").bold()),
            text(counter_text),
            Divider,
            hstack((decrement, spacer(), increment)).spacing(2.0),
            toggle("Enable counting", &enabled),
            field("Name", &name),
            text(greeting.computed()),
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
