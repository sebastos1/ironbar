use crate::spawn;
use niri_ipc::{Event, Request, Window, Workspace, socket::Socket};
use std::collections::BTreeMap;
use tokio::sync::broadcast;
use tracing::error;

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: u64,
    pub workspace_id: Option<u64>,
    pub app_id: String,
    pub title: String,
    pub is_focused: bool,
    pub layout_pos: (usize, usize),
}

impl From<Window> for WindowInfo {
    fn from(w: Window) -> Self {
        Self {
            id: w.id,
            workspace_id: w.workspace_id,
            app_id: w.app_id.unwrap_or_default(),
            title: w.title.unwrap_or_default(),
            is_focused: w.is_focused,
            layout_pos: w.layout.pos_in_scrolling_layout.unwrap_or_default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: u64,
    pub idx: u8,
    pub name: String,
    pub output: String,
    pub is_active: bool,
    pub is_focused: bool,
    pub windows: Vec<WindowInfo>,
}

#[derive(Debug, Clone)]
pub struct NiriUpdate {
    pub workspaces: Vec<WorkspaceInfo>,
}

#[derive(Debug)]
pub enum NiriAction {
    FocusWorkspace(u64),
    FocusWindow(u64),
}

#[derive(Debug)]
pub struct Client {
    tx: broadcast::Sender<NiriUpdate>,
    _rx: broadcast::Receiver<NiriUpdate>,
}

impl Client {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(32);

        let tx2 = tx.clone();
        spawn(async move {
            let Ok(mut socket) =
                Socket::connect().inspect_err(|e| error!("could not connect to niri socket: {e}"))
            else {
                return;
            };

            match socket.send(Request::EventStream) {
                Ok(Ok(niri_ipc::Response::Handled)) => {}
                other => {
                    error!("unexpected reply to EventStream: {other:?}");
                    return;
                }
            }

            let mut my_workspaces: BTreeMap<u64, Workspace> = BTreeMap::new();
            let mut my_windows: BTreeMap<u64, WindowInfo> = BTreeMap::new();

            let build_workspaces =
                |workspaces: &BTreeMap<u64, Workspace>, windows: &BTreeMap<u64, WindowInfo>| {
                    let mut ws_list: Vec<WorkspaceInfo> = workspaces
                        .values()
                        .map(|ws| {
                            let mut wins: Vec<WindowInfo> = windows
                                .values()
                                .filter(|w| w.workspace_id == Some(ws.id))
                                .cloned()
                                .collect();
                            wins.sort_by_key(|w| w.layout_pos);
                            WorkspaceInfo {
                                id: ws.id,
                                idx: ws.idx,
                                name: ws.name.clone().unwrap_or_else(|| ws.idx.to_string()),
                                output: ws.output.clone().unwrap_or_default(),
                                is_active: ws.is_active,
                                is_focused: ws.is_focused,
                                windows: wins,
                            }
                        })
                        .collect();
                    ws_list.sort_by_key(|w| (w.output.clone(), w.idx));
                    ws_list
                };

            let mut next_event = socket.read_events();
            loop {
                let event = match next_event() {
                    Ok(ev) => ev,
                    Err(err) => {
                        error!("niri IPC error: {err}");
                        break;
                    }
                };

                match event {
                    Event::WorkspacesChanged { workspaces } => {
                        my_workspaces = workspaces.into_iter().map(|w| (w.id, w)).collect();
                    }
                    Event::WorkspaceActivated { id, focused } => {
                        error!("focusing workspace: {id}");
                        let output = my_workspaces
                            .get(&id)
                            .and_then(|ws| ws.output.clone())
                            .unwrap_or_default();
                        my_workspaces.values_mut().for_each(|w| {
                            if w.id == id {
                                w.is_active = true;
                                w.is_focused = focused;
                            } else {
                                if focused {
                                    w.is_focused = false;
                                }
                                if w.output.as_deref() == Some(output.as_str()) {
                                    w.is_active = false;
                                }
                            }
                        });
                    }
                    Event::WindowsChanged { windows } => {
                        my_windows = windows
                            .into_iter()
                            .map(|w| (w.id, WindowInfo::from(w)))
                            .collect();
                    }
                    Event::WindowOpenedOrChanged { window } => {
                        if window.is_focused {
                            my_windows.values_mut().for_each(|w| w.is_focused = false);
                        }
                        my_windows.insert(window.id, WindowInfo::from(window));
                    }
                    Event::WindowClosed { id } => {
                        my_windows.remove(&id);
                    }
                    Event::WindowFocusChanged { id } => {
                        my_windows
                            .values_mut()
                            .for_each(|w| w.is_focused = Some(w.id) == id);
                    }
                    Event::WindowLayoutsChanged { changes } => {
                        for (id, layout) in changes {
                            if let Some(win) = my_windows.get_mut(&id) {
                                win.layout_pos = layout.pos_in_scrolling_layout.unwrap_or_default();
                            }
                        }
                    }
                    _ => continue,
                }

                let _ = tx2.send(NiriUpdate {
                    workspaces: build_workspaces(&my_workspaces, &my_windows),
                });
            }
        });

        Self { tx, _rx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NiriUpdate> {
        self.tx.subscribe()
    }
}
