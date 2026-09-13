//! Experimental terminal backend for WaterUI.
//!
//! This crate maps WaterUI's declarative view tree onto a terminal cell grid:
//! views are dispatched into a retained [`node::Node`] tree, measured and
//! placed with the shared `waterui-layout` algorithms in logical points, then
//! quantized to whole terminal cells for drawing with `ratatui`. Keyboard,
//! mouse and resize events come from `crossterm`.
//!
//! # Supported views
//!
//! - `Text` / `StyledStr` / `Str`, including styled spans (weight, italic,
//!   underline, strikethrough, colors) and `line_limit`
//! - `FixedContainer` / `LazyContainer` — stacks, padding, frames, spacers
//!   and any other `Layout`-driven container work unchanged
//! - `Button`, `Toggle`, `TextField`
//! - `Divider`, `Color`/`ResolvedColor` fills
//! - `Gradient` — linear/radial/angular gradients are sampled per cell into
//!   `▀` half-block pairs (two gradient rows per terminal row); mesh
//!   gradients reach the backend as `GpuSurface` like every other GPU view
//! - `GpuSurface` — images and other `GpuView` content are rasterized once
//!   through an offscreen wgpu pass, then resampled into half-block cells;
//!   hosts without a GPU adapter draw a `[gpu]` placeholder
//! - `Metadata<Environment>` / `LayoutPriority` / `Retain` / `LifeCycleHook`
//!   and the accessibility metadata keys (recorded, not exposed)
//! - `Dynamic` subtrees are re-dispatched in place when their signal updates
//!
//! Keyboard: `Tab`/`Shift-Tab` move focus, `Enter`/`Space` activate the
//! focused control, `Esc`/`q`/`Ctrl-C` quit. Terminal mouse clicks focus and
//! activate the control under the cursor.
//!
//! # Entry points
//!
//! [`run`] is the batteries-included path: it installs the terminal theme,
//! takes over the terminal, and runs the event loop. For embedding into a
//! larger terminal application, drive [`TuiRenderer`] and [`node::Node`]
//! directly.

mod app;
pub mod gpu;
pub mod gradient;
pub mod node;
mod renderer;
pub mod style;
pub mod theme;
pub mod units;

pub use app::run;
pub use node::Node;
pub use renderer::TuiRenderer;
pub use theme::install_terminal_theme;
