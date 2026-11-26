//! On-disk locations, and reading the settings file and the environment. The `teams-client` crate takes all of this as
//! explicit arguments; this is the one place that reads the environment and picks paths. How settings combine
//! (flag, environment, file, default) is in `settings.rs`.

use crate::cache::Cache;
use crate::clipboard::{self, Clipboard, Layered, Mode as ClipboardMode};
use crate::gfx;
use crate::gfx::detect::TermEnv;
use crate::settings::{self, CredentialsMode, FileValues, Flag, Settings};
use crate::vault::{self, EncryptedStore, KeyError, KeyProvider, OsKeys};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use teams_client::graph::{DEFAULT_CLIENT_ID, DEFAULT_TENANT, GraphConfig};
use teams_client::teams::BrowserOptions;
use teams_client::{CredentialStore, FileStore, HeaderLog, ShapeRecorder};

/// The Graph app registration to use: the settings `client_id` and `tenant`, else the defaults (Microsoft's own public
/// client, which a tenant may block; register your own and give its id).
pub fn graph_config(client_id: Option<&str>, tenant: Option<&str>) -> GraphConfig {
    GraphConfig::new(
        client_id.unwrap_or(DEFAULT_CLIENT_ID),
        tenant.unwrap_or(DEFAULT_TENANT),
    )
}

/// `%APPDATA%\vtteams` on Windows, `$XDG_CONFIG_HOME/vtteams` or `~/.config/vtteams` elsewhere. There is deliberately no
/// fallback to the current directory: credentials must not end up wherever the program happened to be started.
pub fn config_dir() -> Result<PathBuf, String> {
    config_dir_from(|var| std::env::var_os(var))
}

/// [`config_dir`] over any environment. A variable that is empty or not an absolute path is treated as unset: using it
/// would put credentials in whatever directory the program happened to be started from.
fn config_dir_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    let absolute = |var: &str| {
        env(var)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    // `APPDATA` is a Windows variable; elsewhere a stray one (inherited through WSL, say) must not move the settings.
    let base = absolute("APPDATA")
        .filter(|_| cfg!(windows))
        .or_else(|| absolute("XDG_CONFIG_HOME"))
        .or_else(|| {
            absolute("HOME")
                .or_else(|| absolute("USERPROFILE"))
                .map(|home| home.join(".config"))
        })
        .ok_or("cannot find a place for settings: none of APPDATA, XDG_CONFIG_HOME, HOME or USERPROFILE is set to an absolute path")?;
    Ok(base.join("vtteams"))
}

/// Whether vtteams runs somewhere other than in front of the person: in an SSH session, or on Linux with no display. The
/// system clipboard there is not the one they copy to and paste from.
pub fn runs_remotely() -> bool {
    runs_remotely_from(|var| std::env::var_os(var), cfg!(target_os = "linux"))
}

/// [`runs_remotely`] over any environment. A variable that is set but empty counts as unset.
fn runs_remotely_from(env: impl Fn(&str) -> Option<std::ffi::OsString>, linux: bool) -> bool {
    let set = |var: &str| env(var).is_some_and(|v| !v.is_empty());
    set("SSH_CONNECTION")
        || set("SSH_CLIENT")
        || set("SSH_TTY")
        || (linux && !set("DISPLAY") && !set("WAYLAND_DISPLAY"))
}

/// What the environment says about the terminal (`TERM`, `TERM_PROGRAM`, `WT_SESSION`, ...): a hint at which graphics
/// protocol it has, which the terminal's own answers then confirm or overrule (see `gfx::detect`).
pub fn terminal_env() -> TermEnv {
    TermEnv::from_lookup(|var| std::env::var(var).ok())
}

/// The variables [`terminal_env`] reads, with what they hold, for `--terminal-report`.
pub fn terminal_variables() -> Vec<(&'static str, Option<String>)> {
    gfx::report::VARIABLES
        .iter()
        .map(|name| (*name, std::env::var(name).ok()))
        .collect()
}

/// The clipboard for the `clipboard` setting: the system one, the terminal's (OSC 52), or the system one with the
/// terminal's behind it (see `clipboard::Layered`).
pub fn clipboard(mode: ClipboardMode) -> Arc<dyn Clipboard> {
    Arc::new(Layered::new(
        mode,
        runs_remotely(),
        Arc::new(clipboard::System::new()),
        Arc::new(clipboard::Terminal::new(std::io::stdout())),
    ))
}

/// Where the cache goes: the per-user cache directory, which is not copied to other computers by a roaming profile and
/// is not backed up with the settings. `%LOCALAPPDATA%\vtteams\cache` on Windows, `~/Library/Caches/vtteams` on macOS,
/// `$XDG_CACHE_HOME/vtteams` or `~/.cache/vtteams` elsewhere. Like [`config_dir`], never relative to the current
/// directory.
pub fn cache_dir() -> Result<PathBuf, String> {
    let os = if cfg!(windows) {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Mac
    } else {
        Os::Other
    };
    cache_dir_from(|var| std::env::var_os(var), os)
}

/// The operating systems whose cache location differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Os {
    Windows,
    Mac,
    Other,
}

/// [`cache_dir`] over any environment and system. A variable that is empty or not an absolute path is treated as unset.
fn cache_dir_from(
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
    os: Os,
) -> Result<PathBuf, String> {
    let absolute = |var: &str| {
        env(var)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
    };
    let base = match os {
        Os::Windows => absolute("LOCALAPPDATA")
            .or_else(|| absolute("USERPROFILE").map(|home| home.join("AppData").join("Local")))
            .map(|base| base.join("vtteams").join("cache")),
        Os::Mac => absolute("HOME").map(|home| home.join("Library").join("Caches").join("vtteams")),
        Os::Other => absolute("XDG_CACHE_HOME")
            .or_else(|| absolute("HOME").map(|home| home.join(".cache")))
            .map(|base| base.join("vtteams")),
    };
    base.ok_or_else(|| {
        "cannot find a place for the cache: no cache or home directory is set to an absolute path"
            .to_string()
    })
}

/// The key that seals the stored sign-in and the cache, as far as it can be had.
#[derive(Clone)]
pub struct Vault {
    pub mode: CredentialsMode,
    /// The key in the operating system's credential store. `None` only with `credentials = file`, when that store cannot
    /// be used: nothing is then encrypted, and there is no cache.
    pub keys: Option<Arc<dyn KeyProvider>>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("mode", &self.mode)
            .field("keys", &self.keys.is_some())
            .finish()
    }
}

impl Vault {
    /// A vault with no key store, for the demo and the one-frame dump of nothing stored.
    pub fn none(mode: CredentialsMode) -> Self {
        Self { mode, keys: None }
    }
}

/// Open the operating system's credential store for the `credentials` setting. With `auto` or `keychain` the key is
/// made (or found) now, so a store that cannot be used stops start-up before anything is signed in or written; with
/// `file` it is only connected to, quietly, for the cache, and the program goes on without it if it is not there.
pub fn open_vault(mode: CredentialsMode) -> Result<Vault, String> {
    open_vault_with(mode, || {
        OsKeys::open().map(|keys| Arc::new(keys) as Arc<dyn KeyProvider>)
    })
}

/// [`open_vault`] over any key store.
pub fn open_vault_with(
    mode: CredentialsMode,
    open: impl FnOnce() -> Result<Arc<dyn KeyProvider>, KeyError>,
) -> Result<Vault, String> {
    let ready = open().and_then(|keys| {
        if mode.encrypts() {
            keys.get_or_create()?;
        }
        Ok(keys)
    });
    match ready {
        Ok(keys) => Ok(Vault {
            mode,
            keys: Some(keys),
        }),
        Err(e) if mode.encrypts() => Err(unusable_store(&e)),
        Err(e) => {
            log::info!("no credential store for the cache ({e}); the cache is off");
            Ok(Vault { mode, keys: None })
        }
    }
}

/// What start-up says when the credential store cannot be used and the sign-in is to be encrypted.
fn unusable_store(e: &KeyError) -> String {
    format!(
        "{e}. vtteams keeps your sign-in encrypted with a key held in that store, and will not write it as readable \
         text unless you say so. To keep it in a plain file instead (readable by anything running as you, and by \
         anyone who gets a copy of the settings folder), set credentials = \"file\" in config.toml, or run with \
         --credentials file, or set VTTEAMS_CREDENTIALS=file"
    )
}

/// Which service a stored sign-in is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Teams,
    Graph,
}

impl Service {
    /// What the sealed file and the cache are bound to, and the cache file's name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Teams => "teams",
            Self::Graph => "graph",
        }
    }

    /// The file the sign-in is kept in as readable text, with `credentials = file`.
    fn plain_name(self) -> &'static str {
        match self {
            Self::Teams => "teams-session.json",
            Self::Graph => "token.json",
        }
    }

    /// The file it is kept in now, encrypted.
    fn sealed_name(self) -> &'static str {
        match self {
            Self::Teams => "teams-session.enc",
            Self::Graph => "token.enc",
        }
    }
}

/// A stored sign-in that is ready to use, and what the person should be told about it.
pub struct Credentials {
    pub store: Arc<dyn CredentialStore>,
    /// Where it is kept, for messages.
    pub path: PathBuf,
    /// Whether it is encrypted.
    pub encrypted: bool,
    /// Things worth saying once at start-up (a readable sign-in left on disk and not used).
    pub notes: Vec<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("path", &self.path)
            .field("encrypted", &self.encrypted)
            .finish_non_exhaustive()
    }
}

/// The stored sign-in for `service` under the settings directory: the encrypted file, or with `credentials = file` the
/// plain one. A plain file found while encrypting (left from a time with `credentials = file`) is not used, and `notes`
/// says it is there, so a readable sign-in is never left on disk without the person knowing.
pub fn credentials(service: Service, vault: &Vault) -> Result<Credentials, String> {
    Ok(open_credentials(&config_dir()?, service, vault))
}

/// [`credentials`] in any directory.
pub fn open_credentials(dir: &Path, service: Service, vault: &Vault) -> Credentials {
    let plain = dir.join(service.plain_name());
    let in_plain_file = |notes: Vec<String>| Credentials {
        store: Arc::new(FileStore::new(&plain)),
        path: plain.clone(),
        encrypted: false,
        notes,
    };
    let Some(keys) = vault.keys.clone().filter(|_| vault.mode.encrypts()) else {
        log::warn!(
            "credentials = file: the sign-in is kept as readable text in {}",
            plain.display()
        );
        return in_plain_file(Vec::new());
    };
    let sealed = EncryptedStore::new(dir.join(service.sealed_name()), keys, service.label());
    let note = plain.exists().then(|| {
        log::warn!("a readable sign-in file is on disk and not used while credentials = keychain");
        format!(
            "{} holds a sign-in as readable text (from credentials = file); it is not used while the sign-in is \
             kept encrypted. Delete it, or run vtteams logout, which deletes it too",
            plain.display()
        )
    });
    Credentials {
        path: sealed.path().to_path_buf(),
        store: Arc::new(sealed),
        encrypted: true,
        notes: note.into_iter().collect(),
    }
}

/// The cache for `account` on `service`, if one can be kept: the key store works and the account is known. Never
/// without a key: the cache is not written as readable text.
pub fn open_cache(
    vault: &Vault,
    dir: &Path,
    service: Service,
    account: Option<&str>,
) -> Option<Cache> {
    let keys = vault.keys.clone()?;
    let account = account.filter(|a| !a.is_empty())?;
    Some(Cache::new(keys, dir, service.label(), account))
}

/// Delete whatever the cache holds (`cache = off`). Best effort.
pub fn remove_cache(dir: &Path) {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => log::info!("cache = off: the cache was deleted"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => log::warn!("cache = off: cannot delete the cache: {e}"),
    }
}

/// What `vtteams logout` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Forgotten {
    /// Something was there and is gone.
    pub removed: bool,
    /// Everything that could not be forgotten, each in words.
    pub failures: Vec<String>,
}

/// Forget everything stored about the sign-in: both services' credentials (plain and encrypted), the browser profile
/// with its single-sign-on cookies, the cache, and the key in the credential store. Carries on whatever fails on the
/// way, and reports every failure.
pub fn forget_all(
    config: Result<&Path, String>,
    cache: Result<&Path, String>,
    keys: Result<Arc<dyn KeyProvider>, String>,
) -> Forgotten {
    let mut done = Forgotten::default();
    let mut had_sealed = false;
    let config = match config {
        Ok(dir) => dir,
        Err(e) => {
            done.failures.push(e);
            // Nothing below can be found without it, but the cache and the key can still be forgotten.
            Path::new("")
        }
    };
    for service in [Service::Graph, Service::Teams] {
        if config.as_os_str().is_empty() {
            break;
        }
        let plain = config.join(service.plain_name());
        if plain.exists() {
            match vault::overwrite_and_remove(&plain) {
                Ok(()) => done.removed = true,
                Err(e) => done
                    .failures
                    .push(format!("cannot remove {}: {e}", plain.display())),
            }
        }
        match FileStore::new(config.join(service.sealed_name())).clear() {
            Ok(was_there) => {
                had_sealed |= was_there;
                done.removed |= was_there;
            }
            Err(e) => {
                had_sealed = true;
                done.failures.push(e.to_string());
            }
        }
    }
    if !config.as_os_str().is_empty() {
        match std::fs::remove_dir_all(config.join("browser-profile")) {
            Ok(()) => done.removed = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => done
                .failures
                .push(format!("cannot remove the browser profile: {e}")),
        }
    }
    match cache {
        Ok(dir) => match std::fs::remove_dir_all(dir) {
            Ok(()) => {
                had_sealed = true;
                done.removed = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                had_sealed = true;
                done.failures.push(format!("cannot remove the cache: {e}"));
            }
        },
        Err(e) => done.failures.push(e),
    }
    match keys {
        Ok(keys) => match keys.delete() {
            Ok(was_there) => done.removed |= was_there,
            Err(e) => done.failures.push(format!(
                "cannot delete the key from the credential store: {e}"
            )),
        },
        // A computer with no credential store (a plain-file setup) has no key to forget; only when there was something
        // sealed is it a loss that the key could not be reached.
        Err(why) if had_sealed => done.failures.push(format!(
            "cannot reach the credential store to delete the key: {why}"
        )),
        Err(why) => log::debug!("logout: no credential store to forget the key in ({why})"),
    }
    done
}

/// The browser used for the Teams web sign-in: the setting `browser`, else whichever is installed, with its private
/// profile (it holds the Microsoft single-sign-on cookies, so `logout` deletes it too).
pub fn browser_options(browser: Option<&str>) -> Result<BrowserOptions, String> {
    let options = BrowserOptions::new(browser_profile_dir()?);
    Ok(match browser {
        Some(path) => options.with_executable(path),
        None => options,
    })
}

pub fn browser_profile_dir() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("browser-profile"))
}

/// Where images opened in the browser are saved.
pub fn images_dir() -> Result<PathBuf, String> {
    Ok(config_dir()?.join("images"))
}

/// Where the settings file is: `--config`, else `VTTEAMS_CONFIG`, else `config.toml` in the settings directory. The flag
/// is true when the path was asked for, in which case a missing file is an error.
pub fn settings_path(flag: Option<String>) -> Result<(PathBuf, bool), String> {
    settings_path_from(flag, std::env::var("VTTEAMS_CONFIG").ok(), config_dir)
}

fn settings_path_from(
    flag: Option<String>,
    env: Option<String>,
    dir: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<(PathBuf, bool), String> {
    // A blank value counts as not given, for the flag and the variable each: an empty `--config ""` must not hide a
    // `VTTEAMS_CONFIG` that is set.
    let nonblank = |p: &String| !p.trim().is_empty();
    let asked = flag.filter(nonblank).or(env.filter(nonblank));
    match asked {
        Some(path) => Ok((PathBuf::from(path), true)),
        None => Ok((dir()?.join("config.toml"), false)),
    }
}

/// The settings file is a few lines; anything bigger is not one.
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;

/// The text of the settings file. Only a regular file is read, and never more than [`MAX_SETTINGS_BYTES`] of it,
/// whatever its reported size (a device or a pipe reports none, and a file can grow while it is read).
fn read_settings_file(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut text = String::new();
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_SETTINGS_BYTES {
        return Err(std::io::Error::other("too large to be a settings file"));
    }
    Ok(text)
}

/// What was found and resolved at startup.
#[derive(Debug)]
pub struct Loaded {
    pub settings: Settings,
    pub path: PathBuf,
    /// Whether the settings file exists.
    pub found: bool,
}

/// Read the settings file (if there is one) and the environment and combine them with the command-line `flags`.
pub fn load(flags: &[Flag], config_flag: Option<String>) -> Result<Loaded, String> {
    let (path, asked) = settings_path(config_flag)?;
    load_from(&path, asked, flags, &|name| std::env::var(name).ok())
}

fn load_from(
    path: &Path,
    asked: bool,
    flags: &[Flag],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Loaded, String> {
    let text = match read_settings_file(path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !asked => None,
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let file = match &text {
        Some(text) => FileValues::parse(text).map_err(|e| format!("{}: {e}", path.display()))?,
        None => FileValues::default(),
    };
    let settings = settings::resolve(flags, env, &file, &path.display().to_string())?;
    Ok(Loaded {
        settings,
        path: path.to_path_buf(),
        found: text.is_some(),
    })
}

/// Write the commented starter settings file to `path`, creating its directory. Never replaces a file.
pub fn write_starter(path: &Path) -> Result<(), String> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        teams_client::create_private_dir(dir).map_err(|e| e.to_string())?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                format!(
                    "{} already exists; edit it, or delete it to start over",
                    path.display()
                )
            } else {
                format!("cannot create {}: {e}", path.display())
            }
        })?;
    file.write_all(settings::sample().as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The recorder behind `--record-shapes <dir>` / `VTTEAMS_RECORD_SHAPES`, if one was asked for.
pub fn shape_recorder(flag: Option<String>) -> Result<Option<Arc<ShapeRecorder>>, String> {
    let nonblank = |d: &String| !d.trim().is_empty();
    let Some(dir) = flag
        .filter(nonblank)
        .or_else(|| std::env::var("VTTEAMS_RECORD_SHAPES").ok().filter(nonblank))
    else {
        return Ok(None);
    };
    let recorder = ShapeRecorder::new(&dir).map_err(|e| e.to_string())?;
    Ok(Some(Arc::new(recorder)))
}

/// Which response headers to write to the log: the selector behind `--log-headers <names>` / `VTTEAMS_LOG_HEADERS`, if
/// one was asked for (the flag first; a blank value counts as none). A selector that cannot be used stops start-up,
/// naming where it came from.
pub fn header_log(flag: Option<String>) -> Result<Option<HeaderLog>, String> {
    header_log_from(flag, |var| std::env::var(var).ok())
}

/// [`header_log`] over any environment.
fn header_log_from(
    flag: Option<String>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<Option<HeaderLog>, String> {
    let nonblank = |s: &String| !s.trim().is_empty();
    let (selector, source) = match flag.filter(nonblank) {
        Some(selector) => (selector, "--log-headers"),
        None => match env("VTTEAMS_LOG_HEADERS").filter(nonblank) {
            Some(selector) => (selector, "VTTEAMS_LOG_HEADERS"),
            None => return Ok(None),
        },
    };
    HeaderLog::parse(&selector)
        .map(Some)
        .map_err(|e| format!("{source}: {e}"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_header_selector_comes_from_the_flag_then_the_variable_and_a_blank_one_counts_as_none() {
        let env = |value: Option<&'static str>| {
            move |var: &str| {
                assert_eq!(var, "VTTEAMS_LOG_HEADERS");
                value.map(String::from)
            }
        };
        let names = |l: Option<teams_client::HeaderLog>| format!("{:?}", l.unwrap());
        // Nothing asked for, or only blanks: no logging of headers.
        assert!(super::header_log_from(None, env(None)).unwrap().is_none());
        assert!(
            super::header_log_from(Some("  ".into()), env(Some("")))
                .unwrap()
                .is_none()
        );
        // The variable is used when there is no flag, and the flag wins over it.
        let from_env = names(super::header_log_from(None, env(Some("connection"))).unwrap());
        assert!(from_env.contains("connection"), "{from_env}");
        let from_flag = names(
            super::header_log_from(Some("keep-alive".into()), env(Some("connection"))).unwrap(),
        );
        assert!(
            from_flag.contains("keep-alive") && !from_flag.contains("connection"),
            "{from_flag}"
        );
        // A blank flag falls through to the variable, as `--record-shapes ""` does.
        assert!(
            super::header_log_from(Some(String::new()), env(Some("via")))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn a_header_selector_that_cannot_be_used_stops_start_up_naming_where_it_came_from() {
        let none = |_: &str| None;
        for bad in ["x y", "!set-cookie", "a:b"] {
            let flag = super::header_log_from(Some(bad.into()), none).unwrap_err();
            assert!(flag.starts_with("--log-headers: "), "{flag}");
            let var = super::header_log_from(None, |_| Some(bad.to_string())).unwrap_err();
            assert!(var.starts_with("VTTEAMS_LOG_HEADERS: "), "{var}");
        }
    }

    #[test]
    fn a_remote_session_is_told_by_ssh_variables_or_a_missing_display() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |var: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == var)
                    .map(|(_, v)| std::ffi::OsString::from(*v))
            }
        };
        let remote = super::runs_remotely_from;
        assert!(
            !remote(env(&[]), false),
            "Windows and macOS have no DISPLAY to look for"
        );
        assert!(remote(
            env(&[("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22")]),
            false
        ));
        assert!(remote(env(&[("SSH_TTY", "/dev/pts/3")]), false));
        assert!(!remote(env(&[("SSH_TTY", "")]), false), "empty is unset");
        assert!(!remote(env(&[("DISPLAY", ":0")]), true));
        assert!(!remote(env(&[("WAYLAND_DISPLAY", "wayland-0")]), true));
        assert!(remote(env(&[]), true), "Linux with no display of its own");
        assert!(remote(
            env(&[("DISPLAY", ":0"), ("SSH_CONNECTION", "x")]),
            true
        ));
    }

    #[test]
    fn only_a_regular_file_of_sensible_size_is_read_as_settings() {
        let dir = crate::testdir::TestDir::new("settings-read");
        let file = dir.join("config.toml");
        std::fs::write(&file, "keymap = \"vim\"\n").unwrap();
        assert!(read_settings_file(&file).unwrap().contains("vim"));
        std::fs::write(&file, "#".repeat(MAX_SETTINGS_BYTES as usize + 1)).unwrap();
        assert!(read_settings_file(&file).is_err());
        assert!(
            read_settings_file(&dir).is_err(),
            "a directory is not a settings file"
        );
    }
    use super::*;
    use std::ffi::OsString;

    fn dir(vars: &[(&str, &str)]) -> Result<PathBuf, String> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(v)))
            .collect();
        config_dir_from(|name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
    }

    fn temp(name: &str) -> crate::testdir::TestDir {
        crate::testdir::TestDir::new(&format!("config-{name}"))
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn a_missing_settings_file_is_fine_unless_it_was_asked_for() {
        let dir = temp("missing");
        let path = dir.join("config.toml");
        let loaded = load_from(&path, false, &[], &no_env).unwrap();
        assert!(!loaded.found);
        assert_eq!(loaded.settings.keymap, crate::editor::Keymap::Standard);
        let e = load_from(&path, true, &[], &no_env).unwrap_err();
        assert!(
            e.contains("cannot read") && e.contains("config.toml"),
            "{e}"
        );
    }

    #[test]
    fn the_settings_file_is_read_and_flags_and_the_environment_override_it() {
        let dir = temp("read");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "keymap = \"vim\"\nimages = \"blocks\"\nmax_chats = 25\n",
        )
        .unwrap();
        let loaded = load_from(&path, false, &[], &no_env).unwrap();
        assert!(loaded.found);
        assert_eq!(loaded.settings.keymap, crate::editor::Keymap::Vim);
        assert_eq!(loaded.settings.max_chats, Some(25));
        let env = |name: &str| (name == "VTTEAMS_KEYMAP").then(|| "standard".to_string());
        let loaded = load_from(&path, false, &[], &env).unwrap();
        assert_eq!(loaded.settings.keymap, crate::editor::Keymap::Standard);
    }

    #[test]
    fn a_broken_settings_file_stops_startup_naming_the_file_and_line() {
        let dir = temp("broken");
        let path = dir.join("config.toml");
        std::fs::write(&path, "keymap = \"vim\"\nkeymapp = \"x\"\n").unwrap();
        let e = load_from(&path, false, &[], &no_env).unwrap_err();
        assert!(
            e.contains("config.toml") && e.contains("keymapp") && e.contains("line 2"),
            "{e}"
        );
        std::fs::write(&path, "keymap = \"emacs\"\n").unwrap();
        let e = load_from(&path, false, &[], &no_env).unwrap_err();
        assert!(e.starts_with("bad keymap 'emacs' in "), "{e}");
        std::fs::write(&path, "x".repeat(70_000)).unwrap();
        assert!(
            load_from(&path, false, &[], &no_env)
                .unwrap_err()
                .contains("too large")
        );
    }

    #[test]
    fn the_starter_file_is_written_once_and_never_replaced() {
        let dir = temp("starter");
        let path = dir.join("nested").join("config.toml");
        write_starter(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keymap = ") && text.contains("VTTEAMS_KEYMAP"));
        assert!(
            load_from(&path, true, &[], &no_env).unwrap().found,
            "and it loads"
        );
        std::fs::write(&path, "keymap = \"standard\"\n").unwrap();
        let e = write_starter(&path).unwrap_err();
        assert!(e.contains("already exists"), "{e}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "keymap = \"standard\"\n"
        );
    }

    #[test]
    fn the_settings_file_sits_in_the_settings_directory_unless_a_path_is_given() {
        let dir = || Ok(PathBuf::from(ABS).join("vtteams"));
        let s = |v: &str| Some(v.to_string());
        let (path, asked) = settings_path_from(s("/tmp/mine.toml"), s("/other"), dir).unwrap();
        assert_eq!(
            (path, asked),
            (PathBuf::from("/tmp/mine.toml"), true),
            "the flag wins"
        );
        let (path, asked) = settings_path_from(None, s("/other.toml"), dir).unwrap();
        assert_eq!(
            (path, asked),
            (PathBuf::from("/other.toml"), true),
            "then VTTEAMS_CONFIG"
        );
        let (path, asked) = settings_path_from(s("  "), s(""), dir).unwrap();
        assert!(!asked, "blank means not given");
        assert_eq!(path, PathBuf::from(ABS).join("vtteams").join("config.toml"));
        // A blank flag does not hide a variable that is set.
        let (path, asked) = settings_path_from(s(""), s("/other.toml"), dir).unwrap();
        assert_eq!((path, asked), (PathBuf::from("/other.toml"), true));
    }

    #[cfg(unix)]
    const ABS: &str = "/home/ada";
    #[cfg(windows)]
    const ABS: &str = r"C:\Users\ada";

    #[test]
    fn the_first_absolute_location_wins_in_a_fixed_order() {
        let appdata = format!("{ABS}/roaming");
        let xdg = format!("{ABS}/xdg");
        // `APPDATA` is the Windows location; on other systems a stray one is ignored.
        let first = if cfg!(windows) { &appdata } else { &xdg };
        assert_eq!(
            dir(&[
                ("APPDATA", &appdata),
                ("XDG_CONFIG_HOME", &xdg),
                ("HOME", ABS)
            ])
            .unwrap(),
            PathBuf::from(first).join("vtteams")
        );
        assert_eq!(
            dir(&[("XDG_CONFIG_HOME", &xdg), ("HOME", ABS)]).unwrap(),
            PathBuf::from(&xdg).join("vtteams")
        );
        assert_eq!(
            dir(&[("HOME", ABS)]).unwrap(),
            PathBuf::from(ABS).join(".config").join("vtteams")
        );
        assert_eq!(
            dir(&[("USERPROFILE", ABS)]).unwrap(),
            PathBuf::from(ABS).join(".config").join("vtteams")
        );
    }

    #[test]
    fn empty_and_relative_values_never_put_credentials_in_the_current_directory() {
        let xdg = format!("{ABS}/xdg");
        // Skipped, not used: the next absolute one is.
        assert_eq!(
            dir(&[("APPDATA", ""), ("XDG_CONFIG_HOME", &xdg)]).unwrap(),
            PathBuf::from(&xdg).join("vtteams")
        );
        assert_eq!(
            dir(&[
                ("APPDATA", "relative/dir"),
                ("XDG_CONFIG_HOME", "."),
                ("HOME", ABS)
            ])
            .unwrap(),
            PathBuf::from(ABS).join(".config").join("vtteams")
        );
        // With nothing usable there is an error, never a relative path.
        assert!(dir(&[("APPDATA", ""), ("HOME", "rel")]).is_err());
        assert!(dir(&[]).is_err());
    }

    fn cache_dir_for(os: Os, vars: &[(&str, &str)]) -> Result<PathBuf, String> {
        let vars: Vec<(String, OsString)> = vars
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(v)))
            .collect();
        cache_dir_from(
            |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()),
            os,
        )
    }

    #[test]
    fn the_cache_goes_in_the_per_user_cache_directory_of_each_system() {
        let local = format!("{ABS}/local");
        let xdg = format!("{ABS}/xdg-cache");
        assert_eq!(
            cache_dir_for(Os::Windows, &[("LOCALAPPDATA", &local), ("APPDATA", ABS)]).unwrap(),
            PathBuf::from(&local).join("vtteams").join("cache"),
            "Windows: local, not roaming"
        );
        assert_eq!(
            cache_dir_for(Os::Windows, &[("USERPROFILE", ABS)]).unwrap(),
            PathBuf::from(ABS)
                .join("AppData")
                .join("Local")
                .join("vtteams")
                .join("cache")
        );
        assert_eq!(
            cache_dir_for(Os::Mac, &[("HOME", ABS), ("XDG_CACHE_HOME", &xdg)]).unwrap(),
            PathBuf::from(ABS)
                .join("Library")
                .join("Caches")
                .join("vtteams")
        );
        assert_eq!(
            cache_dir_for(Os::Other, &[("XDG_CACHE_HOME", &xdg), ("HOME", ABS)]).unwrap(),
            PathBuf::from(&xdg).join("vtteams")
        );
        assert_eq!(
            cache_dir_for(Os::Other, &[("HOME", ABS)]).unwrap(),
            PathBuf::from(ABS).join(".cache").join("vtteams")
        );
    }

    #[test]
    fn an_empty_or_relative_cache_variable_never_puts_the_cache_in_the_current_directory() {
        let xdg = format!("{ABS}/xdg-cache");
        assert_eq!(
            cache_dir_for(Os::Other, &[("XDG_CACHE_HOME", ""), ("HOME", ABS)]).unwrap(),
            PathBuf::from(ABS).join(".cache").join("vtteams")
        );
        assert_eq!(
            cache_dir_for(Os::Other, &[("XDG_CACHE_HOME", "relative"), ("HOME", ABS)]).unwrap(),
            PathBuf::from(ABS).join(".cache").join("vtteams")
        );
        assert_eq!(
            cache_dir_for(Os::Other, &[("XDG_CACHE_HOME", &xdg)]).unwrap(),
            PathBuf::from(&xdg).join("vtteams")
        );
        for os in [Os::Windows, Os::Mac, Os::Other] {
            assert!(cache_dir_for(os, &[]).is_err(), "{os:?}");
            assert!(
                cache_dir_for(
                    os,
                    &[("LOCALAPPDATA", "x"), ("HOME", "y"), ("USERPROFILE", "")]
                )
                .is_err(),
                "{os:?}"
            );
        }
    }

    use crate::vault::{KeyError, MemoryKeys};

    fn vault_with(keys: &Arc<MemoryKeys>, mode: CredentialsMode) -> Vault {
        let provider: Arc<dyn KeyProvider> = keys.clone();
        open_vault_with(mode, || Ok(provider)).unwrap()
    }

    #[test]
    fn an_unusable_credential_store_stops_start_up_with_a_message_unless_the_file_was_asked_for() {
        for mode in [CredentialsMode::Auto, CredentialsMode::Keychain] {
            let broken = MemoryKeys::broken(KeyError::Unavailable("no Secret Service".into()));
            let provider: Arc<dyn KeyProvider> = broken;
            let e = open_vault_with(mode, || Ok(provider)).unwrap_err();
            assert!(e.contains("not available: no Secret Service"), "{e}");
            assert!(
                e.contains("will not write it as readable text")
                    && e.contains("credentials = \"file\"")
                    && e.contains("--credentials file")
                    && e.contains("VTTEAMS_CREDENTIALS=file"),
                "the message offers the way out: {e}"
            );
            let e =
                open_vault_with(mode, || Err(KeyError::Unavailable("no bus".into()))).unwrap_err();
            assert!(e.contains("no bus"), "{e}");
        }
        // With the file asked for, a store that does not work only means there is no cache.
        let none = open_vault_with(CredentialsMode::File, || {
            Err(KeyError::Unavailable("no bus".into()))
        })
        .unwrap();
        assert!(none.keys.is_none());
        assert!(!none.mode.encrypts());
    }

    #[test]
    fn nothing_is_written_when_the_store_is_unusable_and_the_key_is_made_when_it_is_not() {
        let keys = MemoryKeys::new();
        assert!(!keys.has_key());
        let _ = vault_with(&keys, CredentialsMode::Auto);
        assert!(
            keys.has_key(),
            "start-up makes the key, so a store that cannot is found out now"
        );
        // With the file asked for, a working store is connected to but nothing is created in it.
        let quiet = MemoryKeys::new();
        let _ = vault_with(&quiet, CredentialsMode::File);
        assert!(!quiet.has_key());
        // A store that fails on the first use leaves the settings directory empty.
        let dir = temp("unusable");
        let broken: Arc<dyn KeyProvider> = MemoryKeys::broken(KeyError::Failed("locked".into()));
        assert!(open_vault_with(CredentialsMode::Auto, || Ok(broken)).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    const PLAIN: &str = r#"{"refresh_token":"old-plain-secret","oid":"ada"}"#;

    #[test]
    fn credentials_in_file_mode_are_the_plain_file_exactly_as_before() {
        let dir = temp("file-mode");
        std::fs::write(dir.join("teams-session.json"), PLAIN).unwrap();
        let vault = Vault::none(CredentialsMode::File);
        let c = open_credentials(&dir, Service::Teams, &vault);
        assert!(!c.encrypted && c.notes.is_empty());
        assert_eq!(c.path, dir.join("teams-session.json"));
        assert_eq!(c.store.load().unwrap().as_deref(), Some(PLAIN.as_bytes()));
        c.store.save(b"{\"changed\":1}").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("teams-session.json")).unwrap(),
            "{\"changed\":1}"
        );
        assert!(!dir.join("teams-session.enc").exists());
        // The Graph sign-in is its own file.
        let g = open_credentials(&dir, Service::Graph, &vault);
        assert_eq!(g.path, dir.join("token.json"));
    }

    #[test]
    fn a_readable_sign_in_left_from_file_mode_is_not_used_when_encrypting_and_the_person_is_told() {
        let dir = temp("left-plain");
        std::fs::write(dir.join("teams-session.json"), PLAIN).unwrap();
        let keys = MemoryKeys::new();
        let vault = vault_with(&keys, CredentialsMode::Auto);
        let c = open_credentials(&dir, Service::Teams, &vault);
        assert!(c.encrypted);
        assert_eq!(c.path, dir.join("teams-session.enc"));
        assert_eq!(c.store.load().unwrap(), None, "the plain file is not read");
        assert_eq!(c.notes.len(), 1, "{:?}", c.notes);
        assert!(
            c.notes[0].contains("teams-session.json") && c.notes[0].contains("logout"),
            "{:?}",
            c.notes
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("teams-session.json")).unwrap(),
            PLAIN,
            "and it is left exactly as it was"
        );
        // With nothing left over, nothing is said.
        let g = open_credentials(&dir, Service::Graph, &vault);
        assert!(g.encrypted && g.notes.is_empty(), "{:?}", g.notes);
    }

    #[test]
    fn a_sign_in_that_cannot_be_decrypted_is_a_clear_error_not_a_blank_start() {
        let dir = temp("lost-key");
        let keys = MemoryKeys::new();
        let vault = vault_with(&keys, CredentialsMode::Auto);
        let c = open_credentials(&dir, Service::Teams, &vault);
        c.store.save(b"{}").unwrap();
        // The folder arrives on another computer, where the key is not.
        let other = vault_with(&MemoryKeys::new(), CredentialsMode::Auto);
        let moved = open_credentials(&dir, Service::Teams, &other);
        let e = moved.store.load().unwrap_err();
        assert!(
            e.to_string().contains("cannot be opened") || e.to_string().contains("decrypted"),
            "{e}"
        );
    }

    #[test]
    fn the_cache_needs_a_key_and_an_account() {
        let dir = temp("open-cache");
        let keys = MemoryKeys::new();
        let with = vault_with(&keys, CredentialsMode::Auto);
        assert!(open_cache(&with, &dir, Service::Teams, Some("ada")).is_some());
        assert!(open_cache(&with, &dir, Service::Teams, None).is_none());
        assert!(open_cache(&with, &dir, Service::Teams, Some("")).is_none());
        let without = Vault::none(CredentialsMode::File);
        assert!(
            open_cache(&without, &dir, Service::Teams, Some("ada")).is_none(),
            "never written without a key"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    fn lay_out(config: &Path, cache: &Path, keys: &Arc<MemoryKeys>) {
        std::fs::write(config.join("teams-session.json"), "plain").unwrap();
        std::fs::write(config.join("token.json"), "plain").unwrap();
        std::fs::write(config.join("teams-session.enc"), "sealed").unwrap();
        std::fs::create_dir_all(config.join("browser-profile")).unwrap();
        std::fs::write(config.join("browser-profile").join("Cookies"), "sso").unwrap();
        std::fs::write(config.join("config.toml"), "keymap = \"vim\"\n").unwrap();
        std::fs::write(cache.join("snapshot-teams.cache"), "sealed").unwrap();
        keys.get_or_create().unwrap();
    }

    #[test]
    fn logout_forgets_the_sign_ins_the_key_the_cache_and_the_profile_and_keeps_the_settings() {
        let (config, cache) = (temp("logout-config"), temp("logout-cache"));
        let keys = MemoryKeys::new();
        lay_out(&config, &cache, &keys);
        let provider: Arc<dyn KeyProvider> = keys.clone();
        let done = forget_all(Ok(&config), Ok(&cache), Ok(provider));
        assert!(done.removed && done.failures.is_empty(), "{done:?}");
        assert!(
            !keys.has_key(),
            "the key is deleted from the credential store"
        );
        assert!(!cache.exists(), "and the cache");
        for gone in [
            "teams-session.json",
            "token.json",
            "teams-session.enc",
            "browser-profile",
        ] {
            assert!(!config.join(gone).exists(), "{gone}");
        }
        assert!(
            config.join("config.toml").exists(),
            "the settings are not a sign-in"
        );
        // Nothing left to forget.
        let provider: Arc<dyn KeyProvider> = keys.clone();
        let again = forget_all(Ok(&config), Ok(&cache), Ok(provider));
        assert_eq!(again, Forgotten::default());
    }

    #[test]
    fn logout_reports_every_failure_together_and_still_forgets_what_it_can() {
        let (config, cache) = (temp("logout-fail-config"), temp("logout-fail-cache"));
        let keys = MemoryKeys::new();
        lay_out(&config, &cache, &keys);
        keys.broken
            .lock()
            .unwrap()
            .replace(KeyError::Failed("locked".into()));
        let provider: Arc<dyn KeyProvider> = keys.clone();
        let done = forget_all(Ok(&config), Err("no cache directory".into()), Ok(provider));
        assert_eq!(done.failures.len(), 2, "{:?}", done.failures);
        assert!(
            done.failures
                .iter()
                .any(|f| f.contains("no cache directory"))
        );
        assert!(
            done.failures
                .iter()
                .any(|f| f.contains("key") && f.contains("locked"))
        );
        assert!(done.removed);
        assert!(
            !config.join("teams-session.enc").exists(),
            "the files went anyway"
        );
        assert!(!config.join("browser-profile").exists());

        // A computer with no credential store at all: a plain-file setup has no key to forget and that is not a failure,
        // but with something sealed on disk the key not being reachable is.
        let (config, cache) = (temp("logout-nostore-config"), temp("logout-nostore-cache"));
        std::fs::write(config.join("teams-session.json"), "plain").unwrap();
        // No cache was ever made here (no key store to seal it with).
        let no_cache = cache.join("never-made");
        let done = forget_all(Ok(&config), Ok(&no_cache), Err("no bus".into()));
        assert!(done.failures.is_empty() && done.removed, "{done:?}");
        std::fs::write(config.join("teams-session.enc"), "sealed").unwrap();
        let done = forget_all(Ok(&config), Ok(&no_cache), Err("no bus".into()));
        assert_eq!(done.failures.len(), 1, "{done:?}");
        assert!(done.failures[0].contains("no bus"));
        let done = forget_all(
            Err("no settings dir".into()),
            Err("no cache dir".into()),
            Err("no bus".into()),
        );
        assert_eq!(
            done.failures.len(),
            2,
            "both directories are named: {done:?}"
        );
    }

    #[test]
    fn turning_the_cache_off_deletes_it() {
        let parent = temp("cache-off");
        let dir = parent.join("cache");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("snapshot-teams.cache"), "x").unwrap();
        remove_cache(&dir);
        assert!(!dir.exists());
        remove_cache(&dir);
    }
}
