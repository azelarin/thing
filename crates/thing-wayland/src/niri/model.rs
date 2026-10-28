//! projection of niri's json event stream into the framework's service values.
//! unknown events are ignored so additions to the protocol remain compatible.

use std::{collections::BTreeMap, io};

use serde::Deserialize;
use serde_json::{Value, json};
use thing_runtime::{NiriAction, NiriState, NiriWindow, Workspace};

#[derive(Default)]
pub(super) struct Model {
    workspaces: BTreeMap<u64, Workspace>,
    windows: BTreeMap<u64, NiriWindow>,
    workspaces_initialized: bool,
    windows_initialized: bool,
}

impl Model {
    pub fn is_initialized(&self) -> bool {
        self.workspaces_initialized && self.windows_initialized
    }

    pub fn snapshot(&self) -> NiriState {
        let mut workspaces: Vec<_> = self.workspaces.values().cloned().collect();
        workspaces.sort_by(|a, b| (&a.output, a.index).cmp(&(&b.output, b.index)));
        NiriState {
            workspaces,
            windows: self.windows.values().cloned().collect(),
        }
    }

    pub fn apply(&mut self, line: &[u8]) -> io::Result<bool> {
        let value: Value = serde_json::from_slice(line)?;
        let object = value
            .as_object()
            .filter(|o| o.len() == 1)
            .ok_or_else(|| io::Error::other("invalid niri event envelope"))?;
        let name = object.keys().next().expect("one event key");
        if !matches!(
            name.as_str(),
            "WorkspacesChanged"
                | "WorkspaceActivated"
                | "WorkspaceUrgencyChanged"
                | "WindowsChanged"
                | "WindowOpenedOrChanged"
                | "WindowFocusChanged"
                | "WindowClosed"
                | "WindowLayoutsChanged"
                | "WindowUrgencyChanged"
        ) {
            return Ok(false);
        }
        match serde_json::from_value::<Event>(value)? {
            Event::WorkspacesChanged { workspaces } => {
                check_count(workspaces.len())?;
                self.workspaces = workspaces.into_iter().map(|w| (w.id, w.into())).collect();
                self.workspaces_initialized = true;
            }
            Event::WorkspaceActivated { id, focused } => {
                let output = self
                    .workspaces
                    .get(&id)
                    .ok_or_else(|| io::Error::other("niri activated an unknown workspace"))?
                    .output
                    .clone();
                for w in self.workspaces.values_mut() {
                    if w.output == output {
                        w.active = w.id == id;
                    }
                    if focused {
                        w.focused = w.id == id;
                    }
                }
            }
            Event::WorkspaceUrgencyChanged { id, urgent } => {
                if let Some(w) = self.workspaces.get_mut(&id) {
                    w.urgent = urgent;
                }
            }
            Event::WindowsChanged { windows } => {
                check_count(windows.len())?;
                self.windows = windows.into_iter().map(|w| (w.id, w.into())).collect();
                self.windows_initialized = true;
            }
            Event::WindowOpenedOrChanged { window } => {
                if window.is_focused {
                    for w in self.windows.values_mut() {
                        w.focused = false;
                    }
                }
                self.windows.insert(window.id, window.into());
                check_count(self.windows.len())?;
            }
            Event::WindowFocusChanged { id } => {
                for w in self.windows.values_mut() {
                    w.focused = Some(w.id) == id;
                }
            }
            Event::WindowClosed { id } => {
                self.windows.remove(&id);
            }
            Event::WindowUrgencyChanged { id, urgent } => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.urgent = urgent;
                }
            }
            Event::WindowLayoutsChanged { changes } => {
                for (id, layout) in changes {
                    if let Some(w) = self.windows.get_mut(&id) {
                        layout.apply(w);
                    }
                }
            }
        }
        Ok(true)
    }
}

fn check_count(count: usize) -> io::Result<()> {
    if count > 10000 {
        Err(io::Error::other("niri snapshot exceeds 10,000 items"))
    } else {
        Ok(())
    }
}

#[derive(Deserialize)]
enum Event {
    WorkspacesChanged { workspaces: Vec<WireWorkspace> },
    WorkspaceActivated { id: u64, focused: bool },
    WorkspaceUrgencyChanged { id: u64, urgent: bool },
    WindowsChanged { windows: Vec<WireWindow> },
    WindowOpenedOrChanged { window: WireWindow },
    WindowFocusChanged { id: Option<u64> },
    WindowClosed { id: u64 },
    WindowLayoutsChanged { changes: Vec<(u64, WireLayout)> },
    WindowUrgencyChanged { id: u64, urgent: bool },
}

#[derive(Deserialize)]
struct WireWorkspace {
    id: u64,
    idx: u8,
    name: Option<String>,
    output: Option<String>,
    is_active: bool,
    is_focused: bool,
    #[serde(default)]
    is_urgent: bool,
}

impl From<WireWorkspace> for Workspace {
    fn from(w: WireWorkspace) -> Self {
        Self {
            id: w.id,
            index: w.idx,
            name: w.name,
            output: w.output,
            active: w.is_active,
            focused: w.is_focused,
            urgent: w.is_urgent,
        }
    }
}

#[derive(Deserialize)]
struct WireWindow {
    id: u64,
    workspace_id: Option<u64>,
    title: Option<String>,
    app_id: Option<String>,
    is_focused: bool,
    is_floating: bool,
    #[serde(default)]
    is_urgent: bool,
    layout: WireLayout,
}

#[derive(Deserialize)]
struct WireLayout {
    pos_in_scrolling_layout: Option<(usize, usize)>,
    tile_size: (f64, f64),
}

impl WireLayout {
    fn apply(self, window: &mut NiriWindow) {
        window.column = self.pos_in_scrolling_layout.map(|p| p.0);
        window.row = self.pos_in_scrolling_layout.map(|p| p.1);
        window.width = self.tile_size.0;
        window.height = self.tile_size.1;
    }
}

impl From<WireWindow> for NiriWindow {
    fn from(w: WireWindow) -> Self {
        let mut window = Self {
            id: w.id,
            workspace_id: w.workspace_id,
            title: w.title,
            app_id: w.app_id,
            focused: w.is_focused,
            floating: w.is_floating,
            urgent: w.is_urgent,
            column: None,
            row: None,
            width: 0.0,
            height: 0.0,
        };
        w.layout.apply(&mut window);
        window
    }
}

pub(super) fn request(action: NiriAction) -> Value {
    json!({"Action": match action {
        NiriAction::FocusWorkspace(id) => json!({"FocusWorkspace": {"reference": {"Id": id}}}),
        NiriAction::FocusWindow(id) => json!({"FocusWindow": {"id": id}}),
        NiriAction::CloseWindow(id) => json!({"CloseWindow": {"id": id}}),
    }})
}

pub(super) fn reply(line: &[u8]) -> io::Result<()> {
    match serde_json::from_slice::<Result<String, String>>(line)? {
        Ok(value) if value == "Handled" => Ok(()),
        Ok(_) => Err(io::Error::other("unexpected niri response")),
        Err(error) => Err(io::Error::other(error)),
    }
}
