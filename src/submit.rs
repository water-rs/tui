//! Field confirmation (`Enter`) as an environment payload.
//!
//! WaterUI has no cross-backend notion of "submit this field" yet. The TUI
//! backend reads [`OnSubmit`] off the environment a `TextField`/`SecureField`
//! was dispatched in and invokes it when the user presses `Enter` while the
//! field is focused. Install it on a field's subtree with
//! [`waterui_core::env::with`]:
//!
//! ```ignore
//! use waterui_core::env::with;
//! use waterui_tui::OnSubmit;
//!
//! with(field("name", &name), OnSubmit::new(move || send()));
//! ```

use std::cell::RefCell;

use waterui_core::handler::BoxedAction;

/// Action invoked when the user presses `Enter` in a `TextField` or
/// `SecureField` dispatched under this environment value.
///
/// This is a TUI-backend concept: other backends ignore the payload, so an
/// app that needs confirmation semantics everywhere should also place a real
/// `Button` alongside the field.
pub struct OnSubmit(pub RefCell<BoxedAction>);

impl OnSubmit {
    /// Wraps a `FnMut(&Environment)` action.
    pub fn new(action: impl FnMut(&waterui_core::Environment) + 'static) -> Self {
        Self(RefCell::new(Box::new(action)))
    }
}
