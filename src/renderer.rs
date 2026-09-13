//! Dispatches WaterUI views into the terminal [`Node`] tree.
//!
//! The renderer is a [`ViewDispatcher`] configured with handlers for every
//! view type this backend realizes natively; anything else expands through
//! `View::body` (stacks, padding, labels, composition views) until it reaches
//! a registered leaf.

use std::cell::{Cell, RefCell};
use std::num::NonZeroUsize;
use std::rc::Rc;

use nami::{Computed, Signal};
use waterui_backend_core::dispatcher::ViewDispatcher;
use waterui_controls::button::ButtonConfig;
use waterui_controls::text_field::ResolvedTextFieldConfig;
use waterui_controls::toggle::ToggleConfig;
use waterui_core::accessibility::{
    AccessibilityChildren, AccessibilityHidden, AccessibilityIdentifier, AccessibilityLabel,
    AccessibilityRole, AccessibilityState, AccessibilityStateSignal, AccessibilityValue,
};
use waterui_core::event::{LifeCycle, LifeCycleHook, OnEvent};
use waterui_core::gesture::GestureObserver;
use waterui_core::layout::{HorizontalAlignment, LayoutPriority, StretchAxis};
use waterui_core::views::{SharedAnyViews, Views};
use waterui_core::{AnyView, Dynamic, Environment, Metadata, Native, Retain, Str, View};
use waterui_graphics::color::{Color, ResolvedColor};
use waterui_graphics::gradient_renderer::ResolvedGradient;
use waterui_graphics::{GpuRuntime, GpuSurface};
use waterui_icon::SystemIcon;
use waterui_layout::container::{FixedContainer, LazyContainer};
use waterui_layout::divider::Divider;
use waterui_layout::spacer::{Spacer, SpacerLayout};
use waterui_layout::stack::Axis;
use waterui_text::styled::StyledStr;
use waterui_text::text::TextConfig;

use crate::gpu::GpuState;
use crate::node::{FieldState, Kind, LazyState, Node};

/// Lazy `GpuRuntime` initialization: `Untried` until the first `GpuSurface`
/// is dispatched, then either `Ready` or `Unavailable` for the rest of the
/// renderer's life.
enum GpuInit {
    Untried,
    Ready(GpuRuntime),
    Unavailable,
}

/// State carried by the dispatcher — reachable from every handler.
pub struct TuiState {
    /// Set by signal watchers; the event loop redraws when raised.
    pub dirty: Rc<Cell<bool>>,
    next_focus: Cell<u32>,
    appear: Vec<(LifeCycleHook, Environment)>,
    gpu: RefCell<GpuInit>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            dirty: Rc::new(Cell::new(true)),
            next_focus: Cell::new(0),
            appear: Vec::new(),
            gpu: RefCell::new(GpuInit::Untried),
        }
    }
}

impl TuiState {
    fn next_focus(&self) -> u32 {
        let id = self.next_focus.get();
        self.next_focus.set(id + 1);
        id
    }

    /// Subscribes `signal` to the dirty flag; the guard lives on `node`.
    fn watch<S: Signal>(&self, signal: &S, node: &mut Node) {
        let dirty = self.dirty.clone();
        node.guards
            .push(Box::new(signal.watch(move |_| dirty.set(true))));
    }

    /// Drains pending `Appear` lifecycle hooks.
    pub fn take_appear_hooks(&mut self) -> Vec<(LifeCycleHook, Environment)> {
        core::mem::take(&mut self.appear)
    }

    /// Returns the shared GPU runtime, initializing it on first use.
    ///
    /// `None` means no usable GPU adapter exists on this host; GPU-backed
    /// nodes then draw a placeholder instead of panicking.
    fn gpu(&self) -> Option<GpuRuntime> {
        {
            let mut init = self.gpu.borrow_mut();
            if matches!(*init, GpuInit::Untried) {
                *init = match pollster::block_on(GpuRuntime::new()) {
                    Ok(runtime) => GpuInit::Ready(runtime),
                    Err(error) => {
                        tracing::warn!("GPU runtime unavailable: {error}");
                        GpuInit::Unavailable
                    }
                };
            }
        }
        match &*self.gpu.borrow() {
            GpuInit::Ready(runtime) => Some(runtime.clone()),
            _ => None,
        }
    }
}

/// The handler context: a pointer back to the dispatcher so container
/// handlers can dispatch their children recursively.
#[derive(Clone, Copy)]
pub struct TuiCtx {
    ptr: *mut ViewDispatcher<TuiState, TuiCtx, Node>,
}

impl TuiCtx {
    /// Dispatches a child view to a node.
    ///
    /// Handlers are `Fn` (not `FnMut`) and the handler table never changes
    /// during dispatch, so re-entering through this raw pointer only ever
    /// produces disjoint `&mut` access to `TuiState` and shared access to the
    /// handler table. The GTK backend uses the same pattern for its renderer.
    fn dispatch(&self, view: impl View, env: &Environment) -> Node {
        // SAFETY: see the type-level note. The dispatcher outlives every
        // dispatch call chain and is not structurally mutated during it.
        unsafe { (&mut *self.ptr).dispatch(view, env, *self) }
    }
}

/// The terminal renderer: dispatches a view tree once, then lets nodes update
/// themselves through nami signals.
pub struct TuiRenderer {
    dispatcher: Box<ViewDispatcher<TuiState, TuiCtx, Node>>,
}

impl Default for TuiRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl TuiRenderer {
    /// Creates a renderer with every supported view handler registered.
    #[must_use]
    pub fn new() -> Self {
        let mut renderer = Self {
            dispatcher: Box::new(ViewDispatcher::with_state(TuiState::default())),
        };
        renderer.register_handlers();
        renderer
    }

    fn ctx(&self) -> TuiCtx {
        TuiCtx {
            ptr: (&*self.dispatcher as *const ViewDispatcher<TuiState, TuiCtx, Node>).cast_mut(),
        }
    }

    /// The dirty flag raised by watchers.
    #[must_use]
    pub fn dirty(&self) -> Rc<Cell<bool>> {
        self.dispatcher.state().dirty.clone()
    }

    /// Drains `Appear` lifecycle hooks collected during the last dispatch.
    pub fn take_appear_hooks(&mut self) -> Vec<(LifeCycleHook, Environment)> {
        self.dispatcher.state_mut().take_appear_hooks()
    }

    /// Whether this host can rasterize `GpuSurface` content (images, mesh
    /// gradients, shader surfaces). `false` means those nodes draw a
    /// placeholder.
    pub fn gpu_available(&self) -> bool {
        self.dispatcher.state().gpu().is_some()
    }

    /// Dispatches a view into the root node.
    pub fn dispatch<V: View>(&mut self, view: V, env: &Environment) -> Node {
        self.dispatcher.dispatch(view, env, self.ctx())
    }

    fn register_handlers(&mut self) {
        let d = self.dispatcher.as_mut();

        // ---- Leaves -----------------------------------------------------

        d.register::<Native<()>>(|_state, _ctx, _view, env| Node::empty(env));

        d.register::<Native<Str>>(|_state, _ctx, view, env| {
            let stretch = view.stretch_axis();
            let mut node = Node::new(
                Kind::Text {
                    content: Computed::constant(StyledStr::plain(view.into_inner())),
                    alignment: Computed::constant(HorizontalAlignment::Leading),
                    line_limit: None,
                },
                env,
            );
            node.stretch = stretch;
            node
        });

        d.register::<Native<TextConfig>>(|state, _ctx, view, env| {
            let stretch = view.stretch_axis();
            let config = view.into_inner();
            let mut node = Node::new(
                Kind::Text {
                    content: config.content.clone(),
                    alignment: config.paragraph_alignment.clone(),
                    line_limit: config.line_limit.map(NonZeroUsize::get),
                },
                env,
            );
            node.stretch = stretch;
            state.watch(&config.content, &mut node);
            state.watch(&config.paragraph_alignment, &mut node);
            node
        });

        d.register::<Native<Spacer>>(|_state, _ctx, view, env| {
            let stretch = view.stretch_axis();
            let mut node = Node::new(
                Kind::Container(Box::new(SpacerLayout::from(view.into_inner()))),
                env,
            );
            node.stretch = stretch;
            node
        });

        d.register::<Divider>(|_state, _ctx, _view, env| {
            // Stacks install their axis into the child environment; a divider
            // inside an `HStack` draws vertically.
            let vertical = matches!(env.get::<Axis>(), Some(Axis::Horizontal));
            let mut node = Node::new(Kind::Divider { vertical }, env);
            node.stretch = if vertical {
                StretchAxis::Vertical
            } else {
                StretchAxis::Horizontal
            };
            node
        });

        d.register::<Native<Color>>(|state, _ctx, view, env| {
            let stretch = view.stretch_axis();
            let signal = view.into_inner().resolve(env);
            let mut node = Node::new(Kind::Fill(signal.clone()), env);
            node.stretch = stretch;
            state.watch(&signal, &mut node);
            node
        });

        d.register::<Native<ResolvedColor>>(|_state, _ctx, view, env| {
            let stretch = view.stretch_axis();
            let mut node = Node::new(Kind::Fill(Computed::constant(view.into_inner())), env);
            node.stretch = stretch;
            node
        });

        d.register::<Native<ResolvedGradient>>(|_state, _ctx, view, env| {
            let stretch = view.stretch_axis();
            let mut node = Node::new(Kind::Gradient(view.into_inner()), env);
            node.stretch = stretch;
            node
        });

        d.register::<Native<GpuSurface>>(|state, _ctx, view, env| {
            let stretch = view.stretch_axis();
            let mut node = Node::new(
                Kind::Gpu(GpuState::new(view.into_inner(), state.gpu(), env)),
                env,
            );
            node.stretch = stretch;
            node
        });

        // There is no OS icon catalog on a terminal; rather than rendering
        // nothing, the icon surfaces as a bracketed name so the asymmetry is
        // visible to the author.
        d.register::<Native<SystemIcon>>(|_state, _ctx, view, env| {
            let icon = view.into_inner();
            let mut node = Node::new(
                Kind::Text {
                    content: Computed::constant(StyledStr::plain(format!("[{}]", icon.name))),
                    alignment: Computed::constant(HorizontalAlignment::Leading),
                    line_limit: Some(1),
                },
                env,
            );
            node.stretch = StretchAxis::None;
            node
        });

        // ---- Controls ---------------------------------------------------

        d.register::<Native<ButtonConfig>>(|state, ctx, view, env| {
            let stretch = view.stretch_axis();
            let config = view.into_inner();
            let mut node = Node::new(
                Kind::Button {
                    action: RefCell::new(config.action),
                    style: config.style,
                },
                env,
            );
            node.children.push(ctx.dispatch(config.label, env));
            node.focus = Some(state.next_focus());
            node.stretch = stretch;
            node
        });

        d.register::<Native<ToggleConfig>>(|state, ctx, view, env| {
            let stretch = view.stretch_axis();
            let config = view.into_inner();
            let mut node = Node::new(
                Kind::Toggle {
                    value: config.toggle.clone(),
                    style: config.style,
                },
                env,
            );
            node.children.push(ctx.dispatch(config.label, env));
            node.focus = Some(state.next_focus());
            node.stretch = stretch;
            state.watch(&config.toggle, &mut node);
            node
        });

        d.register::<Native<ResolvedTextFieldConfig>>(|state, _ctx, view, env| {
            let stretch = view.stretch_axis();
            let config = view.into_inner();
            let label = config.label.accessibility_label();
            let cursor = config.value.get().to_plain().chars().count();
            let mut node = Node::new(
                Kind::Field(FieldState {
                    label: label.clone(),
                    value: config.value.clone(),
                    prompt: config.prompt.content.clone(),
                    cursor: Cell::new(cursor),
                }),
                env,
            );
            node.focus = Some(state.next_focus());
            node.stretch = stretch;
            state.watch(&config.value, &mut node);
            state.watch(&config.prompt.content, &mut node);
            state.watch(&label, &mut node);
            node
        });

        // ---- Containers ---------------------------------------------------

        d.register::<Native<FixedContainer>>(|_state, ctx, view, env| {
            let stretch = view.stretch_axis();
            let (layout, contents) = view.into_inner().into_inner();
            let mut node = Node::new(Kind::Container(layout), env);
            node.children = contents
                .into_iter()
                .map(|view| ctx.dispatch(view, env))
                .collect();
            node.stretch = stretch;
            node
        });

        d.register::<Native<LazyContainer>>(|state, ctx, view, env| {
            let stretch = view.stretch_axis();
            let (layout, contents) = view.into_inner().into_inner();
            let contents = SharedAnyViews::from(contents);
            let children = Rc::new(RefCell::new(materialize(&contents, ctx, env)));

            // Rebuild materialized children in place when the collection
            // changes. Node state under the replaced range is discarded —
            // the same trade-off `Dynamic` makes.
            let children_slot = children.clone();
            let contents_slot = contents.clone();
            let env_slot = env.clone();
            let dirty = state.dirty.clone();
            let guard = contents.watch(.., move |_| {
                *children_slot.borrow_mut() = materialize(&contents_slot, ctx, &env_slot);
                dirty.set(true);
            });

            let mut node = Node::new(
                Kind::Lazy(LazyState {
                    layout,
                    contents,
                    children,
                }),
                env,
            );
            node.stretch = stretch;
            node.guards.push(Box::new(guard));
            node
        });

        d.register::<Native<Dynamic>>(|state, ctx, view, env| {
            let stretch = view.stretch_axis();
            let slot = Rc::new(RefCell::new(Node::empty(env)));
            let slot_in_hook = slot.clone();
            let env_in_hook = env.clone();
            let dirty = state.dirty.clone();
            view.into_inner().connect(move |new_view| {
                *slot_in_hook.borrow_mut() = ctx.dispatch(new_view.into_value(), &env_in_hook);
                dirty.set(true);
            });
            let mut node = Node::new(Kind::Dynamic(slot), env);
            node.stretch = stretch;
            node
        });

        // ---- Metadata ------------------------------------------------------

        d.register::<Metadata<Environment>>(|_state, ctx, metadata, _env| {
            ctx.dispatch(metadata.content, &metadata.value)
        });

        d.register::<Metadata<LayoutPriority>>(|_state, ctx, metadata, env| {
            let mut node = ctx.dispatch(metadata.content, env);
            node.priority = metadata.value.get();
            node
        });

        d.register::<Metadata<Retain>>(|_state, ctx, metadata, env| {
            let mut node = ctx.dispatch(metadata.content, env);
            node.retained.push(metadata.value);
            node
        });

        d.register::<Metadata<LifeCycleHook>>(|state, ctx, metadata, env| {
            // `Appear` runs after the first drawn frame; `Disappear` hooks are
            // retained on the node and would need a drop-env to run — the TUI
            // experiment keeps them alive but never fires them.
            if metadata.value.lifecycle() == LifeCycle::Appear {
                state.appear.push((metadata.value, env.clone()));
            }
            ctx.dispatch(metadata.content, env)
        });

        // Accessibility metadata is recorded semantics: a terminal has no
        // screen reader, so it passes straight through to the content.
        macro_rules! passthrough {
            ($($ty:ty),* $(,)?) => {
                $(d.register::<Metadata<$ty>>(|_state, ctx, metadata, env| {
                    ctx.dispatch(metadata.content, env)
                });)*
            };
        }
        passthrough!(
            AccessibilityLabel,
            AccessibilityValue,
            AccessibilityIdentifier,
            AccessibilityRole,
            AccessibilityHidden,
            AccessibilityChildren,
            AccessibilityState,
            AccessibilityStateSignal,
            OnEvent,
            GestureObserver,
        );

        // `IgnorableMetadata<T>` unwraps itself in `body`, so it never needs a
        // handler.
    }
}

fn materialize(contents: &SharedAnyViews<AnyView>, ctx: TuiCtx, env: &Environment) -> Vec<Node> {
    let len = contents.len().get();
    (0..len)
        .filter_map(|index| contents.get_view(index))
        .map(|view| ctx.dispatch(view, env))
        .collect()
}
