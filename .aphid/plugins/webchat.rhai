//! Chat with this session from a browser. Use /server start and /server stop.

// How it works. A plugin is only awake inside a hook, so the server cannot live
// here: `/server start` writes a small Python server into /tmp and starts it
// detached, which keeps it listening between hooks. What the browser sends is
// appended to an inbox file, which `on_tick` reads and puts to the model. What
// the model says goes the other way, as one POST for each chunk of the reply.
//
// Nothing is written in the workspace, and nothing is left running: the pid goes
// in the plugin's memory, and `/server stop` and the end of the session use it.

/// The page the browser gets.
///
/// A function and not a constant, because a command body is called without the
/// script's scope and `global::` reaches nothing from there. No backtick and no
/// dollar-brace anywhere inside: this is a Rhai literal string, and it would
/// read those as interpolation.
fn page() { `
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>aphid</title>
<style>
  :root {
    color-scheme: light dark;
    --bg: #fbfbfa; --fg: #1c1c1a; --dim: #6b6b66;
    --user: #e7e4dc; --tool: #f0efea; --line: #dedcd4; --accent: #7a5cff;
  }
  @media (prefers-color-scheme: dark) {
    :root {
      --bg: #14140f; --fg: #e8e6dd; --dim: #8f8d83;
      --user: #26261e; --tool: #1d1d17; --line: #2f2f26; --accent: #a48bff;
    }
  }
  * { box-sizing: border-box; }
  body {
    margin: 0; background: var(--bg); color: var(--fg);
    font: 15px/1.55 ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
    display: flex; flex-direction: column; height: 100dvh;
  }
  header {
    padding: .7rem 1rem; border-bottom: 1px solid var(--line);
    display: flex; gap: .6rem; align-items: baseline;
  }
  header b { font-weight: 620; letter-spacing: .02em; }
  header span { color: var(--dim); font-size: .82rem; }
  #dot { width: .5rem; height: .5rem; border-radius: 50%; background: var(--dim); }
  #dot.live { background: var(--accent); }
  main { flex: 1; overflow-y: auto; padding: 1rem; }
  .wrap { max-width: 46rem; margin: 0 auto; display: flex; flex-direction: column; gap: .7rem; }
  .msg { padding: .6rem .8rem; border-radius: .7rem; white-space: pre-wrap; overflow-wrap: anywhere; }
  .user { background: var(--user); align-self: flex-end; max-width: 85%; }
  .assistant { background: transparent; padding-left: 0; padding-right: 0; }
  .tool {
    background: var(--tool); color: var(--dim); border-radius: .5rem;
    font: 12.5px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace;
    padding: .4rem .6rem; overflow-x: auto; white-space: pre;
  }
  .notice { color: var(--dim); font-size: .85rem; font-style: italic; }
  footer { border-top: 1px solid var(--line); padding: .7rem 1rem; }
  form { max-width: 46rem; margin: 0 auto; display: flex; gap: .6rem; }
  textarea {
    flex: 1; resize: none; border: 1px solid var(--line); border-radius: .6rem;
    background: var(--bg); color: var(--fg); padding: .55rem .7rem;
    font: inherit; min-height: 2.6rem; max-height: 9rem;
  }
  textarea:focus { outline: 2px solid var(--accent); outline-offset: -1px; }
  button {
    border: 0; border-radius: .6rem; background: var(--accent); color: #fff;
    padding: 0 1.1rem; font: inherit; font-weight: 600; cursor: pointer;
  }
  button:disabled { opacity: .5; cursor: default; }
</style>
</head>
<body>
<header><b>aphid</b><span id="dot"></span><span id="state">connecting</span></header>
<main><div class="wrap" id="log"></div></main>
<footer>
  <form id="composer">
    <textarea id="text" rows="1" placeholder="Message the session. Enter sends, Shift+Enter is a new line."></textarea>
    <button id="send" type="submit">Send</button>
  </form>
</footer>
<script>
(function () {
  var log = document.getElementById("log");
  var main = document.querySelector("main");
  var box = document.getElementById("text");
  var dot = document.getElementById("dot");
  var state = document.getElementById("state");
  var open = null;

  function atBottom() {
    return main.scrollHeight - main.scrollTop - main.clientHeight < 60;
  }
  function scroll(stick) {
    if (stick) main.scrollTop = main.scrollHeight;
  }
  function add(kind, text) {
    var stick = atBottom();
    var node = document.createElement("div");
    node.className = "msg " + kind;
    node.textContent = text;
    log.appendChild(node);
    scroll(stick);
    return node;
  }

  function apply(item) {
    if (item.kind === "user") { open = null; add("user", item.text); return; }
    if (item.kind === "delta") {
      var stick = atBottom();
      if (!open) open = add("assistant", "");
      open.textContent += item.text;
      scroll(stick);
      return;
    }
    if (item.kind === "tool") { open = null; add("tool", item.text); return; }
    if (item.kind === "notice") { open = null; add("notice", item.text); return; }
    if (item.kind === "end") { open = null; }
  }

  var feed = new EventSource("/feed");
  feed.onopen = function () {
    dot.classList.add("live");
    state.textContent = "connected";
  };
  feed.onerror = function () {
    dot.classList.remove("live");
    state.textContent = "reconnecting";
  };
  feed.onmessage = function (event) {
    apply(JSON.parse(event.data));
  };

  function send() {
    var text = box.value.trim();
    if (!text) return;
    box.value = "";
    fetch("/send", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ text: text })
    }).catch(function () {
      add("notice", "could not reach the session");
    });
  }

  document.getElementById("composer").addEventListener("submit", function (event) {
    event.preventDefault();
    send();
  });
  box.addEventListener("keydown", function (event) {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      send();
    }
  });
  box.focus();
})();
</script>
</body>
</html>
` }

/// The server, in Python. Standard library only, so nothing has to be
/// installed for it to run.
fn server_source() { `
"""The web chat for one aphid session. Started by the webchat plugin.

Reads APHID_TOKEN, APHID_PORT and APHID_HOST from the environment, and keeps
every file it needs beside itself: index.html to serve, inbox for what the
browser sends, ready to say that the port is open.
"""

import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse, parse_qs

TOKEN = os.environ.get("APHID_TOKEN", "")
PORT = int(os.environ.get("APHID_PORT", "8000"))
HOST = os.environ.get("APHID_HOST", "127.0.0.1")
HERE = os.path.dirname(os.path.abspath(__file__))
KEPT = 1000

feed = []
waiting = threading.Condition()
writing = threading.Lock()


def publish(kind, text):
    """Add one item to the feed and wake every reader."""
    with waiting:
        seq = feed[-1][0] + 1 if feed else 1
        feed.append((seq, kind, text))
        del feed[:-KEPT]
        waiting.notify_all()


def since(cursor):
    with waiting:
        return [item for item in feed if item[0] > cursor]


def enqueue(text):
    """Append one message for the plugin to pick up.

    One message is one line, so a newline becomes an escape. The plugin puts it
    back before the model sees it.
    """
    line = text.replace("\r", "").replace("\n", "\\n").strip()
    if not line:
        return
    with writing:
        with open(os.path.join(HERE, "inbox"), "a", encoding="utf-8") as inbox:
            inbox.write(line + "\n")
            inbox.flush()
            os.fsync(inbox.fileno())


class Chat(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        """Quiet: the log file is for failures, not for traffic."""

    def allowed(self, query):
        if not TOKEN:
            return True
        if query.get("t", [""])[0] == TOKEN:
            return True
        if self.headers.get("x-aphid-token") == TOKEN:
            return True
        return ("aphid=" + TOKEN) in self.headers.get("Cookie", "")

    def same_site(self):
        """Refuse a request another site sent on the user's behalf.

        The cookie is SameSite=Strict, so a cross-site request arrives without
        it; this refuses the ones that carry a foreign Origin as well, which is
        what keeps a page in another tab from driving the session.
        """
        origin = self.headers.get("Origin")
        if not origin:
            return True
        return origin.endswith("//" + self.headers.get("Host", ""))

    def local(self):
        return self.client_address[0] in ("127.0.0.1", "::1")

    def reply(self, code, body=b"", kind="text/plain; charset=utf-8", cookie=None):
        self.send_response(code)
        self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body)))
        if cookie:
            self.send_header("Set-Cookie", cookie)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def do_GET(self):
        url = urlparse(self.path)
        query = parse_qs(url.query)

        if not self.allowed(query):
            self.reply(403, b"this session needs the link it printed\n")
            return

        if url.path in ("/", "/index.html"):
            with open(os.path.join(HERE, "index.html"), "rb") as page:
                body = page.read()
            cookie = "aphid=" + TOKEN + "; Path=/; SameSite=Strict; Max-Age=86400"
            self.reply(200, body, "text/html; charset=utf-8", cookie if TOKEN else None)
            return

        if url.path == "/feed":
            self.stream()
            return

        self.reply(404, b"no\n")

    def do_POST(self):
        url = urlparse(self.path)
        query = parse_qs(url.query)

        if not self.allowed(query) or not self.same_site():
            self.reply(403, b"no\n")
            return

        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length).decode("utf-8", "replace")

        if url.path == "/send":
            try:
                text = json.loads(body).get("text", "")
            except ValueError:
                self.reply(400, b"expected json\n")
                return
            enqueue(text)
            self.reply(204)
            return

        # The plugin's own side of the feed, which never leaves this machine.
        if url.path == "/push" and self.local():
            kind, _, text = body.partition("\n")
            publish(kind, text)
            self.reply(204)
            return

        self.reply(404, b"no\n")

    def stream(self):
        """Server-sent events, closed by hanging up rather than by a length."""
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True

        try:
            cursor = int(self.headers.get("Last-Event-ID") or 0)
        except ValueError:
            cursor = 0

        try:
            while True:
                items = since(cursor)
                if not items:
                    with waiting:
                        waiting.wait(15)
                    items = since(cursor)
                if not items:
                    self.wfile.write(b": still here\n\n")
                    self.wfile.flush()
                    continue
                for seq, kind, text in items:
                    cursor = seq
                    payload = json.dumps({"kind": kind, "text": text})
                    self.wfile.write(("id: %d\ndata: %s\n\n" % (seq, payload)).encode("utf-8"))
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, ValueError):
            return


def main():
    os.umask(0o077)
    server = ThreadingHTTPServer((HOST, PORT), Chat)
    server.daemon_threads = True
    # Written last: the plugin waits for this file to know the port is open.
    with open(os.path.join(HERE, "ready"), "w", encoding="utf-8") as ready:
        ready.write(str(os.getpid()))
    server.serve_forever()


main()
` }

register_command(#{
    name: "server",
    description: "Open or close the browser chat for this session.",
    run: |args| {
        let action = trimmed(args);
        if action == "start" {
            start()
        } else if action == "stop" {
            stop()
        } else if action == "" || action == "status" {
            report()
        } else {
            notice("usage: /server start | stop | status")
        }
    }
});

/// A trimmed copy.
///
/// Rhai's own `trim` works on the string in place and gives back nothing, so
/// this is the version that can be used as a value.
fn trimmed(text) {
    let out = text;
    out.trim();
    out
}

/// Where everything this plugin needs at runtime lives.
///
/// Under /tmp, named after the project, so the repository stays clean and two
/// projects never write over each other.
fn scratch() {
    let path = trimmed(exec("pwd").stdout);
    let parts = path.split('/');
    let name = parts[parts.len() - 1];
    if name == "" {
        name = "workspace";
    }
    "/tmp/aphid-webchat/" + name
}

/// The address to print, which is the one another machine can reach.
fn address() {
    let found = trimmed(exec("hostname -I 2>/dev/null | awk '{ print $1 }'").stdout);
    if found == "" { "127.0.0.1" } else { found }
}

fn start() {
    let memory = state();
    if memory.serving == true {
        return notice("already serving on " + memory.url);
    }
    if exec("command -v python3").status != 0 {
        return notice("the chat needs python3, which is not on this machine");
    }

    let settings = config();
    let port = settings.port ?? 8000;
    let host = settings.host ?? "0.0.0.0";
    let dir = scratch();
    let token = trimmed(exec("head -c 16 /dev/urandom | od -An -tx1 | tr -d '[:space:]'").stdout);

    fs_write(dir + "/server.py", server_source());
    fs_write(dir + "/index.html", page());
    fs_write(dir + "/inbox", "");

    // One `exec`, because a command runs on the loop that draws the screen and
    // every call blocks it. The redirect is not optional either: `exec` reads
    // the shell's output to the end, and a detached child still holding that
    // pipe would never let it finish.
    let settings_env = "APHID_TOKEN=" + token;
    settings_env += " APHID_PORT=" + port;
    settings_env += " APHID_HOST=" + host;

    let script = "cd " + dir + " || exit 1\n";
    script += "rm -f ready\n";
    script += settings_env + " nohup python3 server.py > log 2>&1 &\n";
    script += "echo $! > pid\n";
    script += "i=0; while [ $i -lt 20 ]; do [ -f ready ] && break; sleep 0.1; i=$((i+1)); done";
    exec(script);

    if !fs_exists(dir + "/ready") {
        let why = "";
        if fs_exists(dir + "/log") {
            why = trimmed(fs_read(dir + "/log"));
        }
        return notice("the chat server did not start. " + why);
    }

    let url = "http://" + address() + ":" + port + "/?t=" + token;
    save_state(#{
        serving: true,
        dir: dir,
        port: port,
        token: token,
        pid: trimmed(fs_read(dir + "/pid")),
        url: url,
        cursor: 0
    });

    notice("chat open on " + url)
}

fn stop() {
    let memory = state();
    if memory.serving != true {
        return notice("the chat is not running");
    }
    close(memory);
    notice("chat closed")
}

fn report() {
    let memory = state();
    if memory.serving == true {
        notice("chat open on " + memory.url)
    } else {
        notice("the chat is closed. /server start opens it")
    }
}

/// Stop the server this plugin started, and only that one.
///
/// The pid comes from this project's own memory, so a session in another
/// project keeps the server it started.
fn close(memory) {
    if memory.pid != () && memory.pid != "" {
        exec("kill " + memory.pid + " 2>/dev/null; true");
    }
    save_state(#{ serving: false, dir: memory.dir, cursor: 0 });
}

/// Send one item to the browser.
///
/// Failure is ignored on purpose: if the server has gone, a session should
/// carry on quietly rather than report every chunk of every reply.
fn push(kind, text) {
    let memory = state();
    if memory.serving != true {
        return;
    }
    try {
        http_post(
            "http://127.0.0.1:" + memory.port + "/push",
            kind + "\n" + text,
            #{ "x-aphid-token": memory.token }
        );
    } catch (error) {
    }
    ()
}

/// Read what the browser sent and put it to the model.
///
/// The inbox is only ever appended to, and this remembers how far it has read,
/// so nothing is deleted while the server may be writing it.
fn on_tick() {
    let memory = state();
    if memory.serving != true {
        return;
    }

    let path = memory.dir + "/inbox";
    if !fs_exists(path) {
        return;
    }

    let lines = [];
    for line in fs_read(path).split('\n') {
        let entry = trimmed(line);
        if entry != "" {
            lines.push(entry);
        }
    }

    let cursor = memory.cursor ?? 0;
    // The file was emptied, which a restart does. Start again rather than wait
    // for it to grow back past a cursor that no longer means anything.
    if lines.len() < cursor {
        cursor = 0;
    }
    if lines.len() == cursor {
        return;
    }

    while cursor < lines.len() {
        let message = lines[cursor];
        message.replace("\\n", "\n");
        prompt(message);
        cursor += 1;
    }

    memory.cursor = cursor;
    save_state(memory);
}

// Everything the browser is shown. `on_prompt` is the one place a message is
// echoed, so a line typed in the terminal reaches the browser as well.
fn on_prompt(request) {
    push("user", request.text);
}

fn on_event(event) {
    if event.kind == "delta" && event.block == "text" {
        push("delta", event.text);
    }
}

fn on_tool_call(tool) {
    push("tool", tool.name + " " + tool.arguments);
}

fn on_run_end(cx, outcome) {
    push("end", outcome.stop);
}

fn on_notify(text) {
    push("notice", text);
}

/// A closed session should not leave a server holding the port.
fn on_session_end(session) {
    let memory = state();
    if memory.serving == true {
        close(memory);
    }
}
