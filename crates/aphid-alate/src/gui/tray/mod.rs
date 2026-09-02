//! An icon in the system tray, and a menu on it.
//!
//! The window can be reached from a key in a window manager, through
//! `aphid alate gui toggle`. This is the other way in, for the times when the
//! window is not on screen and neither is a terminal.
//!
//! **Every item is a [`Command`]** — the same ones the control socket carries.
//! The tray is another client of the same small protocol, so nothing here knows
//! what a window is, and a person with neither a tray nor a window manager
//! loses nothing that the socket does not still offer.
//!
//! ## Two systems, three mechanisms
//!
//! On Linux there are two tray protocols and no way to ask one to do the
//! other's job, so both are spoken. **StatusNotifierItem**, over D-Bus through
//! `ksni`, is what KDE, a GNOME with the extension, waybar and swaybar listen
//! for. **XEmbed** is what everything older provides — i3 with polybar,
//! xfce4-panel, stalonetray, trayer — where an icon is a window a panel adopts
//! rather than a message on a bus. A desktop answers one or the other, so this
//! offers the first and falls back to the second.
//!
//! The companion this borrows from ran its tray in a **subprocess**, because
//! the Python library it used wanted GTK 3 on Linux and the main thread on
//! macOS; neither of these wants either, so the tray lives in this process.
//!
//! On macOS it is an `NSStatusItem` through `tray-icon`, built on the thread
//! GPUI runs its event loop on, since AppKit will take it from nowhere else.
//! GPUI has an `NSStatusItem` of its own (`platform/mac/status_item.rs`), but
//! `mod mac` is private, so it cannot be borrowed.
//!
//! ## What the menu does not do
//!
//! It does not show what is currently chosen. The items are verbs and not
//! settings — *Show*, *Switch mode*, *Familiar ▸ sap* — so there is no state in
//! the menu to keep in step with the window, and the two platforms need no
//! update path between them. The window is where the state is visible.

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub mod xembed;

use tokio::sync::mpsc::UnboundedSender;

use super::Msg;
use super::config::Familiar;
use super::control::Command;
use crate::gateway::is_listening;
use crate::home::Home;

/// One thing a person can pick.
pub(super) struct Choice {
    /// Stable across a run, and what the macOS side matches an event by. The
    /// Linux menu carries the command in a closure instead, so there the field
    /// is read only by the test that keeps the two in step.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    id: String,
    pub(super) label: String,
    pub(super) command: Command,
}

/// A row of the menu: something to pick, a group of them, or a line.
pub(super) enum Row {
    One(Choice),
    Group { label: String, choices: Vec<Choice> },
    Separator,
}

/// The alates on this machine that are awake, in name order.
///
/// Read once, when the tray starts. An alate started afterwards is reached by
/// `aphid alate gui --name`, which is the same gesture from the other side.
fn awake() -> Vec<String> {
    let Ok(root) = Home::root_dir() else {
        return Vec::new();
    };
    let Ok(names) = Home::list_in(&root) else {
        return Vec::new();
    };
    names
        .into_iter()
        .filter(|name| is_listening(&root.join(name).join("gateway.sock")))
        .collect()
}

/// The whole menu.
pub(super) fn rows() -> Vec<Row> {
    let mut rows = vec![
        Row::One(Choice {
            id: "show".to_owned(),
            label: "Show".to_owned(),
            command: Command::Show,
        }),
        Row::One(Choice {
            id: "toggle".to_owned(),
            label: "Expand or collapse".to_owned(),
            command: Command::Toggle,
        }),
        Row::One(Choice {
            id: "mode".to_owned(),
            label: "Switch mode".to_owned(),
            command: Command::Mode,
        }),
        Row::Separator,
        Row::Group {
            label: "Familiar".to_owned(),
            choices: [Familiar::Sap, Familiar::Drift]
                .into_iter()
                .map(|familiar| Choice {
                    id: format!("familiar:{}", familiar.label()),
                    label: familiar.label().to_owned(),
                    command: Command::Familiar {
                        name: familiar.label().to_owned(),
                    },
                })
                .collect(),
        },
    ];

    let alates = awake();
    if !alates.is_empty() {
        rows.push(Row::Group {
            label: "Alate".to_owned(),
            choices: alates
                .into_iter()
                .map(|name| Choice {
                    id: format!("alate:{name}"),
                    label: name.clone(),
                    command: Command::Instance { name },
                })
                .collect(),
        });
    }
    rows.push(Row::Separator);
    rows.push(Row::One(Choice {
        id: "quit".to_owned(),
        // Said in full, because the two are easy to confuse and only one of
        // them stops the agent.
        label: "Quit the window".to_owned(),
        command: Command::Quit,
    }));
    rows
}

/// The icon: a filled dot in the green of the wordmark.
///
/// Drawn here rather than shipped as a file so that there is no asset to find
/// at run time, and no icon theme to install into. RGBA, which is what the
/// macOS side wants; the Linux side turns it into ARGB.
pub(super) fn glyph(size: u32) -> Vec<u8> {
    // 0x80c96b, the accent of both interfaces.
    const COLOR: [u8; 3] = [0x80, 0xc9, 0x6b];
    let half = size as f32 / 2.;
    let radius = half - 1.5;
    let mut pixels = Vec::with_capacity(size as usize * size as usize * 4);
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - half;
            let dy = y as f32 + 0.5 - half;
            // One pixel of falloff, so the dot is not a staircase.
            let edge = radius - (dx * dx + dy * dy).sqrt();
            let alpha = edge.clamp(0., 1.);
            pixels.extend_from_slice(&COLOR);
            pixels.push((alpha * 255.) as u8);
        }
    }
    pixels
}

/// How big the icon is drawn. Tray hosts scale it; 22 is the size most of them
/// ask for.
const ICON: u32 = 22;

/// Whatever has to stay alive for the icon to stay in the tray.
///
/// Dropping it takes the icon away, which is what should happen when the window
/// closes.
pub struct Tray {
    /// Held when the desktop answered on the bus. An icon docked into a panel
    /// needs nothing held: it is a window, and it lives until the panel or the
    /// process does.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    _handle: Option<ksni::Handle<Menu>>,
    #[cfg(target_os = "macos")]
    _icon: tray_icon::TrayIcon,
}

// ---------------------------------------------------------------- Linux ----

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
struct Menu {
    orders: UnboundedSender<Msg>,
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
impl Menu {
    fn item(&self, choice: Choice) -> ksni::MenuItem<Self> {
        let command = choice.command;
        ksni::menu::StandardItem {
            label: choice.label,
            activate: Box::new(move |menu: &mut Self| {
                // The callback must not block: it runs on the tray's own thread
                // and the menu is frozen until it returns. Sending is all it
                // does; the window does the rest.
                let _ = menu.orders.send(Msg::Control(command.clone()));
            }),
            ..Default::default()
        }
        .into()
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
impl ksni::Tray for Menu {
    fn id(&self) -> String {
        crate::gui::window::APP_ID.to_owned()
    }

    fn title(&self) -> String {
        "alate".to_owned()
    }

    /// A left click brings the window forward, which is what a person expects
    /// of an icon that stands for a window.
    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.orders.send(Msg::Control(Command::Show));
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let rgba = glyph(ICON);
        // ARGB32 in network byte order, which is the alpha moved to the front
        // of each pixel.
        let mut argb = Vec::with_capacity(rgba.len());
        for pixel in rgba.as_chunks::<4>().0 {
            argb.push(pixel[3]);
            argb.extend_from_slice(&pixel[..3]);
        }
        vec![ksni::Icon {
            width: ICON as i32,
            height: ICON as i32,
            data: argb,
        }]
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        rows()
            .into_iter()
            .map(|row| match row {
                Row::One(choice) => self.item(choice),
                Row::Separator => ksni::MenuItem::Separator,
                Row::Group { label, choices } => ksni::menu::SubMenu {
                    label,
                    submenu: choices
                        .into_iter()
                        .map(|choice| self.item(choice))
                        .collect(),
                    ..Default::default()
                }
                .into(),
            })
            .collect()
    }
}

/// Put the icon in the tray, whichever tray this desktop has.
///
/// The bus first, because a desktop that answers there wants a menu it draws
/// itself. A panel that speaks only XEmbed gets a docked window instead, with
/// the menu opening in the alate's own window since there is nothing on that
/// side to draw one with.
///
/// # Errors
///
/// Fails when neither protocol finds anybody, which is a desktop with no tray
/// at all. The window is reachable from the control socket either way, so this
/// is worth saying once and not worth stopping for.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(super) fn start(
    orders: UnboundedSender<Msg>,
    runtime: &tokio::runtime::Handle,
) -> Result<Tray, String> {
    use ksni::TrayMethods as _;

    // Registering is a D-Bus round trip, and it is done on the runtime the
    // window already owns rather than on an executor of ksni's own. It answers
    // rather than hanging when there is no watcher.
    let bus = runtime.block_on(
        Menu {
            orders: orders.clone(),
        }
        .spawn(),
    );
    match bus {
        Ok(handle) => {
            return Ok(Tray {
                _handle: Some(handle),
            });
        }
        Err(error) => {
            // Not a failure yet: most desktops that answer nothing here have a
            // panel that speaks the other protocol.
            let _ = error;
        }
    }

    // Docking blocks on X events for as long as the panel keeps the icon, so it
    // takes a thread. Whether it docked at all is known before that thread is
    // left to itself.
    let (told, docked) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("alate-tray".to_owned())
        .spawn(move || {
            let result = xembed::dock(|click| {
                let command = match click {
                    xembed::Click::Primary => Command::Show,
                    xembed::Click::Toggle => Command::Toggle,
                    xembed::Click::Menu => Command::Menu,
                };
                let _ = orders.send(Msg::Control(command));
            });
            // Only the first of these is listened for; the rest is the panel
            // going away long afterwards.
            let _ = told.send(result.as_ref().err().map(ToString::to_string));
        })
        .map_err(|error| format!("no thread to dock a tray icon on: {error}"))?;

    // The docking is a handful of round trips to the X server. Waiting for it
    // is what turns "no icon" into a reason.
    match docked.recv_timeout(std::time::Duration::from_secs(2)) {
        Ok(None) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(Tray { _handle: None }),
        Ok(Some(reason)) => Err(reason),
        Err(error) => Err(error.to_string()),
    }
}

// ---------------------------------------------------------------- macOS ----

/// Put the icon in the menu bar.
///
/// Must be called on the thread GPUI runs on: AppKit builds an `NSStatusItem`
/// nowhere else. The events arrive on `MenuEvent::receiver`, which the window
/// drains on its own beat rather than on a run loop of its own.
#[cfg(target_os = "macos")]
pub(super) fn start(
    orders: UnboundedSender<Msg>,
    runtime: &tokio::runtime::Handle,
) -> Result<Tray, String> {
    let _ = runtime;
    use tray_icon::menu::{Menu, MenuItem, Submenu};

    let menu = Menu::new();
    for row in rows() {
        match row {
            Row::One(choice) => {
                let item = MenuItem::with_id(choice.id, choice.label, true, None);
                menu.append(&item).map_err(|error| error.to_string())?;
            }
            Row::Separator => {
                menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
                    .map_err(|error| error.to_string())?;
            }
            Row::Group { label, choices } => {
                let group = Submenu::new(label, true);
                for choice in choices {
                    let item = MenuItem::with_id(choice.id, choice.label, true, None);
                    group.append(&item).map_err(|error| error.to_string())?;
                }
                menu.append(&group).map_err(|error| error.to_string())?;
            }
        }
    }

    let icon =
        tray_icon::Icon::from_rgba(glyph(ICON), ICON, ICON).map_err(|error| error.to_string())?;
    let tray = tray_icon::TrayIconBuilder::new()
        .with_id(crate::gui::window::APP_ID)
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip("alate")
        .build()
        .map_err(|error| error.to_string())?;
    // The receiver is global and lives as long as the process, so the window
    // drains it rather than a thread here doing so.
    let _ = orders;
    Ok(Tray { _icon: tray })
}

/// Turn whatever the menu bar reported into commands.
///
/// Called on the window's beat. On Linux it does nothing: there, the menu
/// callbacks send directly.
pub(super) fn drain(orders: &UnboundedSender<Msg>) {
    #[cfg(target_os = "macos")]
    {
        while let Ok(event) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            if let Some(command) = command_of(event.id.as_ref()) {
                let _ = orders.send(Msg::Control(command));
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = orders;
    }
}

/// The command an item's id stands for.
///
/// The ids are built in [`rows`] and matched here, which is what lets the macOS
/// menu be a plain list of ids with no closures hanging off it.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn command_of(id: &str) -> Option<Command> {
    match id {
        "show" => Some(Command::Show),
        "toggle" => Some(Command::Toggle),
        "mode" => Some(Command::Mode),
        "quit" => Some(Command::Quit),
        _ => {
            if let Some(name) = id.strip_prefix("familiar:") {
                Some(Command::Familiar {
                    name: name.to_owned(),
                })
            } else {
                id.strip_prefix("alate:").map(|name| Command::Instance {
                    name: name.to_owned(),
                })
            }
        }
    }
}

/// Nothing to put an icon in.
#[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
pub(super) fn start(
    orders: UnboundedSender<Msg>,
    runtime: &tokio::runtime::Handle,
) -> Result<Tray, String> {
    let _ = (orders, runtime);
    Err("this system has no tray".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_item_has_an_id_that_maps_back_to_its_command() {
        // What keeps the two platforms in step: the Linux menu carries the
        // command in a closure and the macOS one carries the id, so an id that
        // does not map back is an item that would do nothing on macOS.
        let mut seen = Vec::new();
        for row in rows() {
            let choices = match row {
                Row::One(choice) => vec![choice],
                Row::Group { choices, .. } => choices,
                Row::Separator => continue,
            };
            for choice in choices {
                assert_eq!(
                    command_of(&choice.id).as_ref(),
                    Some(&choice.command),
                    "{} does not map back",
                    choice.id
                );
                assert!(!seen.contains(&choice.id), "{} appears twice", choice.id);
                seen.push(choice.id);
            }
        }
        assert!(seen.len() >= 6, "the menu lost items: {seen:?}");
    }

    #[test]
    fn the_menu_offers_both_familiars_and_a_way_out() {
        let ids: Vec<String> = rows()
            .into_iter()
            .flat_map(|row| match row {
                Row::One(choice) => vec![choice.id],
                Row::Group { choices, .. } => choices.into_iter().map(|c| c.id).collect(),
                Row::Separator => Vec::new(),
            })
            .collect();
        assert!(ids.contains(&"familiar:sap".to_owned()));
        assert!(ids.contains(&"familiar:drift".to_owned()));
        assert!(ids.contains(&"quit".to_owned()));
    }

    #[test]
    fn the_icon_is_a_dot_and_not_a_square() {
        let pixels = glyph(ICON);
        assert_eq!(pixels.len() as u32, ICON * ICON * 4);
        // Transparent in the corner, opaque in the middle: the alpha is what
        // makes it a dot, and a tray that ignored it would show a green block.
        assert_eq!(pixels[3], 0, "the corner is not transparent");
        let middle = ((ICON / 2) * ICON + ICON / 2) as usize * 4;
        assert_eq!(pixels[middle + 3], 255, "the middle is not opaque");
    }
}
