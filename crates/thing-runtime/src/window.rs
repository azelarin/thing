//! read-only state handles for windows a configuration declares. the host
//! publishes what the compositor configured; the handle's requests are
//! event-only actions.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use mlua::{Error, Lua, Result, Table, Value};

use crate::{
    Action,
    state::{Scalar, Signal, Tracker},
};

/// at most one handle per window, and a configuration has at most 64 surfaces.
const MAX_WINDOW_STATES: usize = 64;

/// what the compositor most recently configured for one window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowState {
    /// whether the window is mapped. a hidden or closed window is not open.
    pub open: bool,
    /// the accepted content size in logical pixels, or zero before the first
    /// configure.
    pub width: u32,
    pub height: u32,
    pub maximized: bool,
    pub fullscreen: bool,
    pub activated: bool,
    /// whether the compositor advertises maximizing and fullscreen. without
    /// the advertisement both are assumed available.
    pub can_maximize: bool,
    pub can_fullscreen: bool,
    /// whether the compositor draws decorations for the window. thing draws
    /// none of its own.
    pub decorated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowRequest {
    Maximized(bool),
    Fullscreen(bool),
}

#[derive(Clone)]
struct WindowSignals {
    open: usize,
    width: usize,
    height: usize,
    maximized: usize,
    fullscreen: usize,
    activated: usize,
    can_maximize: usize,
    can_fullscreen: usize,
    decorated: usize,
    handle: Table,
}

#[derive(Default)]
pub(crate) struct WindowStates {
    declared: BTreeMap<String, WindowSignals>,
}

impl WindowStates {
    pub(crate) fn keys(&self) -> impl Iterator<Item = &String> {
        self.declared.keys()
    }

    /// writes one window's state. a key without a handle has nothing to update.
    pub(crate) fn set(
        &self,
        tracker: &RefCell<Tracker>,
        key: &str,
        state: WindowState,
    ) -> Result<()> {
        let Some(signals) = self.declared.get(key) else {
            return Ok(());
        };
        tracker.borrow_mut().write_batch(vec![
            (signals.open, Scalar::Boolean(state.open)),
            (signals.width, Scalar::Number(f64::from(state.width))),
            (signals.height, Scalar::Number(f64::from(state.height))),
            (signals.maximized, Scalar::Boolean(state.maximized)),
            (signals.fullscreen, Scalar::Boolean(state.fullscreen)),
            (signals.activated, Scalar::Boolean(state.activated)),
            (signals.can_maximize, Scalar::Boolean(state.can_maximize)),
            (
                signals.can_fullscreen,
                Scalar::Boolean(state.can_fullscreen),
            ),
            (signals.decorated, Scalar::Boolean(state.decorated)),
        ])
    }
}

pub(crate) fn install(
    lua: &Lua,
    ui: &Table,
    tracker: Rc<RefCell<Tracker>>,
    registry: Rc<RefCell<WindowStates>>,
    actions: Rc<RefCell<Vec<Action>>>,
    allowed: Rc<Cell<bool>>,
    open: Rc<Cell<bool>>,
) -> Result<()> {
    ui.set(
        "window_state",
        lua.create_function(move |lua, value: Value| {
            if !open.get() {
                return Err(Error::runtime(
                    "window state handles must be created while the configuration loads",
                ));
            }
            let Value::String(key) = value else {
                return Err(Error::runtime("ui.window_state expects a window key"));
            };
            let key = key.to_str()?.to_owned();
            if key.is_empty() || key.len() > 256 {
                return Err(Error::runtime(
                    "window keys must be nonempty strings of at most 256 bytes",
                ));
            }
            if let Some(signals) = registry.borrow().declared.get(&key) {
                return Ok(signals.handle.clone());
            }
            if registry.borrow().declared.len() >= MAX_WINDOW_STATES {
                return Err(Error::runtime(format!(
                    "a configuration may create at most {MAX_WINDOW_STATES} window state handles"
                )));
            }
            let handle = lua.create_table()?;
            let mut ids = Vec::new();
            for (name, initial) in [
                ("open", Scalar::Boolean(false)),
                ("width", Scalar::Number(0.0)),
                ("height", Scalar::Number(0.0)),
                ("maximized", Scalar::Boolean(false)),
                ("fullscreen", Scalar::Boolean(false)),
                ("activated", Scalar::Boolean(false)),
                ("can_maximize", Scalar::Boolean(false)),
                ("can_fullscreen", Scalar::Boolean(false)),
                ("decorated", Scalar::Boolean(false)),
            ] {
                let id = tracker.borrow_mut().insert(initial)?;
                handle.set(
                    name,
                    Signal {
                        id,
                        tracker: tracker.clone(),
                        readonly: true,
                    },
                )?;
                ids.push(id);
            }
            for (name, request) in [
                (
                    "set_maximized",
                    WindowRequest::Maximized as fn(bool) -> WindowRequest,
                ),
                ("set_fullscreen", WindowRequest::Fullscreen),
            ] {
                let key = key.clone();
                let actions = actions.clone();
                let allowed = allowed.clone();
                handle.set(
                    name,
                    lua.create_function(move |_, value: Value| {
                        let Value::Boolean(value) = value else {
                            return Err(Error::runtime(format!("{name} expects a boolean")));
                        };
                        crate::services::enqueue(
                            &actions,
                            &allowed,
                            Action::Window {
                                key: key.clone(),
                                request: request(value),
                            },
                        )
                    })?,
                )?;
            }
            handle.set_readonly(true);
            registry.borrow_mut().declared.insert(
                key,
                WindowSignals {
                    open: ids[0],
                    width: ids[1],
                    height: ids[2],
                    maximized: ids[3],
                    fullscreen: ids[4],
                    activated: ids[5],
                    can_maximize: ids[6],
                    can_fullscreen: ids[7],
                    decorated: ids[8],
                    handle: handle.clone(),
                },
            );
            Ok(handle)
        })?,
    )
}
