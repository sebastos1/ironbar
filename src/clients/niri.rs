use crate::spawn;
use niri_ipc::{Event, Request, Window, Workspace};
use std::collections::BTreeMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tracing::error;

async fn connect() -> std::io::Result<UnixStream> {
    let socket_path = std::env::var_os("NIRI_SOCKET").ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "NIRI_SOCKET not found")
    })?;
    UnixStream::connect(socket_path).await
}

async fn send_request(stream: &mut UnixStream, request: &Request) -> std::io::Result<()> {
    let buf = serde_json::to_string(request)?;
    stream.write_all(buf.as_bytes()).await?;
    stream.shutdown().await
}

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
}

#[derive(Debug, Clone)]
pub struct NiriUpdate {
    pub workspaces: Vec<WorkspaceInfo>,
    pub windows: Vec<WindowInfo>,
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
            let mut stream = match connect().await {
                Ok(s) => s,
                Err(e) => {
                    error!("could not connect to niri socket: {e}");
                    return;
                }
            };

            if let Err(e) = send_request(&mut stream, &Request::EventStream).await {
                error!("niri EventStream error: {e}");
                return;
            }

            // read and discard the initial reply line
            let mut buf = String::new();
            let mut reader = BufReader::new(stream);
            reader.read_line(&mut buf).await.ok();

            let mut my_workspaces: BTreeMap<u64, Workspace> = BTreeMap::new();
            let mut my_windows: BTreeMap<u64, WindowInfo> = BTreeMap::new();

            loop {
                buf.clear();
                reader.read_line(&mut buf).await.unwrap_or(0);
                let event: Event = match serde_json::from_str(&buf) {
                    Ok(ev) => ev,
                    Err(e) => {
                        error!("niri IPC parse error: {e}");
                        continue;
                    }
                };

                match event {
                    Event::WorkspacesChanged { workspaces } => {
                        my_workspaces = workspaces.into_iter().map(|w| (w.id, w)).collect();
                    }
                    Event::WorkspaceActivated { id, focused } => {
                        let output = my_workspaces
                            .get(&id)
                            .and_then(|ws| ws.output.clone())
                            .unwrap_or_default();
                        if let Some(ws) = my_workspaces.get_mut(&id) {
                            ws.is_active = true;
                            ws.is_focused = focused;
                        }
                        if focused {
                            my_workspaces
                                .values_mut()
                                .filter(|w| w.id != id)
                                .for_each(|w| {
                                    w.is_focused = false;
                                    if w.output.as_deref() == Some(output.as_str()) {
                                        w.is_active = false;
                                    }
                                });
                        } else {
                            my_workspaces
                                .values_mut()
                                .filter(|w| {
                                    w.id != id && w.output.as_deref() == Some(output.as_str())
                                })
                                .for_each(|w| w.is_active = false);
                        }
                    }
                    Event::WindowsChanged { windows } => {
                        let focused_id = my_windows.values().find(|w| w.is_focused).map(|w| w.id);
                        my_windows = windows
                            .into_iter()
                            .map(|w| (w.id, WindowInfo::from(w)))
                            .collect();
                        if let Some(id) = focused_id {
                            if let Some(w) = my_windows.get_mut(&id) {
                                w.is_focused = true;
                            }
                        }
                    }
                    Event::WindowOpenedOrChanged { window } => {
                        let was_focused = my_windows
                            .get(&window.id)
                            .map(|w| w.is_focused)
                            .unwrap_or(false);
                        let mut info = WindowInfo::from(window);
                        info.is_focused = was_focused;
                        my_windows.insert(info.id, info);
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
                            if let Some(w) = my_windows.get_mut(&id) {
                                w.layout_pos = layout.pos_in_scrolling_layout.unwrap_or_default();
                            }
                        }
                    }
                    _ => continue,
                }

                let _ = tx2.send(build_update(&my_workspaces, &my_windows));
            }
        });

        Self { tx, _rx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<NiriUpdate> {
        self.tx.subscribe()
    }

    pub async fn dispatch(&self, action: NiriAction) {
        spawn(async move {
            let mut stream = match connect().await {
                Ok(s) => s,
                Err(e) => {
                    error!("could not connect to niri socket: {e}");
                    return;
                }
            };
            let request = match action {
                NiriAction::FocusWorkspace(id) => {
                    Request::Action(niri_ipc::Action::FocusWorkspace {
                        reference: niri_ipc::WorkspaceReferenceArg::Id(id),
                    })
                }
                NiriAction::FocusWindow(id) => {
                    Request::Action(niri_ipc::Action::FocusWindow { id })
                }
            };
            if let Err(e) = send_request(&mut stream, &request).await {
                error!("niri IPC error: {e}");
            }
        });
    }
}

fn build_update(
    workspaces: &BTreeMap<u64, Workspace>,
    windows: &BTreeMap<u64, WindowInfo>,
) -> NiriUpdate {
    let mut ws_list: Vec<WorkspaceInfo> = workspaces
        .values()
        .map(|ws| WorkspaceInfo {
            id: ws.id,
            idx: ws.idx,
            name: ws.name.clone().unwrap_or_else(|| ws.idx.to_string()),
            output: ws.output.clone().unwrap_or_default(),
            is_active: ws.is_active,
            is_focused: ws.is_focused,
        })
        .collect();
    ws_list.sort_by_key(|w| (w.output.clone(), w.idx));
    NiriUpdate {
        workspaces: ws_list,
        windows: windows.values().cloned().collect(),
    }
}
