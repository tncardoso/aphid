//! The X11 half: above the others, out of the taskbar, and where we said.
//!
//! A port of what the desktop companion this borrows from does in Python. The
//! window manager is asked through EWMH, which is a client message to the root
//! window, and moved with `ConfigureWindow`. Both are requests and not orders:
//! a window manager is free to ignore either, and several do.

use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, EventMask, Window,
};

/// Add a property to `_NET_WM_STATE`.
const ADD: u32 = 1;
/// Say the request comes from a program and not from a person's pointer.
const FROM_APPLICATION: u32 = 1;

/// Put our windows on top and at `origin`.
///
/// Silent about everything: no X server, no window manager, a manager that
/// refuses, or a window that is not in the client list yet.
pub fn place(origin: (i32, i32)) {
    let _ = try_place(origin);
}

fn try_place(origin: (i32, i32)) -> Result<(), Box<dyn Error>> {
    // No `DISPLAY`, or a Wayland session with no X server, ends here.
    let (connection, screen) = x11rb::connect(None)?;
    let root = connection
        .setup()
        .roots
        .get(screen)
        .ok_or("the X server reported no screen")?
        .root;

    let state = atom(&connection, b"_NET_WM_STATE")?;
    let above = atom(&connection, b"_NET_WM_STATE_ABOVE")?;
    let skip_taskbar = atom(&connection, b"_NET_WM_STATE_SKIP_TASKBAR")?;
    let skip_pager = atom(&connection, b"_NET_WM_STATE_SKIP_PAGER")?;

    for window in ours(&connection, root)? {
        // Two properties fit in one client message, so the third goes in a
        // second one. The specification says so, and a manager that reads only
        // the first two would silently drop the third.
        raise(&connection, root, window, state, above, skip_taskbar)?;
        raise(&connection, root, window, state, skip_pager, 0)?;
        connection.configure_window(window, &ConfigureWindowAux::new().x(origin.0).y(origin.1))?;
    }
    connection.flush()?;
    Ok(())
}

/// The windows on this screen that belong to this process.
///
/// `_NET_CLIENT_LIST` is what the window manager is managing, and every window
/// GPUI opens carries `_NET_WM_PID`. A build with more than one window on
/// screen gets each of them placed, which is what a companion that reopened
/// itself to change mode wants.
fn ours(connection: &impl Connection, root: Window) -> Result<Vec<Window>, Box<dyn Error>> {
    let list = atom(connection, b"_NET_CLIENT_LIST")?;
    let pid_atom = atom(connection, b"_NET_WM_PID")?;
    let reply = connection
        .get_property(false, root, list, AtomEnum::WINDOW, 0, u32::MAX)?
        .reply()?;
    let Some(windows) = reply.value32() else {
        // No window manager, or one that does not keep the list. Nothing to
        // ask, and nothing that would have listened.
        return Ok(Vec::new());
    };

    let mine = std::process::id();
    let mut found = Vec::new();
    for window in windows {
        let reply = connection
            .get_property(false, window, pid_atom, AtomEnum::CARDINAL, 0, 1)?
            .reply();
        // A window can be destroyed between the list and this question.
        let Ok(reply) = reply else { continue };
        if reply.value32().and_then(|mut ids| ids.next()) == Some(mine) {
            found.push(window);
        }
    }
    Ok(found)
}

/// Ask for one or two `_NET_WM_STATE` properties to be added.
fn raise(
    connection: &impl Connection,
    root: Window,
    window: Window,
    state: u32,
    first: u32,
    second: u32,
) -> Result<(), Box<dyn Error>> {
    let event =
        ClientMessageEvent::new(32, window, state, [ADD, first, second, FROM_APPLICATION, 0]);
    connection.send_event(
        false,
        root,
        // A state change goes to the root as a request to the manager, which is
        // what these two masks say: it is for whoever is redirecting this
        // window's structure.
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        event,
    )?;
    Ok(())
}

fn atom(connection: &impl Connection, name: &[u8]) -> Result<u32, Box<dyn Error>> {
    Ok(connection.intern_atom(false, name)?.reply()?.atom)
}
