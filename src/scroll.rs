//! Scroll position reporting as an environment payload.
//!
//! WaterUI's `ScrollController` is write-only: an app can ask a `scroll` view
//! to move, but has no way to learn where it currently rests — which a chat
//! transcript needs to decide whether to keep pinning itself to the bottom as
//! new content streams in. The TUI backend reads [`OnScroll`] off the
//! environment a `scroll` view was dispatched in and invokes it whenever the
//! view's offset or scrollable extent changes. Install it with
//! [`waterui_core::env::with`]:
//!
//! ```ignore
//! use waterui_core::env::with;
//! use waterui_tui::OnScroll;
//!
//! with(scroll(content), OnScroll::new(move |metrics| {
//!     at_bottom.set(metrics.at_end());
//! }));
//! ```

use std::cell::RefCell;

use ratatui::layout::Rect;

/// A vertical shift a scroll view applied between the previously presented
/// frame and the one being rendered.
///
/// Scroll nodes push these into [`crate::node::DrawCtx::scroll_ops`] while
/// rendering; the presentation step replays each as a hardware scroll-region
/// command (`DECSTBM` + `SU`/`SD`) plus a matching rotation of the previous
/// frame buffer, so the cell diff only has to repaint the newly exposed rows
/// instead of every cell in the viewport.
#[derive(Debug, Clone, Copy)]
pub struct ScrollOp {
    /// The viewport band on screen (cell coordinates) whose content shifted.
    pub region: Rect,
    /// Offset change since the last presented frame, in cell columns/rows.
    /// Positive `y` means the view moved down (content moved up).
    pub delta: (i32, i32),
}

/// A scroll view's position at the moment [`OnScroll`] fired.
#[derive(Debug, Clone, Copy)]
pub struct ScrollMetrics {
    /// Current scroll offset in cell columns/rows.
    pub offset: (i32, i32),
    /// Largest reachable offset — content extent minus the viewport, clamped
    /// at zero — in cell columns/rows.
    pub max: (i32, i32),
}

impl ScrollMetrics {
    /// Whether the view is pinned to the scrollable end on both axes.
    pub fn at_end(&self) -> bool {
        self.offset.0 >= self.max.0 && self.offset.1 >= self.max.1
    }
}

/// Action invoked when a `scroll` view's offset or extent changes.
///
/// This is a TUI-backend concept: other backends ignore the payload. The
/// callback runs on the UI thread during event/render dispatch; keep it cheap
/// and defer real work through signals.
pub struct OnScroll(pub RefCell<Box<dyn FnMut(ScrollMetrics)>>);

impl OnScroll {
    /// Wraps a `FnMut(ScrollMetrics)` action.
    pub fn new(action: impl FnMut(ScrollMetrics) + 'static) -> Self {
        Self(RefCell::new(Box::new(action)))
    }
}
