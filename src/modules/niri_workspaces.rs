use crate::channels::{AsyncSenderExt, BroadcastReceiverExt};
use crate::clients::niri::{NiriAction, NiriUpdate, WindowInfo, WorkspaceInfo};
use crate::config::CommonConfig;
use crate::gtk_helpers::{IronbarGtkExt, MouseButton};
use crate::modules::{
    Module, ModuleInfo, ModuleParts, ModulePopup, ModuleUpdateEvent, PopupButton, WidgetContext,
};
use crate::{module_impl, register_client, spawn};
use color_eyre::Result;
use gtk::prelude::*;
use gtk::{Label, Orientation};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use tokio::sync::mpsc;

register_client!(crate::clients::niri::Client, niri);

struct WorkspaceButton {
    button: gtk::Button,
    /// For the popups. probably some better way to get the windows
    windows: Rc<RefCell<Vec<WindowInfo>>>,
}

#[derive(Debug, Deserialize, Clone)]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
#[serde(default)]
pub struct NiriWorkspacesModule {
    icon_size: i32,
    #[serde(flatten)]
    pub common: Option<CommonConfig>,
}

impl Default for NiriWorkspacesModule {
    fn default() -> Self {
        Self {
            icon_size: 32,
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
        let client = context.client::<crate::clients::niri::Client>();

        // from the niri client
        {
            let tx = context.tx.clone();
            let output_name = info.output_name.to_string();
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
                                windows: update.windows,
                            };
                            tx.send_update(filtered).await;
                        }
                        Err(crate::modules::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("niri module lagged by {n} messages");
                        }
                        Err(crate::modules::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        // handle interactions from the bar
        spawn(async move {
            while let Some(event) = rx.recv().await {
                client.dispatch(event).await;
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
        let popup_container = gtk::Box::new(Orientation::Vertical, 0);

        let image_provider = context.ironbar.image_provider();
        let controller_tx = context.controller_tx.clone();

        let buttons: Rc<RefCell<BTreeMap<u64, WorkspaceButton>>> =
            Rc::new(RefCell::new(BTreeMap::new()));

        context.subscribe().recv_glib(
            (&container, &context.tx, &buttons, &popup_container),
            move |(container, tx, buttons, popup_container), update: NiriUpdate| {
                on_update(
                    update,
                    container,
                    tx,
                    buttons,
                    popup_container,
                    &image_provider,
                    &controller_tx,
                    self.icon_size,
                );
            },
        );

        let popup = Some(popup_container).into_popup_parts_with_finder(Rc::new(move |id| {
            buttons
                .borrow()
                .values()
                .find(|wb| wb.button.popup_id() == id)
                .map(|wb| wb.button.clone())
        }));

        Ok(ModuleParts {
            widget: container,
            popup,
        })
    }
}

fn on_update(
    update: NiriUpdate,
    container: &gtk::Box,
    tx: &mpsc::Sender<ModuleUpdateEvent<NiriUpdate>>,
    buttons: &Rc<RefCell<BTreeMap<u64, WorkspaceButton>>>,
    popup_container: &gtk::Box,

    // static garbage
    image_provider: &crate::image::Provider,
    controller_tx: &mpsc::Sender<NiriAction>,
    icon_size: i32,
) {
    // current visible workspaces
    let visible_workspaces: Vec<&WorkspaceInfo> = update
        .workspaces
        .iter()
        // only show the new (empty) workspace if its active
        .filter(|ws| update.windows.iter().any(|w| w.workspace_id == Some(ws.id)) || ws.is_active)
        .collect();
    let current_ids: Vec<u64> = visible_workspaces.iter().map(|ws| ws.id).collect();

    // remove old workspace buttons
    buttons.borrow_mut().retain(|id, wb| {
        if current_ids.contains(id) {
            true
        } else {
            container.remove(&wb.button);
            false
        }
    });

    for workspace in visible_workspaces {
        let workspace_windows: Vec<WindowInfo> = update
            .windows
            .iter()
            .filter(|w| w.workspace_id == Some(workspace.id))
            .cloned()
            .collect();

        // get button
        let mut buttons = buttons.borrow_mut();
        let workspace_button = buttons.entry(workspace.id).or_insert_with(|| {
            let new_button = new_workspace_button(workspace, controller_tx, tx, popup_container);
            container.append(&new_button.button);
            new_button
        });

        // update windows
        *workspace_button.windows.borrow_mut() = workspace_windows.clone();
        workspace_button.button.set_child(Some(&build_workspace_box(
            workspace,
            &workspace_windows,
            image_provider,
            controller_tx,
            icon_size,
        )));

        if workspace.is_active {
            workspace_button.button.add_css_class("active");
        } else {
            workspace_button.button.remove_css_class("active");
        }
        if workspace.is_focused {
            workspace_button.button.add_css_class("focused");
        } else {
            workspace_button.button.remove_css_class("focused");
        }
    }
}

fn new_workspace_button(
    workspace: &WorkspaceInfo,
    controller_tx: &mpsc::Sender<NiriAction>,
    tx: &mpsc::Sender<ModuleUpdateEvent<NiriUpdate>>,
    popup_container: &gtk::Box,
) -> WorkspaceButton {
    let button = gtk::Button::new();
    button.ensure_popup_id();
    button.add_css_class("workspace");

    // left click focuses it
    let id = workspace.id;
    button.connect_pressed(MouseButton::Primary, {
        let controller_tx = controller_tx.clone();
        move || controller_tx.send_spawn(NiriAction::FocusWorkspace(id))
    });

    // right click opens the popup
    let windows = Rc::new(RefCell::new(Vec::<WindowInfo>::new()));
    button.connect_pressed(MouseButton::Secondary, {
        let tx = tx.clone();
        let button = button.clone();
        let popup_container = popup_container.clone();
        let controller_tx = controller_tx.clone();
        let windows = windows.clone();
        move || {
            // the popup is shared, so clear before adding windows
            while let Some(child) = popup_container.first_child() {
                popup_container.remove(&child);
            }

            for win in windows.borrow().iter() {
                let label = Label::new(Some(&win.title));
                label.add_css_class("window-title");

                // left clicking a window in the popup focuses it
                let id = win.id;
                label.connect_pressed(MouseButton::Primary, {
                    let controller_tx = controller_tx.clone();
                    move || controller_tx.send_spawn(NiriAction::FocusWindow(id))
                });
                popup_container.append(&label);
            }

            tx.send_spawn(ModuleUpdateEvent::TogglePopup(button.popup_id()));
        }
    });

    WorkspaceButton { button, windows }
}

fn build_workspace_box(
    workspace: &WorkspaceInfo,
    windows: &[WindowInfo],
    image_provider: &crate::image::Provider,
    controller_tx: &mpsc::Sender<NiriAction>,
    icon_size: i32,
) -> gtk::Box {
    let ws_box = gtk::Box::new(Orientation::Horizontal, 0);

    let label = Label::new(Some(&workspace.name));
    label.add_css_class("workspace-name");
    ws_box.append(&label);

    let mut sorted_windows = windows.to_vec();
    sorted_windows.sort_by_key(|w| w.layout_pos);

    for window in &sorted_windows {
        let window_box = gtk::Box::new(Orientation::Horizontal, 0);
        window_box.add_css_class("window");
        if window.is_focused {
            window_box.add_css_class("focused");
        }

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::ScaleDown)
            .build();
        picture.add_css_class("window-icon");

        let app_id = window.app_id.clone();
        let image_provider = image_provider.clone();
        let pic_clone = picture.clone();
        glib::spawn_future_local(async move {
            image_provider
                .load_into_picture_silent(&app_id, icon_size, true, &pic_clone)
                .await;
        });

        let id = window.id;
        window_box.connect_pressed(MouseButton::Primary, {
            let controller_tx = controller_tx.clone();
            move || controller_tx.send_spawn(NiriAction::FocusWindow(id))
        });

        window_box.append(&picture);
        ws_box.append(&window_box);
    }

    ws_box
}
