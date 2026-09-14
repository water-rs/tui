//! Frame presentation: scroll-region replay, cell diff, cursor, flush.
//!
//! Ratatui's `Terminal` can't host this — the hardware-scroll replay must
//! mutate the *previous* buffer before the diff runs, and `Terminal` never
//! exposes it — so the event loop owns its own `prev`/`cur` buffer pair and
//! presents through this module. The single invariant is
//! `prev buffer == physical screen`: every byte emitted to the backend is
//! mirrored into `prev`, then `prev.diff_iter(cur)` emits only the cells the
//! scroll could not provide (newly exposed rows, the scrollbar column, and
//! any content that changed under the scroll).

use ratatui::backend::Backend;
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;

use crate::scroll::ScrollOp;

/// Presents `cur`: replays eligible scroll ops as hardware scrolls, writes the
/// remaining cell diff, then places the cursor. `resized` means the buffers
/// were just rebuilt — nothing on screen corresponds to `prev` anymore, so
/// scroll replay is skipped and the diff repaints everything.
pub fn present<B: Backend>(
    backend: &mut B,
    prev: &mut Buffer,
    cur: &Buffer,
    ops: &[ScrollOp],
    cursor: Option<(u16, u16)>,
    resized: bool,
) -> Result<(), B::Error> {
    // The diff writer hops the hardware cursor to every run it prints; with
    // the field cursor left visible those hops flicker as stray blocks.
    backend.hide_cursor()?;
    if !resized {
        replay_scroll_ops(backend, prev, ops)?;
    }
    backend.draw(prev.diff_iter(cur))?;
    if let Some(position) = cursor {
        backend.set_cursor_position(position)?;
        backend.show_cursor()?;
    }
    backend.flush()
}

/// Replays each eligible [`ScrollOp`] on the backend and applies the matching
/// row rotation to `prev`, keeping it consistent with the physical screen.
/// Ineligible ops (horizontal scroll, page jumps, partial-width or overlapping
/// regions) are skipped entirely — the subsequent diff repaints them.
fn replay_scroll_ops<B: Backend>(
    backend: &mut B,
    prev: &mut Buffer,
    ops: &[ScrollOp],
) -> Result<(), B::Error> {
    let screen = prev.area;
    let mut eligible: Vec<bool> = ops
        .iter()
        .map(|op| {
            let (dx, dy) = op.delta;
            // Without DECLRMM the scroll region spans whole rows, so only a
            // full-width region can scroll without smearing neighbours.
            dx == 0
                && dy != 0
                && dy.unsigned_abs() < u32::from(op.region.height)
                && op.region.x == screen.x
                && op.region.width == screen.width
        })
        .collect();
    // Two scrolled regions sharing rows can't both rotate — a nested scroll
    // inside a scrolled parent hits this. Skip the overlapping pair; the diff
    // repaints both.
    for i in 0..ops.len() {
        for j in (i + 1)..ops.len() {
            if eligible[i]
                && eligible[j]
                && ops[i].region.top() < ops[j].region.bottom()
                && ops[j].region.top() < ops[i].region.bottom()
            {
                eligible[i] = false;
                eligible[j] = false;
            }
        }
    }
    for (op, ok) in ops.iter().zip(eligible) {
        if !ok {
            continue;
        }
        let dy = op.delta.1;
        let rows = op.region.top()..op.region.bottom();
        if dy > 0 {
            backend.scroll_region_up(rows, dy as u16)?;
        } else {
            backend.scroll_region_down(rows, (-dy) as u16)?;
        }
        scroll_buffer_region(prev, op.region, dy);
    }
    Ok(())
}

/// Mirrors a hardware scroll inside `buf`: row `y` takes what was at
/// `y + dy` (positive `dy` = content scrolled up, so its source is further
/// down); rows whose source left the region are blanked. Rows are visited in
/// the direction that reads each source row before a destination overwrites
/// it: top-down when scrolling up, bottom-up when scrolling down.
fn scroll_buffer_region(buf: &mut Buffer, region: Rect, dy: i32) {
    let cols = usize::from(region.width);
    for i in 0..region.height {
        let y = if dy > 0 {
            region.top() + i
        } else {
            region.bottom() - 1 - i
        };
        let src_y = i32::from(y) + dy;
        let dst = buf.index_of(region.x, y);
        if src_y >= i32::from(region.top()) && src_y < i32::from(region.bottom()) {
            let src = buf.index_of(region.x, src_y as u16);
            for i in 0..cols {
                buf.content[dst + i] = buf.content[src + i].clone();
            }
        } else {
            buf.content[dst..dst + cols].fill(Cell::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::rc::Rc;

    use ratatui::backend::CrosstermBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::{present, scroll_buffer_region};
    use crate::scroll::ScrollOp;

    /// A writer shared with the test so emitted ANSI bytes can be inspected;
    /// `CrosstermBackend::writer` is unstable, so capture goes through `Write`.
    #[derive(Clone, Default)]
    struct Capture(Rc<RefCell<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.borrow_mut().flush()
        }
    }

    fn grid(buf: &mut Buffer, cols: u16, rows: u16) {
        for y in 0..rows {
            for x in 0..cols {
                buf.cell_mut((x, y))
                    .unwrap()
                    .set_char(char::from_digit(u32::from(y) % 10, 10).unwrap());
            }
        }
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (buf.area.left()..buf.area.right())
            .map(|x| buf.cell((x, y)).unwrap().symbol().chars().next().unwrap())
            .collect()
    }

    /// Removes CSI sequences (`ESC [` … final byte `0x40..=0x7E`), leaving the
    /// printed cell text so tests can assert exactly what was drawn.
    fn strip_csi(out: &str) -> String {
        let mut text = String::new();
        let mut chars = out.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' && chars.next_if_eq(&'[').is_some() {
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            } else {
                text.push(c);
            }
        }
        text
    }

    #[test]
    fn scroll_buffer_region_up_shifts_and_blanks_tail() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 5));
        grid(&mut buf, 4, 5);
        scroll_buffer_region(&mut buf, Rect::new(0, 1, 4, 4), 2);
        assert_eq!(row_text(&buf, 0), "0000"); // outside region, untouched
        assert_eq!(row_text(&buf, 1), "3333"); // got row 3
        assert_eq!(row_text(&buf, 2), "4444"); // got row 4
        assert_eq!(row_text(&buf, 3), "    "); // exposed, blanked
        assert_eq!(row_text(&buf, 4), "    ");
    }

    #[test]
    fn scroll_buffer_region_down_shifts_and_blanks_head() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 5));
        grid(&mut buf, 4, 5);
        scroll_buffer_region(&mut buf, Rect::new(0, 1, 4, 4), -1);
        assert_eq!(row_text(&buf, 1), "    "); // exposed, blanked
        assert_eq!(row_text(&buf, 2), "1111"); // got row 1
        assert_eq!(row_text(&buf, 4), "3333");
    }

    #[test]
    fn present_emits_scroll_and_only_repaints_exposed_rows() {
        let capture = Capture::default();
        let mut backend = CrosstermBackend::new(capture.clone());
        let screen = Rect::new(0, 0, 6, 4);
        let mut prev = Buffer::empty(screen);
        let mut cur = Buffer::empty(screen);
        grid(&mut prev, 6, 4);
        // Current frame = content scrolled up by 1 within the region: row y
        // shows old row y+1, and the bottom row carries brand-new content.
        for y in 0..3 {
            for x in 0..6 {
                let cell = prev.cell((x, y + 1)).unwrap().clone();
                *cur.cell_mut((x, y)).unwrap() = cell;
            }
        }
        for x in 0..6 {
            cur.cell_mut((x, 3)).unwrap().set_char('n');
        }

        present(
            &mut backend,
            &mut prev,
            &cur,
            &[ScrollOp {
                region: screen,
                delta: (0, 1),
            }],
            None,
            false,
        )
        .unwrap();

        let bytes = capture.0.borrow();
        let out = String::from_utf8_lossy(&bytes);
        // DECSTBM rows 1..4, then SU by 1, then reset.
        assert!(
            out.contains("\u{1b}[1;4r\u{1b}[1S\u{1b}[r"),
            "missing scroll seq: {out:?}"
        );
        // prev was rotated to match: only the exposed bottom row diffs.
        for y in 0..3 {
            for x in 0..6 {
                assert_eq!(
                    prev.cell((x, y)).unwrap().symbol(),
                    cur.cell((x, y)).unwrap().symbol()
                );
            }
        }
        assert_eq!(
            strip_csi(&out),
            "nnnnnn",
            "only the exposed row's cells should be written: {out:?}"
        );
    }

    #[test]
    fn present_skips_partial_width_region() {
        let capture = Capture::default();
        let mut backend = CrosstermBackend::new(capture.clone());
        let mut prev = Buffer::empty(Rect::new(0, 0, 8, 4));
        let cur = Buffer::empty(Rect::new(0, 0, 8, 4));
        grid(&mut prev, 8, 4);

        present(
            &mut backend,
            &mut prev,
            &cur,
            &[ScrollOp {
                region: Rect::new(2, 0, 6, 4),
                delta: (0, 1),
            }],
            None,
            false,
        )
        .unwrap();

        let bytes = capture.0.borrow();
        let out = String::from_utf8_lossy(&bytes);
        assert!(
            !out.contains("\u{1b}[1;4r") && !out.contains("[1S") && !out.contains("[1T"),
            "scroll seq must not be emitted: {out:?}"
        );
        // prev untouched — the full diff repaints.
        assert_eq!(row_text(&prev, 0), "00000000");
    }
}
