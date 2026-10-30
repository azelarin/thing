//! immutable compositor data and typed requests exposed to luau.

use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

use mlua::{Error, Result, Table, UserData, UserDataMethods};

use crate::state::{Scalar, Signal, Tracker};

/// a compositor action queued by a configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NiriAction {
    FocusWorkspace(u64),
    FocusWindow(u64),
    CloseWindow(u64),
    SetOverview(bool),
    ToggleOverview,
    SwitchLayout(NiriLayoutTarget),
}

/// a keyboard layout selected by direction or zero-based compositor index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NiriLayoutTarget {
    Next,
    Previous,
    Index(u8),
}

/// configured keyboard layouts and their zero-based active index. an empty
/// configuration uses index zero.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NiriKeyboardLayouts {
    pub names: Vec<String>,
    pub current_index: usize,
}

/// one workspace in compositor order, identified independently of its index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub id: u64,
    pub index: u8,
    pub name: Option<String>,
    pub output: Option<String>,
    pub active: bool,
    pub focused: bool,
    pub urgent: bool,
}

/// a window and its position in niri's scrolling layout.
#[derive(Clone, Debug, PartialEq)]
pub struct NiriWindow {
    pub id: u64,
    pub workspace_id: Option<u64>,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub focused: bool,
    pub urgent: bool,
    pub floating: bool,
    pub column: Option<usize>,
    pub row: Option<usize>,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NiriState {
    pub workspaces: Vec<Workspace>,
    pub windows: Vec<NiriWindow>,
    pub overview_open: Option<bool>,
    pub keyboard_layouts: Option<NiriKeyboardLayouts>,
}

impl NiriState {
    pub(crate) fn validate(&self) -> Result<()> {
        let valid_text = |text: &Option<String>| {
            text.as_ref()
                .is_none_or(|s| s.len() <= 65536 && !s.contains('\0'))
        };
        let mut workspaces = BTreeSet::new();
        let mut windows = BTreeSet::new();
        if self.workspaces.len() > 10000
            || self.windows.len() > 10000
            || self
                .workspaces
                .iter()
                .any(|w| !workspaces.insert(w.id) || !valid_text(&w.name) || !valid_text(&w.output))
            || self.keyboard_layouts.as_ref().is_some_and(|layouts| {
                layouts.names.len() > 256
                    || (layouts.names.is_empty() && layouts.current_index != 0)
                    || (!layouts.names.is_empty() && layouts.current_index >= layouts.names.len())
                    || layouts
                        .names
                        .iter()
                        .any(|name| name.len() > 65536 || name.contains('\0'))
            })
            || self.windows.iter().any(|w| {
                !windows.insert(w.id)
                    || !valid_text(&w.title)
                    || !valid_text(&w.app_id)
                    || !w.width.is_finite()
                    || !w.height.is_finite()
                    || w.width < 0.0
                    || w.height < 0.0
            })
        {
            return Err(Error::runtime("invalid niri snapshot"));
        }
        Ok(())
    }
}

pub(crate) struct NiriData {
    state: Rc<RefCell<NiriState>>,
    tracker: Rc<RefCell<Tracker>>,
    revisions: [usize; 3],
    overview: usize,
}

impl NiriData {
    pub fn install(table: &Table, tracker: Rc<RefCell<Tracker>>) -> Result<Self> {
        let workspaces = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let windows = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let layouts = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let overview = tracker.borrow_mut().insert(Scalar::Nil)?;
        let revisions = [workspaces, windows, layouts];
        let state = Rc::new(RefCell::new(NiriState::default()));
        for (index, name) in ["workspaces", "windows"].into_iter().enumerate() {
            table.set(
                name,
                List {
                    tracker: tracker.clone(),
                    state: state.clone(),
                    revision: revisions[index],
                    windows: index == 1,
                },
            )?;
        }
        table.set(
            "overview_open",
            Signal {
                id: overview,
                tracker: tracker.clone(),
                readonly: true,
            },
        )?;
        table.set(
            "keyboard_layouts",
            Layouts {
                tracker: tracker.clone(),
                state: state.clone(),
                revision: layouts,
            },
        )?;
        Ok(Self {
            state,
            tracker,
            revisions,
            overview,
        })
    }

    pub fn snapshot(&self) -> NiriState {
        self.state.borrow().clone()
    }

    pub fn set(&self, state: NiriState) -> Result<()> {
        state.validate()?;
        let old = self.state.replace(state);
        let current = self.state.borrow();
        for (index, changed) in [
            old.workspaces != current.workspaces,
            old.windows != current.windows,
            old.keyboard_layouts != current.keyboard_layouts,
        ]
        .into_iter()
        .enumerate()
        {
            if changed {
                let mut tracker = self.tracker.borrow_mut();
                let revision = self.revisions[index];
                let value = tracker.read(revision) != Scalar::Boolean(true);
                tracker.write(revision, Scalar::Boolean(value))?;
            }
        }
        self.tracker.borrow_mut().write(
            self.overview,
            current.overview_open.map_or(Scalar::Nil, Scalar::Boolean),
        )?;
        Ok(())
    }
}

struct Layouts {
    tracker: Rc<RefCell<Tracker>>,
    state: Rc<RefCell<NiriState>>,
    revision: usize,
}

impl UserData for Layouts {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let state = this.state.borrow();
            let Some(layouts) = &state.keyboard_layouts else {
                return Ok(None);
            };
            let names = lua.create_sequence_from(layouts.names.iter().cloned())?;
            names.set_readonly(true);
            let value = lua.create_table()?;
            value.set("names", names)?;
            value.set(
                "current_index",
                (!layouts.names.is_empty()).then_some(layouts.current_index + 1),
            )?;
            value.set_readonly(true);
            Ok(Some(value))
        });
    }
}

struct List {
    tracker: Rc<RefCell<Tracker>>,
    state: Rc<RefCell<NiriState>>,
    revision: usize,
    windows: bool,
}

impl UserData for List {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("get", |lua, this, ()| {
            this.tracker.borrow_mut().read(this.revision);
            let state = this.state.borrow();
            let list = lua.create_table()?;
            if this.windows {
                for (index, w) in state.windows.iter().enumerate() {
                    let item = lua.create_table()?;
                    item.set("id", w.id.to_string())?;
                    item.set("workspace_id", w.workspace_id.map(|id| id.to_string()))?;
                    item.set("title", w.title.as_deref())?;
                    item.set("app_id", w.app_id.as_deref())?;
                    item.set("focused", w.focused)?;
                    item.set("urgent", w.urgent)?;
                    item.set("floating", w.floating)?;
                    item.set("column", w.column)?;
                    item.set("row", w.row)?;
                    item.set("width", w.width)?;
                    item.set("height", w.height)?;
                    item.set_readonly(true);
                    list.raw_set(index + 1, item)?;
                }
            } else {
                for (index, w) in state.workspaces.iter().enumerate() {
                    let item = lua.create_table()?;
                    item.set("id", w.id.to_string())?;
                    item.set("index", w.index)?;
                    item.set("name", w.name.as_deref())?;
                    item.set("output", w.output.as_deref())?;
                    item.set("active", w.active)?;
                    item.set("focused", w.focused)?;
                    item.set("urgent", w.urgent)?;
                    item.set_readonly(true);
                    list.raw_set(index + 1, item)?;
                }
            }
            list.set_readonly(true);
            Ok(list)
        });
    }
}

pub(crate) fn parse_id(value: mlua::Value) -> Result<u64> {
    let mlua::Value::String(value) = value else {
        return Err(Error::runtime(
            "niri IDs must be decimal strings from the service",
        ));
    };
    let value = value.to_str()?;
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::runtime("invalid niri ID"));
    }
    value
        .parse()
        .map_err(|_| Error::runtime("niri ID exceeds 64 bits"))
}

pub(crate) fn parse_layout_target(value: mlua::Value) -> Result<NiriLayoutTarget> {
    let index = match value {
        mlua::Value::String(value) => {
            return match value.to_str()?.as_ref() {
                "next" => Ok(NiriLayoutTarget::Next),
                "prev" => Ok(NiriLayoutTarget::Previous),
                _ => Err(Error::runtime(
                    "layout must be 'next', 'prev', or an index from 1 to 256",
                )),
            };
        }
        mlua::Value::Integer(index) => index as f64,
        mlua::Value::Number(index) => index,
        _ => {
            return Err(Error::runtime(
                "layout must be 'next', 'prev', or an index from 1 to 256",
            ));
        }
    };
    if !index.is_finite() || index.fract() != 0.0 || !(1.0..=256.0).contains(&index) {
        return Err(Error::runtime(
            "layout index must be a whole number from 1 to 256",
        ));
    }
    Ok(NiriLayoutTarget::Index((index - 1.0) as u8))
}
