//! The in-game native hook.
//!
//! This is the library the game process loads (a proxy DLL on Windows,
//! `LD_PRELOAD` on Linux, a bundled dylib on macOS). Its job at milestone M0 is
//! only the skeleton around the real work:
//!
//! - run early, off the loader lock (a thread from `DllMain` on Windows, a
//!   [`ctor`](https://docs.rs/ctor) constructor on Unix);
//! - identify the running executable and find a matching build [`profile`];
//! - fail closed - install nothing - when no profile matches the build;
//! - connect to the agent's shared-memory link if it is present;
//! - log every step to a file under the per-user data directory.
//!
//! There are no game-specific targets yet: locating and detouring TPF3 functions
//! comes with the release-day profile (see `docs/DAY_ONE.md`). The pieces that
//! do that - [`tpf3mp_hookcore`] scanning/resolution and the detour engine - are
//! ready and tested; this crate wires them to the process.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use tpf3mp_hookcore::profile::{BuildIdentity, Profile, ProfileError};
use tpf3mp_ipc::{Link, Role};

mod platform;

/// The shared-memory link name the agent publishes. Both ends must agree.
pub const AGENT_IPC_NAME: &str = "tpf3mp.default";

/// Application name used for the per-user data directory.
const APP_DIR: &str = "TPF3-MP";

/// Runs the whole bootstrap sequence. Called once, on a thread that is not
/// holding the loader lock. Never panics across the FFI boundary: every step
/// logs its outcome and returns.
pub fn bootstrap() {
    let data_dir = data_dir();
    if let Some(dir) = &data_dir {
        let _ = fs::create_dir_all(dir);
    }
    let mut log = Logger::open(data_dir.as_deref());
    log.line("hook bootstrap starting");

    match resolve_build(&mut log, data_dir.as_deref()) {
        BuildOutcome::Matched { name, targets } => {
            log.line(&format!(
                "matched profile {name:?} ({targets} targets); target installation lands with the release-day profile"
            ));
        }
        BuildOutcome::FailedClosed(reason) => {
            log.line(&format!("multiplayer disabled (fail-closed): {reason}"));
        }
    }

    match Link::open(AGENT_IPC_NAME, Role::Hook) {
        Ok(link) => {
            link.heartbeat();
            log.line(&format!(
                "connected to agent IPC (abi {}, session {:#010x}, agent pid {})",
                link.abi_version(),
                link.session(),
                link.peer_pid()
            ));
        }
        Err(error) => log.line(&format!("agent IPC not present: {error}")),
    }

    log.line("hook bootstrap complete");
}

/// The result of trying to match the running build to a profile.
enum BuildOutcome {
    Matched { name: String, targets: usize },
    FailedClosed(String),
}

fn resolve_build(log: &mut Logger, data_dir: Option<&Path>) -> BuildOutcome {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return BuildOutcome::FailedClosed(format!("cannot find this executable: {error}"));
        }
    };
    let identity = match BuildIdentity::of_file(&exe) {
        Ok(identity) => identity,
        Err(error) => {
            return BuildOutcome::FailedClosed(format!("cannot hash {}: {error}", exe.display()));
        }
    };
    log.line(&format!(
        "executable {} sha256={} size={:?}",
        exe.display(),
        identity.sha256,
        identity.size
    ));

    let profiles_dir = data_dir.map(|dir| dir.join("profiles"));
    let profiles = profiles_dir
        .as_deref()
        .map(load_profiles)
        .unwrap_or_default();
    for loaded in &profiles {
        if let Err(error) = &loaded.profile {
            log.line(&format!(
                "ignoring unreadable profile {}: {error:?}",
                loaded.path.display()
            ));
        }
    }

    match select_profile(&profiles, &identity) {
        Some(profile) => BuildOutcome::Matched {
            name: profile.name.clone(),
            targets: profile.targets.len(),
        },
        None => BuildOutcome::FailedClosed(format!(
            "no profile in {:?} matches build {}",
            profiles_dir, identity.sha256
        )),
    }
}

/// Why a profile file could not be turned into a [`Profile`].
#[derive(Debug)]
pub enum LoadError {
    /// The file could not be read.
    Read(String),
    /// The file was read but is not a valid profile.
    Parse(ProfileError),
}

/// A profile file that was found, parsed or not.
pub struct LoadedProfile {
    pub path: PathBuf,
    pub profile: Result<Profile, LoadError>,
}

/// Reads and parses every `*.toml` in `dir`. Missing directory yields an empty
/// list; unreadable or malformed files are kept as `Err` so they can be logged.
pub fn load_profiles(dir: &Path) -> Vec<LoadedProfile> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let profile = match fs::read_to_string(&path) {
            Ok(text) => Profile::from_toml(&text).map_err(LoadError::Parse),
            Err(error) => Err(LoadError::Read(error.to_string())),
        };
        out.push(LoadedProfile { path, profile });
    }
    out
}

/// The first profile whose declared build identity matches the running build.
/// Returns `None` when none match, which is the fail-closed signal.
pub fn select_profile<'a>(
    profiles: &'a [LoadedProfile],
    identity: &BuildIdentity,
) -> Option<&'a Profile> {
    profiles
        .iter()
        .filter_map(|loaded| loaded.profile.as_ref().ok())
        .find(|profile| profile.verify_identity(identity).is_ok())
}

/// The per-user data directory for logs and profiles.
pub fn data_dir() -> Option<PathBuf> {
    data_dir_from(|key| std::env::var(key).ok())
}

/// Resolves the data directory from an environment getter, so the mapping can
/// be tested without touching the real environment.
fn data_dir_from(get: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        get("LOCALAPPDATA").map(|base| PathBuf::from(base).join(APP_DIR))
    }
    #[cfg(target_os = "macos")]
    {
        get("HOME").map(|home| {
            PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(APP_DIR)
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        get("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| get("HOME").map(|home| PathBuf::from(home).join(".local").join("share")))
            .map(|base| base.join(APP_DIR))
    }
}

/// A tiny append-only line logger. When no data directory is available it drops
/// messages rather than failing the hook.
struct Logger {
    file: Option<File>,
}

impl Logger {
    fn open(dir: Option<&Path>) -> Self {
        let file = dir.and_then(|dir| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("hook.log"))
                .ok()
        });
        Self { file }
    }

    fn line(&mut self, message: &str) {
        if let Some(file) = &mut self.file {
            let seconds = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let _ = writeln!(file, "[{seconds}] {message}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn profile_toml(name: &str, sha: &str) -> String {
        format!(
            r#"
name = "{name}"
[build]
sha256 = "{sha}"
[[target]]
name = "t"
signature = "40 53"
prologue = "40 53"
"#
        )
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir()
                .join(format!("tpf3mp-hook-{tag}-{}-{nanos}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn data_dir_maps_the_expected_environment_variable() {
        let mut env = HashMap::new();
        #[cfg(windows)]
        {
            env.insert("LOCALAPPDATA", r"C:\Users\x\AppData\Local".to_string());
        }
        #[cfg(target_os = "macos")]
        {
            env.insert("HOME", "/Users/x".to_string());
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            env.insert("HOME", "/home/x".to_string());
        }
        let dir = data_dir_from(|key| env.get(key).cloned()).expect("a base dir");
        assert!(dir.ends_with(APP_DIR), "{dir:?} should end with {APP_DIR}");
    }

    #[test]
    fn data_dir_is_none_without_the_environment() {
        assert!(data_dir_from(|_| None).is_none());
    }

    #[test]
    fn select_profile_matches_by_identity_and_fails_closed_otherwise() {
        let dir = TempDir::new("select");
        fs::write(dir.0.join("a.toml"), profile_toml("Build A", "aaaa")).unwrap();
        fs::write(dir.0.join("b.toml"), profile_toml("Build B", "bbbb")).unwrap();
        fs::write(dir.0.join("notes.txt"), "ignored").unwrap();
        let profiles = load_profiles(&dir.0);
        assert_eq!(profiles.len(), 2, "only .toml files are loaded");

        let matches_b = BuildIdentity {
            sha256: "bbbb".into(),
            size: None,
            pe_timestamp: None,
        };
        assert_eq!(
            select_profile(&profiles, &matches_b).map(|p| p.name.as_str()),
            Some("Build B")
        );

        let matches_none = BuildIdentity {
            sha256: "cccc".into(),
            size: None,
            pe_timestamp: None,
        };
        assert!(
            select_profile(&profiles, &matches_none).is_none(),
            "an unknown build matches no profile (fail-closed)"
        );
    }

    #[test]
    fn load_profiles_on_missing_directory_is_empty() {
        assert!(load_profiles(Path::new("/no/such/dir/tpf3mp")).is_empty());
    }
}
