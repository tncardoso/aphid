# Sandbox

Alate runs commands in a Bubblewrap sandbox. The sandbox protects the host
from commands that an agent or a plugin starts. It is enabled by default.

The sandbox applies to Alate command execution. It does not sandbox the Alate
daemon, the model gateway, or the user interface.

## Architecture

The Alate daemon stays outside the sandbox. It loads the agent configuration,
the sandbox policy, and the plugin scripts. It then creates one command
launcher for the agent.

When a model command or a Rhai plugin calls `exec`, Aphid sends the command to
this launcher. The launcher starts `bwrap`, which starts `bash -c` in a new
sandbox. Each command gets a new sandbox process.

```text
model command or plugin exec
            |
            v
     Aphid command registry
            |
            v
 Bubblewrap command launcher
            |
            v
       sandboxed bash -c
```

Built-in file tools run in the daemon. Their path checks limit them to the
agent workspace and to the paths that the policy grants. Rhai plugin file
operations stay limited to the workspace. Policy grants apply to shell
commands and built-in file tools.

## Filesystem boundary

The workspace is the only host data directory that Alate exposes by default.
It is writable. The sandbox creates an empty temporary directory and uses it
for `HOME`, temporary files, and XDG data paths.

The command can read a small runtime view of the operating system. This view
includes system program and library directories such as `/usr`, `/bin`, and
`/lib`, plus read-only `/etc`. It is needed to run shell commands. It does not
include the host home directory or other user data directories.

The sandbox creates new user, process, IPC, UTS, and cgroup namespaces. It
also creates new `/proc` and `/dev` mounts. A command cannot see host processes
through `/proc`.

You can grant more paths with `read_only` or `read_write`. Grant the smallest
path that a command needs. Aphid rejects grants that overlap each other or the
workspace, because an overlapping grant can make the policy unclear.

## Network boundary

The default `host` setting keeps network access. Set `network` to `none` to
create a network namespace with no network interfaces.

```json
{
  "version": 1,
  "network": "none"
}
```

Network isolation affects sandboxed commands and plugin `exec` calls. It also
disables HTTP access from Rhai plugins. It does not block network requests that
the Alate daemon makes for the model gateway.

## Policy file

The sandbox policy belongs to the local user, not to an agent workspace. For
an agent named `work`, its file is:

```text
~/.aphid/alate/.sandbox/work.json
```

An absent or empty policy file uses the strict default policy. The strict
policy enables Bubblewrap, gives write access only to the workspace, and keeps
host networking.

Use this policy to add explicit grants:

```json
{
  "version": 1,
  "enabled": true,
  "network": "none",
  "read_only": ["/home/user/reference"],
  "read_write": ["/home/user/output"],
  "host_environment": ["SSH_AUTH_SOCK"]
}
```

The policy can set `bubblewrap` to the absolute path of the `bwrap` program.
If it is not set, Alate searches `PATH`. Alate fails to start the agent when
Bubblewrap is not available or cannot create the required sandbox. This fail
closed behavior avoids an unprotected fallback.

Set `enabled` to `false` only when you intentionally want to run an agent
without a command sandbox. This can be useful on systems that do not support
Bubblewrap. Alate currently supports command sandboxing on Linux only.

## Environment

Alate clears the command environment before it starts the command. It adds a
small safe set of terminal and locale variables, then uses synthetic values
for `HOME`, `TMPDIR`, and XDG data directories.

Set literal command environment variables in the agent `alate.json` file:

```json
{
  "environment": {
    "RUST_BACKTRACE": "1",
    "SSH_AUTH_SOCK": "${SSH_AUTH_SOCK}"
  }
}
```

`"${NAME}"` copies a host environment value only when `NAME` is listed in the
policy `host_environment` array. It must be the complete value. Alate rejects
a missing or non-allowlisted value instead of silently using the host value.

Use `"$${NAME}"` when the command must receive the literal text `"${NAME}"`.
Alate does not expand embedded values or run recursive expansion.

## Limits

The sandbox is a command boundary, not a complete operating system security
profile. It does not add seccomp filters, CPU or memory limits, or a firewall
for the Alate daemon. Treat path grants and host environment allowlists as
security-sensitive configuration.

Paths are checked before daemon-side file operations. Do not change a granted
path to a symlink after Alate starts. Keep the policy file under user control.
