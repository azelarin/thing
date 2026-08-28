//! immutable compositor data and typed requests exposed to luau.

use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

use mlua::{Error, Result, Table, UserData, UserDataMethods};

use crate::state::{Scalar, Tracker};

/// a niri action addressed by a stable compositor id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NiriAction {
    FocusWorkspace(u64),
    FocusWindow(u64),
    CloseWindow(u64),
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
}

impl NiriState {
    fn validate(&self) -> Result<()> {
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
    revisions: [usize; 2],
}

impl NiriData {
    pub fn install(table: &Table, tracker: Rc<RefCell<Tracker>>) -> Result<Self> {
        let workspaces = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let windows = tracker.borrow_mut().insert(Scalar::Boolean(false))?;
        let revisions = [workspaces, windows];
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
        Ok(Self {
            state,
            tracker,
            revisions,
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
        Ok(())
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
