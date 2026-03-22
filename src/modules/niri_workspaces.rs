use crate::channels::{AsyncSenderExt, BroadcastReceiverExt};
use crate::clients::niri::{NiriAction, NiriUpdate, WorkspaceInfo};
use crate::config::CommonConfig;
use crate::gtk_helpers::{IronbarGtkExt, MouseButton};
use crate::modules::{
    Module, ModuleInfo, ModuleParts, ModulePopup, ModuleUpdateEvent, PopupButton, WidgetContext,
};
use crate::{module_impl, register_client, spawn};
use color_eyre::Result;
use gtk::prelude::*;
use gtk::{Label, Orientation};
use niri_ipc::{Request, socket::Socket};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use tokio::sync::mpsc;

register_client!(crate::clients::niri::Client, niri);

#[derive(Debug, Deserialize, Clone)]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct NiriWorkspacesModule {
    icon_size: i32,
    #[serde(skip)]
    popup_workspace_id: Rc<RefCell<Option<u64>>>,
    #[serde(flatten)]
    pub common: Option<CommonConfig>,
}

impl Default for NiriWorkspacesModule {
    fn default() -> Self {
        Self {
            icon_size: 32,
            popup_workspace_id: Rc::new(RefCell::new(None)),
            common: Some(CommonConfig::default()),
        }
    }
}

impl Module<gtk::Box> for NiriWorkspacesModule {
    type SendMessage = NiriUpdate;
    type ReceiveMessage = NiriAction;

    module_impl!("niri_workspaces");

    fn spawn_controller(
        &self,
        info: &ModuleInfo,
        context: &WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        mut rx: mpsc::Receiver<Self::ReceiveMessage>,
    ) -> Result<()> {
        let tx = context.tx.clone();
        let output_name = info.output_name.to_string();
        let client = context.client::<crate::clients::niri::Client>();
        let mut niri_rx = client.subscribe();

        spawn(async move {
            loop {
                match niri_rx.recv().await {
                    Ok(update) => {
                        let filtered = NiriUpdate {
                            workspaces: update
                                .workspaces
                                .into_iter()
                                .filter(|ws| ws.output == output_name)
                                .collect(),
                        };
                        tx.send_update(filtered).await;
                    }
                    Err(crate::modules::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("niri subscriber lagged by {n} messages");
                        continue;
                    }
                    Err(_) => break,
                }
            }
        });

        spawn(async move {
            while let Some(action) = rx.recv().await {
                let mut socket = Socket::connect().expect("could not connect to niri socket");
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
                if let Err(e) = socket.send(request) {
                    tracing::error!("niri IPC error: {e}");
                }
            }
        });

        Ok(())
    }

    fn into_widget(
        self,
        context: WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        _info: &ModuleInfo,
    ) -> Result<ModuleParts<gtk::Box>> {
        let container = gtk::Box::new(Orientation::Horizontal, 0);
        let image_provider = context.ironbar.image_provider();
        let popup_workspace_id = self.popup_workspace_id.clone();
        let controller_tx = context.controller_tx.clone();
        let buttons: Rc<RefCell<BTreeMap<u64, gtk::Button>>> =
            Rc::new(RefCell::new(BTreeMap::new()));
        let last_update: Rc<RefCell<Option<NiriUpdate>>> = Rc::new(RefCell::new(None));
        let popup_container = gtk::Box::new(Orientation::Vertical, 0);
        let icon_size = self.icon_size;
        let popup_container_2 = popup_container.clone();

        context.subscribe().recv_glib(
            (&container, &context.tx, &buttons, &last_update),
            move |(container, tx, buttons, last_update), update: NiriUpdate| {
                *last_update.borrow_mut() = Some(update.clone());

                let current_ids: Vec<u64> = update
                    .workspaces
                    .iter()
                    .filter(|ws| !ws.windows.is_empty() || ws.is_active)
                    .map(|ws| ws.id)
                    .collect();

                buttons.borrow_mut().retain(|id, btn| {
                    if current_ids.contains(id) {
                        true
                    } else {
                        container.remove(btn);
                        false
                    }
                });

                for ws in update
                    .workspaces
                    .iter()
                    .filter(|ws| !ws.windows.is_empty() || ws.is_active)
                {
                    if let Some(btn) = buttons.borrow().get(&ws.id) {
                        if ws.is_active {
                            btn.add_css_class("active");
                        } else {
                            btn.remove_css_class("active");
                        }
                        if ws.is_focused {
                            btn.add_css_class("focused");
                        } else {
                            btn.remove_css_class("focused");
                        }
                        btn.set_child(Some(&build_ws_box(
                            ws,
                            &image_provider,
                            &controller_tx,
                            icon_size,
                        )));
                    } else {
                        let ws_button = gtk::Button::new();
                        ws_button.ensure_popup_id();
                        ws_button.add_css_class("workspace");
                        if ws.is_active {
                            ws_button.add_css_class("active");
                        }
                        if ws.is_focused {
                            ws_button.add_css_class("focused");
                        }
                        ws_button.set_child(Some(&build_ws_box(
                            ws,
                            &image_provider,
                            &controller_tx,
                            icon_size,
                        )));

                        let ws_id = ws.id;

                        ws_button.connect_pressed(MouseButton::Primary, {
                            let controller_tx = controller_tx.clone();
                            move || {
                                controller_tx.send_spawn(NiriAction::FocusWorkspace(ws_id));
                            }
                        });

                        ws_button.connect_pressed(MouseButton::Secondary, {
                            let tx = tx.clone();
                            let btn = ws_button.clone();
                            let popup_workspace_id = popup_workspace_id.clone();
                            let last_update = last_update.clone();
                            let popup_container = popup_container_2.clone();
                            let controller_tx = controller_tx.clone();
                            move || {
                                *popup_workspace_id.borrow_mut() = Some(ws_id);

                                while let Some(child) = popup_container.first_child() {
                                    popup_container.remove(&child);
                                }

                                if let Some(update) = last_update.borrow().as_ref() {
                                    for ws in update.workspaces.iter().filter(|ws| ws.id == ws_id) {
                                        for win in &ws.windows {
                                            let label = Label::new(Some(&win.title));
                                            label.add_css_class("window-title");
                                            let win_id = win.id;
                                            label.connect_pressed(MouseButton::Primary, {
                                                let controller_tx = controller_tx.clone();
                                                move || {
                                                    controller_tx.send_spawn(
                                                        NiriAction::FocusWindow(win_id),
                                                    );
                                                }
                                            });
                                            popup_container.append(&label);
                                        }
                                    }
                                }

                                tx.send_spawn(ModuleUpdateEvent::OpenPopup(btn.popup_id()));
                            }
                        });

                        container.append(&ws_button);
                        buttons.borrow_mut().insert(ws.id, ws_button);
                    }
                }
            },
        );

        let popup = Some(popup_container).into_popup_parts_with_finder(Rc::new(move |id| {
            buttons
                .borrow()
                .values()
                .find(|b| b.popup_id() == id)
                .cloned()
        }));

        Ok(ModuleParts {
            widget: container,
            popup,
        })
    }
}

fn build_ws_box(
    ws: &WorkspaceInfo,
    image_provider: &crate::image::Provider,
    controller_tx: &mpsc::Sender<NiriAction>,
    icon_size: i32,
) -> gtk::Box {
    let ws_box = gtk::Box::new(Orientation::Horizontal, 0);

    let ws_label = Label::new(Some(&ws.name));
    ws_label.add_css_class("workspace-name");
    ws_box.append(&ws_label);

    for win in &ws.windows {
        let win_box = gtk::Box::new(Orientation::Horizontal, 0);
        win_box.add_css_class("window");
        if win.is_focused {
            win_box.add_css_class("focused");
        }

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::ScaleDown)
            .build();
        picture.add_css_class("window-icon");

        let app_id = win.app_id.clone();
        let ip = image_provider.clone();
        let pic = picture.clone();
        glib::spawn_future_local(async move {
            ip.load_into_picture_silent(&app_id, icon_size, true, &pic)
                .await;
        });

        let gesture = gtk::GestureClick::new();
        gesture.set_button(1);
        let controller_tx = controller_tx.clone();
        let win_id = win.id;
        gesture.connect_pressed(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            controller_tx.send_spawn(NiriAction::FocusWindow(win_id));
        });
        win_box.add_controller(gesture);

        win_box.append(&picture);
        ws_box.append(&win_box);
    }

    ws_box
}
