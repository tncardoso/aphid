//! The other tray protocol: an icon docked in a panel's own window.
//!
//! `ksni` speaks StatusNotifierItem, which is what KDE, a GNOME with the
//! extension, waybar and swaybar listen for. A great many desktops listen for
//! nothing of the sort: i3 with polybar, xfce4-panel, stalonetray, trayer and
//! everything else built on the older **XEmbed** system tray specification,
//! where an icon is not a message on a bus but a window a panel adopts.
//!
//! Neither protocol can be asked to do the other's job, so this is the second
//! half. A panel that owns `_NET_SYSTEM_TRAY_S<screen>` is asked to take a
//! window of ours; that window draws the icon and reports the clicks.
//!
//! ## What it does not do
//!
//! It draws no menu. A menu here would be a window of our own, hit-tested and
//! drawn by hand, in a program that already has a perfectly good window with
//! buttons in it — so the right button opens the menu **in that window**
//! instead. What the icon carries is the pointer, which is the part the control
//! socket and a key binding cannot give you.

use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Arc as XArc, AtomEnum, ChangeGCAux, ClientMessageEvent, ColormapAlloc, ConnectionExt as _,
    CreateGCAux, CreateWindowAux, EventMask, ImageFormat, PropMode, Screen, Visualid, Window,
    WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;

use super::glyph;
use crate::gui::window::APP_ID;

/// Ask the panel to take this window. The one opcode of the specification this
/// side needs.
const REQUEST_DOCK: u32 = 0;
/// `_XEMBED_INFO` says the client is ready to be shown.
const XEMBED_MAPPED: u32 = 1;
/// How big the icon is drawn before a panel says otherwise.
const ICON: u16 = 22;
/// The background-pixmap that means "whatever my parent has behind me". X11
/// names it `ParentRelative`; x11rb has no name for it, only the number.
const PARENT_RELATIVE: u32 = 1;
/// The green of the wordmark, which is what the icon is.
const ACCENT: (u8, u8, u8) = (0x80, 0xc9, 0x6b);

/// What a click on the icon meant.
pub enum Click {
    /// The left button: the plain thing, which is to come forward.
    Primary,
    /// The right button: everything else, which is the menu.
    Menu,
    /// The middle button, which is the other plain thing.
    Toggle,
}

/// What the panel will let the icon be drawn with.
struct Look {
    /// Whether the panel gave a deeper visual than the screen's, which is a
    /// panel that composites and an icon that may carry its own alpha.
    composited: bool,
    depth: u8,
    /// How many bytes one pixel takes at that depth.
    bytes: usize,
    /// The accent, in whatever order this visual keeps its channels.
    accent: u32,
}

/// Dock an icon and report the clicks on it.
///
/// Blocking, and meant for a thread of its own. It ends when the panel lets the
/// icon go or the display does.
///
/// # Errors
///
/// Fails when there is no X server, or when nothing owns the tray selection,
/// which is the ordinary case of a desktop whose panel has no tray in it.
pub fn dock(clicked: impl Fn(Click)) -> Result<(), Box<dyn Error>> {
    let (connection, screen_index) = x11rb::connect(None)?;
    let screen = connection
        .setup()
        .roots
        .get(screen_index)
        .ok_or("the X server reported no screen")?
        .clone();

    let selection = atom(
        &connection,
        format!("_NET_SYSTEM_TRAY_S{screen_index}").as_bytes(),
    )?;
    let owner = connection.get_selection_owner(selection)?.reply()?.owner;
    if owner == x11rb::NONE {
        return Err("no panel on this screen has a tray in it".into());
    }

    // A panel that composites wants a deeper visual, and says which one. A
    // window of a different depth from its parent inherits nothing, so the
    // colormap and the border have to be given as well.
    let visual = tray_visual(&connection, owner)?.unwrap_or(screen.root_visual);
    let depth = depth_of(&screen, visual).unwrap_or(screen.root_depth);
    let composited = depth > screen.root_depth;
    let colormap = connection.generate_id()?;
    connection.create_colormap(ColormapAlloc::NONE, colormap, screen.root, visual)?;

    let mut aux = CreateWindowAux::new()
        .border_pixel(0)
        .colormap(colormap)
        .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY);
    aux = if composited {
        // Nothing behind the icon: the panel puts its own bar there when it
        // composites what we draw over it.
        aux.background_pixel(0)
    } else {
        // There is no alpha to composite with, so the panel's background is
        // borrowed instead. `ParentRelative` fills from whatever window this is
        // adopted into, which is the bar — without it the icon is a black
        // square on a bar of some other colour.
        aux.background_pixmap(PARENT_RELATIVE)
    };

    let window = connection.generate_id()?;
    connection.create_window(
        depth,
        window,
        screen.root,
        0,
        0,
        ICON,
        ICON,
        0,
        WindowClass::INPUT_OUTPUT,
        visual,
        &aux,
    )?;

    // What the panel reads to know the icon is ready to be shown.
    let info = atom(&connection, b"_XEMBED_INFO")?;
    connection.change_property32(PropMode::REPLACE, window, info, info, &[0, XEMBED_MAPPED])?;
    // Say what this is, so a panel that sorts or styles its icons has a name.
    let class = format!("{APP_ID}\0{APP_ID}\0");
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        class.as_bytes(),
    )?;

    let opcode = atom(&connection, b"_NET_SYSTEM_TRAY_OPCODE")?;
    connection.send_event(
        false,
        owner,
        EventMask::NO_EVENT,
        ClientMessageEvent::new(
            32,
            owner,
            opcode,
            [x11rb::CURRENT_TIME, REQUEST_DOCK, window, 0, 0],
        ),
    )?;
    connection.flush()?;

    let gc = connection.generate_id()?;
    connection.create_gc(gc, window, &CreateGCAux::new())?;

    let look = Look {
        composited,
        depth,
        // How many bytes the server wants for one pixel at this depth. Usually
        // four even at depth 24, where the fourth is ignored — but that is the
        // server's to report and not ours to assume, and `PutImage` refuses the
        // whole request when the answer is wrong.
        bytes: connection
            .setup()
            .pixmap_formats
            .iter()
            .find(|format| format.depth == depth)
            .map_or(4, |format| usize::from(format.bits_per_pixel / 8))
            .max(3),
        accent: pixel_of(&screen, visual, ACCENT),
    };

    let mut size = ICON;
    loop {
        match connection.wait_for_event()? {
            Event::Expose(_) | Event::MapNotify(_) => {
                paint(&connection, window, gc, size, &look)?;
            }
            // The panel decides how big its icons are.
            Event::ConfigureNotify(event) => {
                let wanted = u32::from(event.width.min(event.height)).max(1);
                if wanted != u32::from(size) {
                    size = u16::try_from(wanted).unwrap_or(ICON);
                    paint(&connection, window, gc, size, &look)?;
                }
            }
            Event::ButtonPress(event) => match event.detail {
                1 => clicked(Click::Primary),
                2 => clicked(Click::Toggle),
                3 => clicked(Click::Menu),
                _ => {}
            },
            // The panel let go of the icon, or went away. There is nothing to
            // be docked into any more.
            Event::UnmapNotify(event) if event.window == window => return Ok(()),
            Event::DestroyNotify(event) if event.window == window => return Ok(()),
            _ => {}
        }
    }
}

/// Draw the icon.
///
/// Two ways, because the panel decides which is possible. Where it composites,
/// the dot is an image with an alpha edge. Where it does not, an image would
/// have to be blended against a background nobody reports, so the dot is a
/// filled circle drawn over whatever the panel put there — no alpha, no edge to
/// get wrong, and the bar showing through the corners.
fn paint(
    connection: &impl Connection,
    window: Window,
    gc: u32,
    size: u16,
    look: &Look,
) -> Result<(), Box<dyn Error>> {
    if !look.composited {
        connection.clear_area(false, window, 0, 0, 0, 0)?;
        connection.change_gc(gc, &ChangeGCAux::new().foreground(look.accent))?;
        let inset = if size > 4 { 2 } else { 0 };
        connection.poly_fill_arc(
            window,
            gc,
            &[XArc {
                x: i16::try_from(inset / 2).unwrap_or(0),
                y: i16::try_from(inset / 2).unwrap_or(0),
                width: size - inset,
                height: size - inset,
                angle1: 0,
                // Sixty-fourths of a degree, all the way round.
                angle2: 360 * 64,
            }],
        )?;
        connection.flush()?;
        return Ok(());
    }

    let rgba = glyph(u32::from(size));
    // Blue first: that is the order a little-endian server stores the channels
    // of a pixel it reads as a number. The colours are multiplied by the alpha,
    // which is what a panel that composites expects.
    let mut pixels = Vec::with_capacity(rgba.len());
    for pixel in rgba.as_chunks::<4>().0 {
        let alpha = u32::from(pixel[3]);
        let scale = |value: u8| u8::try_from(u32::from(value) * alpha / 255).unwrap_or(value);
        pixels.push(scale(pixel[2]));
        pixels.push(scale(pixel[1]));
        pixels.push(scale(pixel[0]));
        if look.bytes > 3 {
            pixels.push(pixel[3]);
        }
    }
    connection.put_image(
        ImageFormat::Z_PIXMAP,
        window,
        gc,
        size,
        size,
        0,
        0,
        0,
        look.depth,
        &pixels,
    )?;
    connection.flush()?;
    Ok(())
}

/// The visual the panel wants its icons in, when it names one.
fn tray_visual(
    connection: &impl Connection,
    owner: Window,
) -> Result<Option<Visualid>, Box<dyn Error>> {
    let property = atom(connection, b"_NET_SYSTEM_TRAY_VISUAL")?;
    let reply = connection
        .get_property(false, owner, property, AtomEnum::VISUALID, 0, 1)?
        .reply()?;
    Ok(reply.value32().and_then(|mut ids| ids.next()))
}

/// How deep a visual is, since a window has to be created with both.
fn depth_of(screen: &Screen, visual: Visualid) -> Option<u8> {
    screen.allowed_depths.iter().find_map(|depth| {
        depth
            .visuals
            .iter()
            .any(|candidate| candidate.visual_id == visual)
            .then_some(depth.depth)
    })
}

/// A colour as this visual's pixel value.
///
/// A visual says where each channel sits in a pixel. Every screen in use today
/// puts them where you would guess, but the masks are there to be read.
fn pixel_of(screen: &Screen, visual: Visualid, (red, green, blue): (u8, u8, u8)) -> u32 {
    let found = screen
        .allowed_depths
        .iter()
        .flat_map(|depth| depth.visuals.iter())
        .find(|candidate| candidate.visual_id == visual);
    let Some(kind) = found else {
        return u32::from_be_bytes([0, red, green, blue]);
    };
    place(red, kind.red_mask) | place(green, kind.green_mask) | place(blue, kind.blue_mask)
}

/// One channel, moved to where a mask says it goes.
fn place(value: u8, mask: u32) -> u32 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let width = mask.count_ones().min(8);
    let scaled = u32::from(value) >> (8 - width);
    (scaled << shift) & mask
}

fn atom(connection: &impl Connection, name: &[u8]) -> Result<u32, Box<dyn Error>> {
    Ok(connection.intern_atom(false, name)?.reply()?.atom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_channel_goes_where_its_mask_says() {
        // The ordinary screen: eight bits each, red highest.
        assert_eq!(place(0xff, 0x00ff_0000), 0x00ff_0000);
        assert_eq!(place(0x80, 0x0000_ff00), 0x0000_8000);
        assert_eq!(place(0x6b, 0x0000_00ff), 0x0000_006b);
    }

    #[test]
    fn a_narrower_channel_keeps_its_high_bits() {
        // 5-6-5, which is what an old server or a virtual one may report.
        assert_eq!(place(0xff, 0b1111_1000_0000_0000), 0b1111_1000_0000_0000);
        assert_eq!(place(0x00, 0b0000_0111_1110_0000), 0);
    }

    #[test]
    fn a_mask_of_nothing_contributes_nothing() {
        assert_eq!(place(0xff, 0), 0);
    }
}
