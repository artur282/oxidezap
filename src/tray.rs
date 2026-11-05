use crossbeam_channel::Sender;
use ksni::menu::{MenuItem, StandardItem, SubMenu};
use ksni::{Icon, ToolTip};
use tracing::{info, warn};

use crate::settings::Account;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrayAction {
    ShowHide,
    OpenAccount(String),
    AddAccount,
    Settings,
    Lock,
    Quit,
}

#[derive(Clone, Debug)]
struct TrayAccount {
    id: String,
    name: String,
    unread: u32,
}

pub struct OxideTray {
    tx: Sender<TrayAction>,
    accounts: Vec<TrayAccount>,
    unread: u32,
    visible: bool,
    icons: Vec<Icon>,
}

impl OxideTray {
    fn new(tx: Sender<TrayAction>) -> Self {
        Self {
            tx,
            accounts: Vec::new(),
            unread: 0,
            visible: true,
            icons: load_icons(),
        }
    }
}

impl ksni::Tray for OxideTray {
    fn id(&self) -> String {
        "oxidezap".to_string()
    }

    fn title(&self) -> String {
        if self.unread > 0 {
            format!("OxideZap — {} unread", self.unread)
        } else {
            "OxideZap".to_string()
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icons.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: String::new(),
            icon_pixmap: self.icons.clone(),
            title: "OxideZap".to_string(),
            description: if self.unread > 0 {
                format!("{} unread message(s)", self.unread)
            } else {
                "WhatsApp Web for Linux".to_string()
            },
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(TrayAction::ShowHide);
    }

    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.try_send(TrayAction::ShowHide);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = Vec::new();
        items.push(
            StandardItem {
                label: if self.visible {
                    "Hide OxideZap".to_string()
                } else {
                    "Show OxideZap".to_string()
                },
                icon_name: "view-restore".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::ShowHide);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Lock OxideZap".to_string(),
                icon_name: "system-lock-screen".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::Lock);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);

        let account_items = self
            .accounts
            .iter()
            .map(|account| {
                let id = account.id.clone();
                let label = if account.unread > 0 {
                    format!("{} ({})", account.name, account.unread)
                } else {
                    account.name.clone()
                };
                StandardItem {
                    label,
                    icon_name: "user-available".to_string(),
                    activate: Box::new(move |tray: &mut Self| {
                        let _ = tray.tx.try_send(TrayAction::OpenAccount(id.clone()));
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect();

        items.push(
            SubMenu {
                label: "Accounts".to_string(),
                submenu: account_items,
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Add account…".to_string(),
                icon_name: "list-add".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::AddAccount);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Settings…".to_string(),
                icon_name: "preferences-system".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::Settings);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit OxideZap".to_string(),
                icon_name: "application-exit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.try_send(TrayAction::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        items
    }
}

pub struct TrayHandle {
    handle: ksni::blocking::Handle<OxideTray>,
}

impl TrayHandle {
    pub fn start(tx: Sender<TrayAction>) -> Option<Self> {
        use ksni::blocking::TrayMethods;
        let tray = OxideTray::new(tx);
        match tray.spawn() {
            Ok(handle) => {
                info!("system tray started");
                Some(Self { handle })
            }
            Err(error) => {
                warn!(%error, "could not start the system tray");
                None
            }
        }
    }

    pub fn update(&self, accounts: &[Account], unread: u32, visible: bool) {
        let accounts = accounts
            .iter()
            .map(|account| TrayAccount {
                id: account.id.clone(),
                name: account.name.clone(),
                unread: account.unread,
            })
            .collect();
        let _ = self.handle.update(move |tray| {
            tray.accounts = accounts;
            tray.unread = unread;
            tray.visible = visible;
        });
    }

    pub fn shutdown(&self) {
        self.handle.shutdown();
    }
}

fn load_icons() -> Vec<Icon> {
    let bytes = include_bytes!("../assets/oxidezap-64.png");
    let Ok(image) = image::load_from_memory(bytes) else {
        warn!("could not decode the tray icon");
        return Vec::new();
    };
    let image = image.to_rgba8();
    let (width, height) = image.dimensions();
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for pixel in image.pixels() {
        // StatusNotifierItem expects ARGB32 in network byte order.
        data.extend_from_slice(&[pixel[3], pixel[0], pixel[1], pixel[2]]);
    }
    vec![Icon {
        width: width as i32,
        height: height as i32,
        data,
    }]
}
