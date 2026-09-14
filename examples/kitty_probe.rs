//! Minimal kitty-graphics probe: transmit one solid-color image, ask kitty to
//! report errors (q=1), capture any APC responses on stdin to
//! /tmp/kitty_resp.bin, then display a placeholder block until a keypress.
//!
//! Run inside kitty: `cargo run --example kitty_probe`

use std::fs;
use std::io::{self, Read as _, Write as _};
use std::time::Duration;

use crossterm::event::{self, Event};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use waterui_tui::kitty::{KittyImage, draw_placeholders};

const W: u32 = 64;
const H: u32 = 32;

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout().lock();
    execute!(out, EnterAlternateScreen)?;

    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let rgba: Vec<u8> = std::iter::repeat_n([255u8, 0, 0, 255], (W * H) as usize)
        .flatten()
        .collect();
    let image = KittyImage::new(7, W, H);
    // q=1: errors are still reported, OK responses suppressed.
    let create = image.create(&rgba, 40, 10);
    let seq = String::from_utf8_lossy(&create).replace("\x1b_Gq=2,", "\x1b_Gq=1,");
    out.write_all(seq.as_bytes())?;
    out.flush()?;

    let mut sink = Vec::new();
    while let Ok(chunk) = rx.recv_timeout(Duration::from_secs(2)) {
        sink.extend_from_slice(&chunk);
    }
    fs::write("/tmp/kitty_resp.bin", &sink)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    terminal.draw(|f| {
        draw_placeholders(
            &image,
            40,
            10,
            Rect::new(4, 2, 40, 10),
            (0, 0),
            f.buffer_mut(),
        );
    })?;

    loop {
        if let Event::Key(_) = event::read()? {
            break;
        }
    }
    execute!(out, LeaveAlternateScreen)?;
    disable_raw_mode()
}
