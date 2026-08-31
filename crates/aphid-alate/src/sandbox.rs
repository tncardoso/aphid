//! The user-owned policy that confines an alate's commands.

use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::collections::BTreeSet;
#[cfg(target_os = "linux")]
use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command as StdCommand;
use std::sync::Arc;

use aphid_agent::exec::Launcher;
#[cfg(target_os = "linux")]
use aphid_agent::exec::Spec;
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use tokio::process::Command;

/// The policy format written by this build.
pub const VERSION: u32 = 1;

/// The network available to command children. Alate itself always retains its
/// provider and gateway connections.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    #[default]
    Host,
    None,
}

/// The part of configuration that the agent must not be able to edit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub version: u32,
    pub enabled: bool,
    pub bubblewrap: Option<PathBuf>,
    pub network: Network,
    pub read_only: Vec<PathBuf>,
    pub read_write: Vec<PathBuf>,
    /// Host variables that `alate.json` may reference as `${NAME}`.
    pub host_environment: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            version: VERSION,
            enabled: true,
            bubblewrap: None,
            network: Network::Host,
            read_only: Vec::new(),
            read_write: Vec::new(),
            host_environment: Vec::new(),
        }
    }
}

impl Policy {
    /// A missing policy is strict by default.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let policy: Self =
            serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        if policy.version > VERSION {
            return Err(format!(
                "{}: version {} was written by a newer aphid; this one understands {VERSION}",
                path.display(),
                policy.version
            ));
        }
        Ok(policy)
    }
}

/// Build the launcher Alate gives to its shared process registry.
pub fn prepare(
    policy: &Policy,
    workspace: &aphid_code::tools::Workspace,
    literals: &BTreeMap<String, String>,
) -> Result<Option<Arc<dyn Launcher>>, String> {
    if !policy.enabled {
        return Ok(None);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (workspace, literals);
        Err("the Bubblewrap sandbox needs Linux; set enabled to false in the sandbox policy to run without it".to_owned())
    }
    #[cfg(target_os = "linux")]
    {
        let workspace = canonical_root(workspace.root())?;
        let read_only = roots(&policy.read_only, &workspace)?;
        let read_write = roots(&policy.read_write, &workspace)?;
        for left in &read_only {
            for right in &read_write {
                if overlaps(left, right) {
                    return Err(format!(
                        "sandbox grants overlap: {} and {}",
                        left.display(),
                        right.display()
                    ));
                }
            }
        }
        let bubblewrap = bubblewrap(policy.bubblewrap.as_deref(), &workspace)?;
        let environment = environment(policy, literals)?;
        // Exercise the kernel features now, before the daemon advertises a
        // socket that will later fail every command.
        let status = StdCommand::new(&bubblewrap)
            .args([
                "--ro-bind",
                "/",
                "/",
                "--proc",
                "/proc",
                "--dev",
                "/dev",
                "--",
                "/usr/bin/true",
            ])
            .status()
            .map_err(|error| {
                format!(
                    "could not start Bubblewrap at {}: {error}",
                    bubblewrap.display()
                )
            })?;
        if !status.success() {
            return Err(format!(
                "Bubblewrap at {} could not create a sandbox",
                bubblewrap.display()
            ));
        }
        Ok(Some(Arc::new(Bubblewrap {
            bubblewrap,
            workspace,
            read_only,
            read_write,
            network: policy.network,
            environment,
        })))
    }
}

#[cfg(target_os = "linux")]
struct Bubblewrap {
    bubblewrap: PathBuf,
    workspace: PathBuf,
    read_only: Vec<PathBuf>,
    read_write: Vec<PathBuf>,
    network: Network,
    environment: BTreeMap<String, OsString>,
}

#[cfg(target_os = "linux")]
impl Launcher for Bubblewrap {
    fn command(&self, spec: &Spec) -> Command {
        let mut command = Command::new(&self.bubblewrap);
        command.current_dir(&self.workspace);
        command
            .arg("--die-with-parent")
            .arg("--new-session")
            .arg("--unshare-user")
            .arg("--unshare-pid")
            .arg("--unshare-ipc")
            .arg("--unshare-uts")
            .arg("--unshare-cgroup-try")
            .arg("--disable-userns")
            .arg("--ro-bind")
            .arg("/usr")
            .arg("/usr");
        for root in ["/bin", "/sbin", "/lib", "/lib64", "/etc"] {
            command.arg("--ro-bind-try").arg(root).arg(root);
        }
        command
            .arg("--proc")
            .arg("/proc")
            .arg("--dev")
            .arg("/dev")
            .arg("--tmpfs")
            .arg("/tmp")
            .arg("--bind")
            .arg(&self.workspace)
            .arg(&self.workspace);
        for root in &self.read_only {
            command.arg("--ro-bind").arg(root).arg(root);
        }
        for root in &self.read_write {
            command.arg("--bind").arg(root).arg(root);
        }
        if self.network == Network::None {
            command.arg("--unshare-net");
        }
        command.arg("--clearenv");
        for (name, value) in &self.environment {
            command.arg("--setenv").arg(name).arg(value);
        }
        command
            .arg("--chdir")
            .arg(spec.cwd.as_deref().unwrap_or(&self.workspace))
            .arg("--")
            .arg("bash")
            .arg("-c")
            .arg(&spec.command);
        command
    }
}

#[cfg(target_os = "linux")]
fn canonical_root(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!("sandbox path {} is not absolute", path.display()));
    }
    path.canonicalize()
        .map_err(|error| format!("could not resolve sandbox path {}: {error}", path.display()))
}

#[cfg(target_os = "linux")]
fn roots(paths: &[PathBuf], workspace: &Path) -> Result<Vec<PathBuf>, String> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for path in paths {
        let path = canonical_root(path)?;
        if overlaps(&path, workspace) {
            return Err(format!(
                "sandbox grant {} overlaps the workspace",
                path.display()
            ));
        }
        if roots.iter().any(|known| overlaps(known, &path)) {
            return Err(format!(
                "sandbox grant {} overlaps another grant",
                path.display()
            ));
        }
        roots.push(path);
    }
    Ok(roots)
}

#[cfg(target_os = "linux")]
fn overlaps(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

#[cfg(target_os = "linux")]
fn bubblewrap(configured: Option<&Path>, workspace: &Path) -> Result<PathBuf, String> {
    let path = match configured {
        Some(path) => canonical_root(path)?,
        None => std::env::var_os("PATH")
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .map(|dir| dir.join("bwrap"))
            .find(|path| path.is_file())
            .ok_or("Bubblewrap is not installed; install bwrap or set enabled to false in the sandbox policy")?
            .canonicalize()
            .map_err(|error| format!("could not resolve Bubblewrap: {error}"))?,
    };
    if path.starts_with(workspace) {
        return Err(format!(
            "Bubblewrap at {} is inside the writable workspace",
            path.display()
        ));
    }
    Ok(path)
}

#[cfg(target_os = "linux")]
fn environment(
    policy: &Policy,
    literals: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, OsString>, String> {
    let allowed: BTreeSet<&str> = policy.host_environment.iter().map(String::as_str).collect();
    let mut environment = BTreeMap::new();
    for (name, value) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name == "PATH"
            || name == "TERM"
            || name == "COLORTERM"
            || name == "NO_COLOR"
            || name == "LANG"
            || name.starts_with("LC_")
        {
            environment.insert(name.into_owned(), value);
        }
    }
    environment.insert("HOME".to_owned(), OsString::from("/tmp/home"));
    environment.insert("TMPDIR".to_owned(), OsString::from("/tmp"));
    environment.insert("XDG_CACHE_HOME".to_owned(), OsString::from("/tmp/cache"));
    environment.insert("XDG_CONFIG_HOME".to_owned(), OsString::from("/tmp/config"));
    environment.insert("XDG_DATA_HOME".to_owned(), OsString::from("/tmp/data"));
    for (target, value) in literals {
        valid_name(target)?;
        let value = if let Some(name) = host_reference(value) {
            if !allowed.contains(name) {
                return Err(format!(
                    "environment variable {target} references host variable {name}, which is not allowed by the sandbox policy"
                ));
            }
            std::env::var_os(name)
                .ok_or_else(|| format!("host environment variable {name} is not set"))?
        } else if value.starts_with("$${") && value.ends_with('}') {
            OsString::from(&value[1..])
        } else {
            OsString::from(value)
        };
        environment.insert(target.clone(), value);
    }
    Ok(environment)
}

#[cfg(target_os = "linux")]
fn host_reference(value: &str) -> Option<&str> {
    value
        .strip_prefix("${")?
        .strip_suffix('}')
        .filter(|name| valid_name(name).is_ok())
}

#[cfg(target_os = "linux")]
fn valid_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first == '_' || first.is_ascii_alphabetic() => {}
        _ => return Err(format!("{name:?} is not an environment variable name")),
    }
    if chars.all(|character| character == '_' || character.is_ascii_alphanumeric()) {
        Ok(())
    } else {
        Err(format!("{name:?} is not an environment variable name"))
    }
}
