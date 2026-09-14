//! Video in the terminal via the Kitty graphics protocol.
//!
//! `cargo run --example video -- path/to/video.mp4`
//!
//! ffmpeg decodes frames on a sidecar process; each RGBA frame is
//! retransmitted onto one kitty image id, so the placeholder cells in the
//! ratatui buffer keep showing the newest frame. Terminals without kitty
//! graphics fall back to half-block cells.
//!
//! Keys: `Space` pause · `←`/`→` seek ±5s · `q`/`Esc` quit.

use std::io::{self, Read as _, Write as _};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui_image::picker::{Picker, ProtocolType};
use waterui_tui::kitty::{KittyImage, draw_placeholders};

const FPS: u32 = 24;
const IMAGE_ID: u32 = 7;

struct Decoder {
    rx: Receiver<Vec<u8>>,
    child: Child,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

/// Spawns an ffmpeg sidecar emitting raw RGBA frames at `seek` seconds.
fn spawn_decoder(path: &str, px_w: u32, px_h: u32, seek: f64, fps: u32) -> io::Result<Decoder> {
    let mut child = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-ss",
            &format!("{seek}"),
            "-i",
            path,
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-s",
            &format!("{px_w}x{px_h}"),
            "-r",
            &format!("{fps}"),
            "-",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout: ChildStdout = child.stdout.take().unwrap();
    let frame_size = (px_w * px_h * 4) as usize;
    let (tx, rx): (SyncSender<Vec<u8>>, _) = sync_channel(2);
    thread::spawn(move || {
        let mut frame = vec![0u8; frame_size];
        while stdout.read_exact(&mut frame).is_ok() {
            if tx.send(frame.clone()).is_err() {
                break;
            }
        }
    });
    Ok(Decoder { rx, child })
}

/// Video duration in seconds via ffprobe.
fn probe_duration(path: &str) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            path,
        ])
        .output();
    out.ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0.0)
}

fn main() -> io::Result<()> {
    let path = std::env::args().nth(1).expect("usage: video <file>");
    let duration = probe_duration(&path);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    // Probe graphics + font metrics after entering the alternate screen.
    let picker = Picker::from_query_stdio().ok();
    let kitty = picker
        .as_ref()
        .is_some_and(|p| p.protocol_type() == ProtocolType::Kitty);
    let font = picker
        .as_ref()
        .map_or(ratatui_image::FontSize::new(8, 17), |p| p.font_size());
    let (font_w, font_h) = (font.width, font.height);

    let size = terminal.size()?;
    // Video area: centered box, 2 rows of chrome (title + status).
    let vw = size.width.saturating_sub(8);
    let vh = size.height.saturating_sub(4);
    let area = Rect::new(4, 2, vw, vh);

    let (px_w, px_h) = if kitty {
        // Transmit resolution is decoupled from display size: kitty scales the
        // image to the placeholder cell rect, so sending full font-pixel
        // resolution just wastes bandwidth. ~480px wide is plenty for cells.
        let natural_w = u32::from(vw) * u32::from(font_w);
        let natural_h = u32::from(vh) * u32::from(font_h);
        let w = natural_w.min(480);
        (w, (natural_h * w / natural_w).max(2))
    } else {
        // Half-block path: two pixel rows per terminal row.
        (u32::from(vw), u32::from(vh) * 2)
    };
    // VIDEO_PNG=1 switches transmission to PNG (`f=100`) — smaller on the
    // wire, much slower to encode; useful on bandwidth-bound links.
    let image = KittyImage::new(IMAGE_ID, px_w, px_h);
    let image = if std::env::var_os("VIDEO_PNG").is_some() {
        image.png()
    } else {
        image
    };

    let mut seek = 0.0;
    let mut decoder = spawn_decoder(&path, px_w, px_h, seek, FPS)?;
    let mut play_start = Instant::now();
    let mut paused = false;
    let mut fps_counter = (0u32, Instant::now(), 0.0f64);
    let mut last_encode_ms = 0.0f64;
    let mut last_write_ms = 0.0f64;
    let mut out = io::stdout().lock();
    let mut created = false;

    'app: loop {
        let iter_start = Instant::now();
        // Drain to the newest decoded frame; send it out-of-band after the
        // draw so ratatui's output stream stays ordered before the APCs.
        let mut frame = None;
        let mut decoder_done = false;
        if !paused {
            loop {
                match decoder.rx.try_recv() {
                    Ok(f) => frame = Some(f),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        decoder_done = true;
                        break;
                    }
                }
            }
        }
        // Loop the video at EOF so the demo keeps playing.
        if decoder_done && duration > 0.0 {
            seek = 0.0;
            play_start = Instant::now();
            drop(decoder);
            decoder = spawn_decoder(&path, px_w, px_h, 0.0, FPS)?;
        }

        terminal.draw(|f| {
            let full = f.area();
            let buf = f.buffer_mut();
            buf.set_style(
                full,
                Style::default().fg(Color::White).bg(Color::Black),
            );
            buf.set_stringn(
                4,
                0,
                format!("waterui-tui · video · {path}"),
                usize::from(full.width) - 8,
                Style::default().add_modifier(Modifier::BOLD),
            );
            let status = if paused { "⏸ paused" } else { "▶ playing" };
            let position = if paused {
                seek
            } else {
                seek + play_start.elapsed().as_secs_f64()
            };
            buf.set_stringn(
                4,
                full.height - 1,
                format!(
                    "{status} · t={position:.0}s/{duration:.0}s · {:.0}fps · enc {last_encode_ms:.0}ms · wr {last_write_ms:.0}ms · {}",
                    fps_counter.2,
                    if kitty { "kitty" } else { "half-block" },
                ),
                usize::from(full.width) - 8,
                Style::default().fg(Color::DarkGray),
            );
            if kitty {
                draw_placeholders(&image, vw, vh, area, (0, 0), buf);
            } else if let Some(rgba) = &frame {
                draw_halfblocks(rgba, px_w, px_h, area, buf);
            }
        })?;

        if kitty && let Some(rgba) = &frame {
            let t = Instant::now();
            let chunk = if created {
                image.frame(rgba, vw, vh)
            } else {
                created = true;
                image.create(rgba, vw, vh)
            };
            last_encode_ms = t.elapsed().as_secs_f64() * 1e3;
            let t = Instant::now();
            out.write_all(&chunk)?;
            out.flush()?;
            last_write_ms = t.elapsed().as_secs_f64() * 1e3;
        }

        // fps accounting
        if frame.is_some() {
            fps_counter.0 += 1;
            let elapsed = fps_counter.1.elapsed().as_secs_f64();
            if elapsed >= 1.0 {
                fps_counter.2 = fps_counter.0 as f64 / elapsed;
                fps_counter.0 = 0;
                fps_counter.1 = Instant::now();
            }
        }

        // Input + pacing: spend only the remainder of the frame budget on
        // waiting — encode+write time counts against it, so a slow frame
        // delays the next key check instead of stacking on top.
        let budget =
            Duration::from_millis(1000 / u64::from(FPS)).saturating_sub(iter_start.elapsed());
        if event::poll(budget)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => break 'app,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        break 'app;
                    }
                    KeyCode::Char(' ') => {
                        if !paused {
                            seek += play_start.elapsed().as_secs_f64();
                        }
                        paused = !paused;
                        play_start = Instant::now();
                    }
                    KeyCode::Left | KeyCode::Right => {
                        let now = seek + play_start.elapsed().as_secs_f64();
                        seek = (now + if key.code == KeyCode::Left { -5.0 } else { 5.0 })
                            .clamp(0.0, duration.max(0.0));
                        play_start = Instant::now();
                        drop(decoder);
                        decoder = spawn_decoder(&path, px_w, px_h, seek, FPS)?;
                        created = false;
                    }
                    _ => {}
                },
                Event::Resize(w, h) => {
                    let _ = (w, h); // terminal.draw relayouts; area stays fixed
                }
                _ => {}
            }
        }
    }

    if created && kitty {
        out.write_all(&image.delete())?;
        out.flush()?;
    }
    drop(out);
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

/// Half-block fallback: each cell samples two pixel rows into fg/bg.
fn draw_halfblocks(rgba: &[u8], px_w: u32, px_h: u32, area: Rect, buf: &mut Buffer) {
    let rows = (px_h / 2).min(u32::from(area.height));
    let cols = px_w.min(u32::from(area.width));
    for cy in 0..rows {
        for cx in 0..cols {
            let top = px(rgba, px_w, px_h, cx, cy * 2);
            let bottom = px(rgba, px_w, px_h, cx, (cy * 2 + 1).min(px_h - 1));
            if let Some(cell) = buf.cell_mut((area.x + cx as u16, area.y + cy as u16)) {
                cell.set_symbol("▀")
                    .set_fg(Color::Rgb(top[0], top[1], top[2]))
                    .set_bg(Color::Rgb(bottom[0], bottom[1], bottom[2]));
            }
        }
    }
}

fn px(rgba: &[u8], w: u32, _h: u32, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * w + x) * 4) as usize;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}
