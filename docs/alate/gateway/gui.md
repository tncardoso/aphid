# Window

`aphid alate gui` opens a window on an alate that runs. It is a client of the
[gateway](../gateway.md), in the same manner as `aphid alate attach` and the
Telegram bot: it holds no agent and no memory of its own, and closing it does
not stop the alate.

It is not built into every aphid. See [Building](#building).

```
aphid alate gui        [--name NAME]    open the window, or bring it forward
aphid alate gui toggle [--name NAME]    expand the window, or collapse it
aphid alate gui show   [--name NAME]    bring it forward
aphid alate gui mode   [--name NAME]    swap console and companion
aphid alate gui quit   [--name NAME]    close it. The alate keeps running
```

Only the first of those opens a window. The other four are a remote control for
the window that is already open, so they return at once, and they are the form
to bind to a key.

## One window

There is one window for the machine, not one for each alate. A second
`aphid alate gui` finds the first through `$APHID_HOME/gui.sock` and brings it
forward; with another `--name` it points that same window at another alate.

```console
$ aphid alate gui --name work      # opens
$ aphid alate gui --name work      # brings the same window forward
$ aphid alate gui --name notes     # points it at the other alate
```

Without `--name`, the window opens on the alate it was last pointed at.

## The two modes

| Mode | What it is |
| --- | --- |
| `console` | A bar across the top of the screen. Expanded, it grows downwards into the alate and what it is saying. |
| `companion` | A column of full height against the right edge: the log, the alate at its foot, and the text box. |

`aphid alate gui mode` swaps them, and so does the *Switch mode* item in the
tray. Swapping closes the window and opens another, because a window's place is
fixed when it is created. The connection is not touched: it belongs to the
program and not to the window, so the conversation carries on across the swap.

Expanding and collapsing the console is a resize, and it keeps its place.

**The console has no log.** Expanded, it is the alate and the balloon it speaks
in — what you glance at while you are doing something else. The log is a mode
away, and every conversation is still there when you get to it. Collapsed, the
bar carries the alate as a glyph, which is the whole of it until you open the
console again.

## Typing in it

Each line goes to the agent, unless it begins with `/`.

| Command | Effect |
| --- | --- |
| `/sessions` | Show the conversations, and open the list to pick one. |
| `/session <id>` | Look at one of them. A shortened id is enough. |
| `/new` | Start another conversation. |
| `/log` | Show or hide notices, heartbeats and session events. |
| `/clear` | Clear what is on screen. The memory does not change. |

| Key | Effect |
| --- | --- |
| `Enter` | Send. |
| `Shift-Enter` | Break the line instead. |
| `Esc` | Close the list or the question on screen; otherwise stop the run; otherwise collapse the console. |

The text box composes: a dead key makes `á`, and so do the input methods of the
system.

There is no model selector, for the reason there is none in the terminal: the
model is a property of the alate. Set `model` in
[`alate.json`](../../alate.md#alatejson).

## The creature

The alate is drawn in the window, and what it does follows the frames the
gateway is already sending. It thinks while a turn runs, talks while text arrives,
looks pleased for two seconds after a run that worked, is startled when a tool
asks permission, and sleeps when the connection is gone.

Two familiars, chosen from the tray:

| Familiar | What it is |
| --- | --- |
| `sap` | The winged aphid, drawn by hand. |
| `drift` | The same creature as a body that turns and ripples. |

On a machine with no device to draw on — no Vulkan, an old driver, a remote
session — the window opens anyway, and a line where the creature would have been
says why. The creature is an ornament; the client is the function.

### What it says

The last thing the alate said stands in a balloon above it, and stays there
until it says something else. It is the reply and nothing else: thinking is what
the face is for, and a tool is a line in the log. `Escape` puts the balloon
away.

In the companion the balloon is above the alate in its band, under the log that
holds everything. In the console it is the whole of what is written, since there
is no log there.

## The tray

The icon carries the same commands: *Show*, *Expand or collapse*, *Switch
mode*, *Familiar*, *Alate*, and *Quit the window*. A desktop with no tray gets
no icon and no complaint, and everything on it is still reachable from
`aphid alate gui`.

The list of alates under *Alate* is read when the window opens. One started
afterwards is reached with `aphid alate gui --name`.

## Waking an alate from the window

If nothing is listening, the window opens anyway, says the alate is asleep, and
offers to start it. Pressing that runs `aphid alate run --name <name>` in a
process group of its own, so it is not taken down with the window.

**This is the exception to the rule in [CLI](cli.md)** that putting an alate in
the background is the work of your system and not of the agent. The window is
already a program with a long life, and a companion that can only tell you to go
and open a terminal is not company. Everywhere else, that rule stands.

## When the connection breaks

The window reconnects, waiting one second, then two, four, eight, sixteen and
thirty. It keeps trying for as long as it is open.

The daemon opens a session for each connection, so what comes back is a **new
conversation** and not the old one carried on. The window says so rather than
drawing the next reply under the last as though nothing had happened. The old
conversation is still there: `/sessions` finds it.

## Where the window sits

Placing a window is not something a program can simply do, and what it can do
differs by system. The window asks; whether it is heard is the desktop's
business.

| Desktop | What happens |
| --- | --- |
| X11 | The window is moved into place and asked to stay above the others, out of the taskbar and out of the pager. A tiling window manager will refuse all of that and tile it; see below. |
| macOS | The window is given a floating level and follows you between spaces. It is placed when it is created. |
| Wayland | Nothing. No program places its own windows there. |

A tiling window manager tiles this window like any other, whatever it asks for,
so it wants the same kind of rule Wayland needs. In i3, in `config`:

```
for_window [class="com.embornal.aphid.alate"] floating enable, sticky enable
```

On Wayland, write a rule in your compositor. The window's app id is
`com.embornal.aphid.alate`.

Hyprland, in `hyprland.conf`:

```
windowrulev2 = float, class:^(com\.embornal\.aphid\.alate)$
windowrulev2 = pin, class:^(com\.embornal\.aphid\.alate)$
windowrulev2 = move 25% 0, class:^(com\.embornal\.aphid\.alate)$
```

Sway, in `config`:

```
for_window [app_id="com.embornal.aphid.alate"] floating enable, sticky enable, move position 25 ppt 0
```

## Binding it to a key

The window has no hotkey of its own, by design: a program that grabs keys for
the whole desktop is a program that fights every other one. Bind the verb
instead.

Hyprland:

```
bind = SUPER, grave, exec, aphid alate gui toggle --name work
```

Sway or i3:

```
bindsym $mod+grave exec aphid alate gui toggle --name work
```

skhd, on macOS:

```
cmd - 0x32 : aphid alate gui toggle --name work
```

## `gui.json`

The window remembers where it was, in `$APHID_HOME/gui.json` — beside `alate/`,
and not inside any one alate's home, because there is one window.

```json
{
  "version": 1,
  "mode": "console",
  "familiar": "sap",
  "instance": "work"
}
```

| Key | Effect |
| --- | --- |
| `mode` | `console` or `companion`. A file that still says `quake` opens the console: that is what this mode was called at first. |
| `familiar` | `sap` or `drift`. |
| `instance` | The alate to open on when `--name` is absent. |

A missing file, an empty one, and a key this build has no name for all give the
defaults. Nothing about where a window sits is worth refusing to open one over.

## Building

The window is behind the `gui` cargo feature, which is on by default. The
release binaries carry it. A build without it keeps the whole agent and stops
compiling the window library, which is most of the build:

```console
$ cargo install aphid-ai --no-default-features
$ aphid alate gui
aphid: this build has no graphical interface. Reinstall with `cargo install aphid-ai --features gui`
```

The gateway needs a Unix socket, so `aphid alate gui` does not work on Windows.
