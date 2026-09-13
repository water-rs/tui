//! Bounded terminal capability probe.
//!
//! `Picker::from_query_stdio` answers the same questions but spawns a
//! detached thread that blocks on `stdin().read()` until the terminal answers
//! every query — on terminals that ignore the kitty query (tmux without
//! passthrough, dumb PTYs, pipes) that thread outlives the probe and races
//! the event loop's stdin reader, silently eating keystrokes. This module
//! issues the queries itself and reads with a termios `VTIME` deadline, so
//! nothing is left reading stdin when it returns.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};
use rustix::termios::{self, OptionalActions, SpecialCodeIndex};

/// Total budget for collecting query responses.
const PROBE_TIMEOUT: Duration = Duration::from_millis(600);

/// Per-`read` deadline in termios `VTIME` units (tenths of a second).
const READ_VTIME: u8 = 2;

/// Builds a [`Picker`] by querying the terminal directly.
///
/// Always returns a picker: `Halfblocks` is the baseline when the terminal
/// does not answer, with env-detected iTerm2 preserved through
/// `from_fontsize`. Errors only on stdio/termios failures.
///
/// # Errors
///
/// Returns terminal I/O errors from `tcgetattr`/`tcsetattr`/`read`.
pub fn picker() -> io::Result<Picker> {
    let responses = query()?;
    let kitty = responses.iter().any(|seq| {
        seq.strip_prefix(b"_Gi=31;")
            .is_some_and(|rest| rest.starts_with(b"OK"))
    });
    let sixel = responses.iter().any(|seq| {
        seq.starts_with(b"[?")
            && seq.ends_with(b"c")
            && seq[2..seq.len() - 1]
                .split(|&b| b == b';')
                .any(|param| param == b"4")
    });
    let font_size = responses.iter().find_map(|seq| {
        let inner = seq.strip_prefix(b"[6;")?.strip_suffix(b"t")?;
        let mut parts = inner.split(|&b| b == b';');
        let height: u16 = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
        let width: u16 = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
        (width > 0 && height > 0).then(|| FontSize::new(width, height))
    });

    let mut picker = match font_size {
        // `from_fontsize` is the only public constructor taking a real cell
        // size; it also keeps the env-based iTerm2/tmux detection.
        #[allow(deprecated)]
        Some(size) => Picker::from_fontsize(size),
        None => Picker::halfblocks(),
    };
    if kitty {
        picker.set_protocol_type(ProtocolType::Kitty);
    } else if sixel {
        picker.set_protocol_type(ProtocolType::Sixel);
    }
    Ok(picker)
}

/// Sends the capability queries and collects response sequences until the
/// device-status terminator arrives or the deadline passes. Each returned
/// item is one escape sequence body with its leading `ESC` stripped.
fn query() -> io::Result<Vec<Vec<u8>>> {
    // Raw mode is already active; `VTIME` bounds each read so a silent
    // terminal costs one bounded wait instead of a leaked thread.
    let stdin = io::stdin();
    let original = termios::tcgetattr(&stdin)?;
    let mut timed = original.clone();
    timed.special_codes[SpecialCodeIndex::VMIN] = 0;
    timed.special_codes[SpecialCodeIndex::VTIME] = READ_VTIME;
    termios::tcsetattr(&stdin, OptionalActions::Now, &timed)?;
    let result = query_inner();
    termios::tcsetattr(&stdin, OptionalActions::Now, &original)?;
    result
}

fn query_inner() -> io::Result<Vec<Vec<u8>>> {
    let mut out = Vec::new();
    if std::env::var_os("TMUX").is_some() {
        // tmux consumes queries itself; wrap them in its passthrough form.
        out.extend_from_slice(b"\x1bPtmux;");
        for &byte in QUERY {
            out.push(byte);
            if byte == b'\x1b' {
                out.push(b'\x1b');
            }
        }
        out.extend_from_slice(b"\x1b\\");
    } else {
        out.extend_from_slice(QUERY);
    }
    io::stdout().write_all(&out)?;
    io::stdout().flush()?;

    let mut received = Vec::new();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut buf = [0u8; 256];
    loop {
        let read = io::stdin().read(&mut buf)?;
        if read == 0 && Instant::now() >= deadline {
            break;
        }
        received.extend_from_slice(&buf[..read]);
        if received.windows(4).any(|window| window == b"\x1b[0n") {
            break;
        }
    }
    Ok(received
        .split(|&byte| byte == b'\x1b')
        .filter(|seq| !seq.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// The query block: kitty graphics probe, device attributes (sixel flag),
/// cell-size report, and a device-status report every terminal answers so
/// the read loop can stop as soon as the queue is drained.
const QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c\x1b[16t\x1b[5n";
