//! Mapping between WaterUI logical points and terminal cells.
//!
//! WaterUI layout runs entirely in points (`f32`). A terminal can only address
//! whole cells, so this backend keeps layout in points and snaps frames to the
//! cell grid when they are assigned to a node.
//!
//! - One column is [`PT_PER_COL`] points wide — text is measured in display
//!   columns, so this is 1:1.
//! - One row is [`PT_PER_ROW`] points tall — chosen so that one line of text
//!   measures exactly one row and a default 10pt stack spacing snaps to one
//!   empty row.

use ratatui::layout::Rect as CellRect;
use waterui_core::layout::Rect as PtRect;

/// Logical points per terminal row.
pub const PT_PER_ROW: f32 = 8.0;

/// Logical points per terminal column.
pub const PT_PER_COL: f32 = 1.0;

/// Point height reported for a single line of text.
pub const LINE_HEIGHT: f32 = PT_PER_ROW;

const fn clamp_cell(v: f32) -> u16 {
    if v <= 0.0 {
        0
    } else if v >= u16::MAX as f32 {
        u16::MAX
    } else {
        v as u16
    }
}

/// Rounds a point width to whole terminal columns.
#[must_use]
pub(crate) fn cols_for(width: f32) -> u16 {
    clamp_cell((width / PT_PER_COL).round())
}

/// Rounds a point height to whole terminal rows (positive heights give at
/// least one row, matching `to_cells`).
#[must_use]
pub(crate) fn rows_for(height: f32) -> u16 {
    if height <= 0.0 {
        0
    } else {
        clamp_cell((height / PT_PER_ROW).round().max(1.0))
    }
}

/// Snaps a point-space rect to whole terminal cells.
///
/// Columns round to the nearest boundary. For rows the top edge is floored and
/// the height is derived from the point height, which keeps inter-view spacing
/// symmetric (a 10pt gap between one-row views always yields one empty row).
#[must_use]
pub(crate) fn to_cells(frame: PtRect) -> CellRect {
    let x = (frame.x() / PT_PER_COL).round();
    let y = (frame.y() / PT_PER_ROW).floor();
    let width = (frame.width() / PT_PER_COL).round();
    let height = if frame.height() <= 0.0 {
        0.0
    } else {
        (frame.height() / PT_PER_ROW).round().max(1.0)
    };
    CellRect::new(
        clamp_cell(x),
        clamp_cell(y),
        clamp_cell(width),
        clamp_cell(height),
    )
}
