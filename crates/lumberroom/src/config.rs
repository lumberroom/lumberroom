//! Credential resolution, and the config file both clients share.
//!
//! `~/.config/lumberroom/config.json` is written by `bin/lumberroom.mjs` and by `wire-mac.sh`, and this client
//! reads and writes the same file. Node's `saveConfig` is a shallow merge over whatever it parsed,
//! so it preserves keys it knows nothing about. A typed struct that serialised only its own fields
//! would delete the other client's data on the first `login`, so the file is carried as a
//! `serde_json::Value` and patched at the top level.

use fs2::FileExt;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_URL: &str = "http://127.0.0.1:8787";
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// How long `save` waits for the config lock. A refresh holds it across one token request,
/// which the default `LUMBERROOM_TIMEOUT_MS` of 15s bounds, so 30s outlasts one full holder
/// with margin. `refresh` passes its own request timeout plus margin instead of this constant,
/// so an operator who raises the timeout keeps a ceiling that still covers the holder.
const LOCK_WAIT: Duration = Duration::from_secs(30);
const LOCK_POLL: Duration = Duration::from_millis(200);

/// Environment lookup, injected rather than read from the process.
///
/// Tests set precedence cases in parallel and `std::env::set_var` is process-wide, so a test that
/// mutated it would race every other test in the binary.
pub trait Env {
    fn get(&self, key: &str) -> Option<String>;
}

pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

impl Env for HashMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        HashMap::get(self, key).cloned()
    }
}

/// The file, kept whole. `value` is whatever parsed, or an empty object when the file is absent or
/// unreadable: node swallows both cases the same way, and a missing config is the normal first run.
#[derive(Debug, Clone)]
pub struct FileConfig {
    pub path: PathBuf,
    pub value: Value,
}

impl FileConfig {
    pub fn load(path: PathBuf) -> Self {
        let value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        Self { path, value }
    }

    pub fn empty(path: PathBuf) -> Self {
        Self { path, value: json!({}) }
    }

    pub fn str_field(&self, key: &str) -> Option<&str> {
        self.value.get(key)?.as_str().filter(|s| !s.is_empty())
    }

    pub fn oauth(&self, key: &str) -> Option<&str> {
        self.value.get("oauth")?.get(key)?.as_str().filter(|s| !s.is_empty())
    }

    pub fn has_oauth_access_token(&self) -> bool {
        self.oauth("access_token").is_some()
    }

    /// Merge a top-level patch into the file on disk and write it back at 0600.
    ///
    /// Two properties the old plain `fs::write` could not give. The write is a sibling file
    /// renamed over the live path, so a reader never sees half a credential file and a process
    /// that dies mid-write cannot leave one behind; the inode the readers hold either has the
    /// whole old file or the whole new one. And the read-modify-write runs under an exclusive
    /// lock on `<path>.lock`, so two processes patching the same file merge instead of one
    /// erasing the other's keys.
    ///
    /// The mode rules are the ones node states and this file has always enforced: the file is
    /// 0600 on disk and the replacement is born 0600 (the temp file is created with that mode;
    /// there is no window at the umask), a file already sitting looser than 0600 is refused
    /// rather than repaired, and the directory the file appears in is 0700 at every level this
    /// call creates.
    pub fn save(&mut self, patch: Map<String, Value>) -> std::io::Result<()> {
        let lock = FileConfig::lock_config(&self.path, LOCK_WAIT)?;
        self.save_locked(&lock, patch)
    }

    /// The write half, under a lock the caller already holds.
    ///
    /// `refresh` in `client.rs` holds the lock across the whole token exchange and finishes
    /// with this; every other caller wants `save`, which takes the lock for the write alone.
    pub fn save_locked(
        &mut self,
        _lock: &ConfigLock,
        patch: Map<String, Value>,
    ) -> std::io::Result<()> {
        // A file already sitting at 0644, from a crash between write and chmod or a restore from
        // a backup, is refused rather than rewritten: repairing it silently would make a token
        // that has been readable by every local account for a week look clean.
        refuse_loose_permissions(&self.path)?;
        // Re-read the file under the lock. `self.value` is a snapshot from whenever this
        // process loaded it, and merging the patch into that snapshot erases every write
        // another process made in between, which against a rotating refresh token is the
        // lockout with two writers instead of one crashed one. Missing or corrupt reads as
        // empty, exactly as `load` treats them.
        self.value = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        let obj = self.value.as_object_mut().expect("config value is an object by construction");
        for (k, v) in patch {
            obj.insert(k, v);
        }
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        create_private_dir(parent)?;
        let body = format!("{}\n", serde_json::to_string_pretty(&self.value)?);

        // Born 0600, in the same directory so the rename stays inside one filesystem. tempfile
        // already creates at 0600 on unix; the explicit mode documents the invariant this whole
        // function is trusted for and holds if that default ever moves.
        let name = self
            .path
            .file_name()
            .map(|n| format!(".{}.", n.to_string_lossy()))
            .unwrap_or_else(|| ".config.".to_string());
        let mut builder = tempfile::Builder::new();
        builder.prefix(&name).suffix(".tmp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o600));
        }
        let mut replacement = builder.tempfile_in(parent)?;
        replacement.write_all(body.as_bytes())?;
        // fsync before the rename: a replacement that survives the crash but not the power cut
        // loses the same refresh token this function exists to protect.
        replacement.as_file().sync_all()?;
        replacement.persist(&self.path).map_err(|e| e.error)?;
        sync_dir(parent)?;
        // No chmod after the rename. The replacement was born 0600 and rename carries the mode
        // with the inode, so there is nothing to repair; a chmod that failed here would return
        // an error from a save that had already landed, and refresh would tell the owner the
        // tokens were not saved while the file on disk held the new pair. The next refresh
        // would then present a token the server had rotated.
        Ok(())
    }

    /// The exclusive lock on `<config>.lock`, held for the life of the guard.
    ///
    /// A waiter gives up after `wait` and gets an error naming the lock file and whoever last
    /// wrote the holder line into it, because the alternative is a silent hang behind a process
    /// stuck on a slow token endpoint. The lock file is never removed: unlinking a lock file
    /// while another process waits on it lets a third open a fresh inode and hold it beside the
    /// first, and then the lock protects nothing.
    ///
    /// This one sleeps the calling thread between attempts. A caller inside a runtime wants
    /// `lock_config_async`.
    pub fn lock_config(path: &Path, wait: Duration) -> std::io::Result<ConfigLock> {
        let (file, lock_path) = open_lock_file(path)?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    write_holder(&file);
                    return Ok(ConfigLock { _file: file });
                }
                Err(_) if std::time::Instant::now() < deadline => std::thread::sleep(LOCK_POLL),
                Err(e) => return Err(lock_refused(&lock_path, wait, e)),
            }
        }
    }

    /// The same lock, waited on without holding the thread.
    ///
    /// flock attaches to the open file description, so two futures in one process that each open
    /// the lock file do conflict. `refresh` runs on a current-thread runtime beside as many as
    /// three other in-flight writes, and a waiter that parks that thread in `thread::sleep`
    /// parks the holder with it: the holder never gets polled, its own request times out on a
    /// token the server has already rotated, and the spent token is left on disk. Yielding
    /// between attempts is what lets the holder finish and the waiter take the lock after it.
    ///
    /// Same file, same deadline and same refusal as the blocking form, so the guarantee across
    /// processes does not move.
    pub async fn lock_config_async(path: &Path, wait: Duration) -> std::io::Result<ConfigLock> {
        let (file, lock_path) = open_lock_file(path)?;
        let deadline = std::time::Instant::now() + wait;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    write_holder(&file);
                    return Ok(ConfigLock { _file: file });
                }
                Err(_) if std::time::Instant::now() < deadline => {
                    tokio::time::sleep(LOCK_POLL).await
                }
                Err(e) => return Err(lock_refused(&lock_path, wait, e)),
            }
        }
    }
}

/// The lock file itself, created at 0600 in a 0700 directory, with the path back for the
/// messages. Both waits open it the same way.
fn open_lock_file(path: &Path) -> std::io::Result<(std::fs::File, PathBuf)> {
    let lock_path = lock_path_for(path);
    if let Some(parent) = lock_path.parent() {
        create_private_dir(parent)?;
    }
    // No truncate on open: the holder line belongs to whoever won the lock.
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts
        .open(&lock_path)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", lock_path.display())))?;
    // The holder line carries no credential, but the file is created here and 0600 is the
    // invariant every other file in this directory keeps.
    restrict(&lock_path)?;
    Ok((file, lock_path))
}

/// What a waiter that outlasted its deadline gets told.
///
/// It names the lock file and the holder, and stops there. Telling anyone to delete the file
/// would be telling them to cause the bug: this message arrives exactly when a refresh is in
/// flight and slow, and whoever unlinks the lock lets the next process open a fresh inode, lock
/// that, read the pre-rotation token and replay it.
fn lock_refused(lock_path: &Path, wait: Duration, e: std::io::Error) -> std::io::Error {
    let who = std::fs::read_to_string(lock_path)
        .ok()
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.trim().to_string())
        .unwrap_or_else(|| "an unnamed process".to_string());
    std::io::Error::new(
        std::io::ErrorKind::WouldBlock,
        format!(
            "another lumberroom process holds {}: {who} (waited {}s: {e}). It may be mid-refresh \
             against a slow token endpoint. Retry in a moment.",
            lock_path.display(),
            wait.as_secs()
        ),
    )
}

/// An exclusive lock over the config directory's lock file. Dropping the guard releases it.
#[derive(Debug)]
pub struct ConfigLock {
    _file: std::fs::File,
}

fn lock_path_for(config: &Path) -> PathBuf {
    let mut name = config.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// Best effort, so a directory some filesystem refuses to fsync cannot fail a save whose rename
/// already landed.
#[cfg(unix)]
fn sync_dir(path: &Path) -> std::io::Result<()> {
    let _ = std::fs::File::open(path)?.sync_all();
    Ok(())
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn write_holder(file: &std::fs::File) {
    let line = format!("pid {} since {}", std::process::id(), chrono::Utc::now().to_rfc3339());
    if let Ok(mut handle) = file.try_clone() {
        let _ = handle.set_len(0);
        let _ = handle.seek(SeekFrom::Start(0));
        let _ = handle.write_all(line.as_bytes());
        let _ = handle.flush();
    }
}

#[cfg(not(unix))]
fn write_holder(_file: &std::fs::File) {}

/// Refuse a config file that group or other can read. No file is fine: a first `login` has
/// nothing to leak yet. Called before a token is read out of the file as well as before one is
/// written into it, so a loose file is never used, only reported.
#[cfg(unix)]
pub fn refuse_loose_permissions(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "{} has mode {mode:04o} and must not be readable by group or other. It holds a \
                 credential that every local account could have read; treat that credential as \
                 exposed, run `chmod 600 {}`, then `lumberroom login` again or re-run wire-mac.sh \
                 with a fresh token.",
                path.display(),
                path.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn refuse_loose_permissions(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// The config directory at 0700, every level of it this call creates. `create_dir_all` would
/// make it at the umask, usually 0755, which is traversable by every local account and makes the
/// file's own mode the only thing between them and the token.
#[cfg(unix)]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
}

#[cfg(not(unix))]
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(path)
}

/// Owner-only, on a file holding a bearer token.
#[cfg(unix)]
pub fn restrict(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
pub fn restrict(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

fn nonempty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

pub fn config_path(env: &dyn Env) -> PathBuf {
    if let Some(p) = env.get("LUMBERROOM_CONFIG").filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    let home = env.get("HOME").unwrap_or_default();
    PathBuf::from(home).join(".config").join("lumberroom").join("config.json")
}

/// Which credential a run is using, for the line `doctor` prints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Credential {
    Static,
    Oauth,
    None,
}

#[derive(Debug, Clone)]
pub struct Resolved {
    /// Trailing slashes stripped, exactly as node does before deriving the other two.
    pub base_url: String,
    /// Where the MCP transport is mounted.
    pub mcp_url: String,
    /// Everything else hangs off here: /healthz, /admin/*, /oauth/*.
    pub http_base: String,
    pub token: String,
    pub credential: Credential,
    pub invocation: String,
    pub timeout_ms: u64,
}

/// Flags beat environment beats file, and a static token beats an OAuth access token.
///
/// The static-over-oauth order is not a preference, it is what keeps a mode switch honest:
/// `wire-mac.sh --token-mode` writes `token` and never touches `oauth`, `--oauth-mode` does the
/// reverse, and a leftover `token` from an earlier mode would otherwise defeat a credential
/// `login` had just minted while looking like a server fault.
pub fn resolve(
    env: &dyn Env,
    file: &FileConfig,
    url_flag: Option<&str>,
    token_flag: Option<&str>,
    invocation_flag: Option<&str>,
    hook: bool,
    timeout_flag: Option<&str>,
) -> Resolved {
    // An empty value counts as unset at every step, which is the one place this client does not
    // copy node. Node's `??` chain stops on an empty string, so `LUMBERROOM_TOKEN=` there yields no
    // credential at all and `LUMBERROOM_URL=` yields the endpoint `/mcp`. An operator who leaves a blank
    // variable in a compose file means "not set", and reading it as "set to nothing" turns a
    // configuration slip into a request against a URL that cannot resolve.
    let base_url = nonempty(url_flag.map(str::to_string))
        .or_else(|| nonempty(env.get("LUMBERROOM_URL")))
        .or_else(|| file.str_field("url").map(str::to_string))
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let base_url = base_url.trim_end_matches('/').to_string();

    let mut credential = Credential::Static;
    let token = nonempty(token_flag.map(str::to_string))
        .or_else(|| nonempty(env.get("LUMBERROOM_TOKEN")))
        .or_else(|| file.str_field("token").map(str::to_string))
        .or_else(|| {
            credential = Credential::Oauth;
            file.oauth("access_token").map(str::to_string)
        })
        .unwrap_or_else(|| {
            credential = Credential::None;
            String::new()
        });

    let mcp_url =
        if base_url.ends_with("/mcp") { base_url.clone() } else { format!("{base_url}/mcp") };
    let http_base = base_url.strip_suffix("/mcp").unwrap_or(&base_url).to_string();

    let invocation = invocation_flag.map(str::to_string).unwrap_or_else(|| {
        if hook {
            "hook".to_string()
        } else {
            "cli".to_string()
        }
    });

    let timeout_ms = nonempty(timeout_flag.map(str::to_string))
        .or_else(|| nonempty(env.get("LUMBERROOM_TIMEOUT_MS")))
        .and_then(|v| crate::args::parse_int_prefix(&v))
        .filter(|n| *n > 0)
        .map(|n| n as u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS);

    Resolved { base_url, mcp_url, http_base, token, credential, invocation, timeout_ms }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn file(value: Value) -> FileConfig {
        FileConfig { path: PathBuf::from("/dev/null"), value }
    }

    #[test]
    fn flag_beats_env_beats_file() {
        let e =
            env(&[("LUMBERROOM_URL", "https://env.example"), ("LUMBERROOM_TOKEN", "env-token")]);
        let f = file(json!({ "url": "https://file.example", "token": "file-token" }));

        let r =
            resolve(&e, &f, Some("https://flag.example"), Some("flag-token"), None, false, None);
        assert_eq!(r.base_url, "https://flag.example");
        assert_eq!(r.token, "flag-token");

        let r = resolve(&e, &f, None, None, None, false, None);
        assert_eq!(r.base_url, "https://env.example");
        assert_eq!(r.token, "env-token");

        let r = resolve(&HashMap::new(), &f, None, None, None, false, None);
        assert_eq!(r.base_url, "https://file.example");
        assert_eq!(r.token, "file-token");

        let r = resolve(&HashMap::new(), &file(json!({})), None, None, None, false, None);
        assert_eq!(r.base_url, DEFAULT_URL);
        assert_eq!(r.token, "");
        assert_eq!(r.credential, Credential::None);
    }

    #[test]
    fn a_static_token_beats_an_oauth_access_token() {
        let f = file(json!({ "token": "static", "oauth": { "access_token": "oauth" } }));
        let r = resolve(&HashMap::new(), &f, None, None, None, false, None);
        assert_eq!(r.token, "static");
        assert_eq!(r.credential, Credential::Static);
    }

    #[test]
    fn oauth_is_used_when_no_static_token_exists() {
        let f = file(json!({ "oauth": { "access_token": "oauth" } }));
        let r = resolve(&HashMap::new(), &f, None, None, None, false, None);
        assert_eq!(r.token, "oauth");
        assert_eq!(r.credential, Credential::Oauth);
    }

    #[test]
    fn trailing_slashes_go_and_mcp_is_not_doubled() {
        let r = resolve(
            &HashMap::new(),
            &file(json!({})),
            Some("https://s.example///"),
            None,
            None,
            false,
            None,
        );
        assert_eq!(r.base_url, "https://s.example");
        assert_eq!(r.mcp_url, "https://s.example/mcp");
        assert_eq!(r.http_base, "https://s.example");

        let r = resolve(
            &HashMap::new(),
            &file(json!({})),
            Some("https://s.example/mcp"),
            None,
            None,
            false,
            None,
        );
        assert_eq!(r.mcp_url, "https://s.example/mcp");
        assert_eq!(r.http_base, "https://s.example");
    }

    #[test]
    fn an_empty_env_value_does_not_shadow_the_file() {
        let e = env(&[("LUMBERROOM_TOKEN", "")]);
        let f = file(json!({ "token": "file-token" }));
        assert_eq!(resolve(&e, &f, None, None, None, false, None).token, "file-token");
    }

    #[test]
    fn invocation_marks_hook_runs() {
        let f = file(json!({}));
        assert_eq!(resolve(&HashMap::new(), &f, None, None, None, false, None).invocation, "cli");
        assert_eq!(resolve(&HashMap::new(), &f, None, None, None, true, None).invocation, "hook");
        assert_eq!(
            resolve(&HashMap::new(), &f, None, None, Some("openwebui"), true, None).invocation,
            "openwebui"
        );
    }

    #[test]
    fn timeout_comes_from_the_flag_then_the_environment() {
        let e = env(&[("LUMBERROOM_TIMEOUT_MS", "5000")]);
        let f = file(json!({}));
        assert_eq!(resolve(&e, &f, None, None, None, false, None).timeout_ms, 5000);
        assert_eq!(resolve(&e, &f, None, None, None, false, Some("900")).timeout_ms, 900);
        assert_eq!(
            resolve(&HashMap::new(), &f, None, None, None, false, None).timeout_ms,
            DEFAULT_TIMEOUT_MS
        );
    }

    #[test]
    fn config_path_honours_lumberroom_config_then_home() {
        let e = env(&[("LUMBERROOM_CONFIG", "/tmp/x.json"), ("HOME", "/home/o")]);
        assert_eq!(config_path(&e), PathBuf::from("/tmp/x.json"));
        let e = env(&[("HOME", "/home/o")]);
        assert_eq!(config_path(&e), PathBuf::from("/home/o/.config/lumberroom/config.json"));
    }

    #[test]
    fn save_keeps_keys_this_client_does_not_know() {
        let dir = std::env::temp_dir().join(format!("lumberroom-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"url":"https://s.example","somethingElse":{"a":1}}"#).unwrap();
        restrict(&path).unwrap();

        let mut cfg = FileConfig::load(path.clone());
        let mut patch = Map::new();
        patch.insert("oauth".into(), json!({ "access_token": "t" }));
        cfg.save(patch).unwrap();

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["somethingElse"]["a"], json!(1));
        assert_eq!(back["url"], json!("https://s.example"));
        assert_eq!(back["oauth"]["access_token"], json!("t"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn saving_a_config_that_never_existed_creates_it_at_0600_directly() {
        // Distinct from `save_keeps_keys_this_client_does_not_know`, which pre-creates the file
        // with a plain `std::fs::write` before ever calling `save`: that test only proves the
        // final mode, not that the file was never briefly world-readable on the way there. This
        // one drives `save` against a path nothing has touched, the shape `lumberroom login` hits on a
        // first run.
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("lumberroom-cfg-fresh-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nested").join("config.json");
        assert!(!path.exists());

        let mut cfg = FileConfig::empty(path.clone());
        let mut patch = Map::new();
        patch.insert("oauth".into(), json!({ "access_token": "t" }));
        cfg.save(patch).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a config file must never be created at a looser mode than 0600");
        let dir_mode =
            std::fs::metadata(path.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "the directory save creates is not traversable by others");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_config_readable_by_others_is_refused_rather_than_rewritten() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("lumberroom-cfg-loose-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"oauth":{"refresh_token":"r"}}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let refused = refuse_loose_permissions(&path).unwrap_err();
        assert!(refused.to_string().contains("chmod 600"), "{refused}");

        let mut cfg = FileConfig::load(path.clone());
        let mut patch = Map::new();
        patch.insert("oauth".into(), json!({ "access_token": "t" }));
        assert!(cfg.save(patch).is_err(), "save must not repair a file that has been exposed");
        let back = std::fs::read_to_string(&path).unwrap();
        assert!(!back.contains("access_token"), "nothing new was written into a loose file");

        assert!(refuse_loose_permissions(&dir.join("absent.json")).is_ok(), "no file, no leak");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn save_replaces_the_file_rather_than_writing_it_in_place() {
        // The inode is the fact that makes the write crash-atomic. A save that opens the live
        // path and truncates it (what std::fs::write does) keeps the inode and leaves every
        // reader of a half-written file holding a credential file with no bottom half: a
        // process killed mid-write, or one that dies between the truncate and the write, has
        // spent a refresh token the file no longer names. A save that writes a sibling and
        // renames it over the live path swaps the inode, and a crash can only cost the whole
        // rename, never half of it.
        use std::os::unix::fs::MetadataExt;

        let dir = std::env::temp_dir().join(format!("lumberroom-cfg-inode-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"url":"https://s.example"}"#).unwrap();
        restrict(&path).unwrap();
        let before = std::fs::metadata(&path).unwrap().ino();

        let mut cfg = FileConfig::load(path.clone());
        let mut patch = Map::new();
        patch.insert("oauth".into(), json!({ "access_token": "t" }));
        cfg.save(patch).unwrap();

        let after = std::fs::metadata(&path).unwrap().ino();
        assert_ne!(
            before, after,
            "save must put the new file in place by rename, not rewrite the live one"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_save_that_cannot_complete_leaves_the_live_file_alone() {
        use std::os::unix::fs::PermissionsExt;

        // The directory pre-exists on purpose: create_private_dir is then a no-op and the
        // refusal has to come from the write itself. A save that cannot put the new file in
        // place must fail with the old file byte-identical, because the old refresh token is
        // still the one the server accepts until a rename says otherwise. std::fs::write
        // opens and truncates the live file, so on the old code this save succeeds and the
        // assertion below fails.
        let dir = std::env::temp_dir().join(format!("lumberroom-cfg-ro-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"url":"https://s.example","oauth":{"refresh_token":"r0"}}"#)
            .unwrap();
        restrict(&path).unwrap();
        // The lock file has to exist before the directory closes, or the save fails creating
        // it and returns before it ever reaches a temp file. The test would then pass on a
        // refusal from the lock rather than from the write, and the rename path, which is what
        // it is here to hold, would go untested.
        std::fs::write(lock_path_for(&path), b"").unwrap();
        restrict(&lock_path_for(&path)).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        // The precondition is that the kernel refuses a write into a 0500 directory. Root ignores
        // directory modes, and so does a filesystem that does not enforce them, and under either
        // one this fixture cannot tell working refusal logic from none at all.
        //
        // It fails here rather than returning. The version that returned reported `ok` for as long
        // as cargo ran as root in the builder container, because libtest captures a passing test's
        // stderr: the line it printed reached nobody and the gate counted a test that measured
        // nothing. A precondition that cannot be established is a broken test, and a broken test
        // has to be loud.
        if std::fs::write(dir.join(".probe"), b"").is_ok() {
            use std::os::unix::fs::MetadataExt;
            let euid = std::fs::metadata(dir.join(".probe")).unwrap().uid();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            std::fs::remove_dir_all(&dir).ok();
            panic!(
                "precondition not met: writing into {} succeeded while it is mode 0500, so this \
                 test cannot observe the refusal it exists to check. This process runs as uid \
                 {euid}, and uid 0 ignores directory modes. Run the suite as a non-root user: \
                 ./scripts/cargo.sh does that through scripts/lib/builder-entrypoint.sh, which \
                 needs the lumberroom-builder image rebuilt (docker build -t lumberroom-builder \
                 -f Dockerfile.builder .).",
                dir.display()
            );
        }

        let mut cfg = FileConfig::load(path.clone());
        let mut patch = Map::new();
        patch.insert("oauth".into(), json!({ "access_token": "t" }));
        let refused = cfg.save(patch).expect_err("save must refuse when it cannot write");
        // The lock opens by path and reports it, so a message naming the lock means the save
        // stopped there and the rename this test guards never ran.
        assert!(
            !refused.to_string().contains(".lock"),
            "the refusal comes from the write, not from the lock: {refused}"
        );

        let back = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            back, r#"{"url":"https://s.example","oauth":{"refresh_token":"r0"}}"#,
            "the live file is the only copy of a token the server still accepts"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_save_merges_what_another_process_wrote_in_the_meantime() {
        // Two processes load the same file, both patch, and the second save must build on what
        // the first wrote rather than on its own stale copy. On the old code the second save
        // writes its stale base plus its own patch, and the first process's keys are gone:
        // against a rotating refresh token, that is the lockout with two writers instead of
        // one crashed one.
        let dir = std::env::temp_dir().join(format!("lumberroom-cfg-lost-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"url":"https://s.example"}"#).unwrap();
        restrict(&path).unwrap();

        let mut first = FileConfig::load(path.clone());
        let mut second = FileConfig::load(path.clone());
        let mut patch = Map::new();
        patch.insert("oauth".into(), json!({ "access_token": "a1", "refresh_token": "r1" }));
        first.save(patch).unwrap();

        let mut patch = Map::new();
        patch.insert("token".into(), json!("static-late"));
        second.save(patch).unwrap();

        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back["url"], json!("https://s.example"));
        assert_eq!(
            back["oauth"]["refresh_token"],
            json!("r1"),
            "the first save's keys survive the second save"
        );
        assert_eq!(back["token"], json!("static-late"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_waiter_for_a_held_lock_names_the_file_and_gives_up() {
        let dir = std::env::temp_dir().join(format!("lumberroom-cfg-lock-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        let held = FileConfig::lock_config(&path, Duration::from_millis(100)).unwrap();
        let start = std::time::Instant::now();
        let refused = FileConfig::lock_config(&path, Duration::from_millis(300)).unwrap_err();
        assert!(start.elapsed() >= Duration::from_millis(300), "gave up before the wait elapsed");
        let text = refused.to_string();
        assert!(text.contains("config.json.lock"), "names the lock file: {text}");
        assert!(text.contains(&format!("pid {}", std::process::id())), "names the holder: {text}");
        // This message appears exactly when a refresh is in flight and slow. A reader who
        // deletes the lock file lets the next process open a fresh inode, lock that, read the
        // pre-rotation token and replay it, which is the lockout this file exists to prevent.
        assert!(!text.contains("remove the file"), "does not advise deleting the lock: {text}");

        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(lock_path_for(&path)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the lock file sits at 0600 like everything else here");

        drop(held);
        FileConfig::lock_config(&path, Duration::from_millis(300))
            .expect("the lock is free again once the holder drops it");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A unique scratch-directory suffix. The process id alone collides across two tests running
    /// in the same binary, which is why the other tests in this module also nest a distinct
    /// literal into their path; this one has none to nest.
    fn uuid_like() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn a_corrupt_config_file_reads_as_empty() {
        let dir = std::env::temp_dir().join(format!("lumberroom-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "not json at all").unwrap();
        let cfg = FileConfig::load(path);
        assert_eq!(cfg.value, json!({}));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The interrupted-write half of the lockout bug. A save that is cut in half, by a second
    /// reader arriving mid-write or by a process that dies during one, must leave the live path
    /// holding either the whole previous file or the whole new one. `std::fs::write` truncates
    /// first and fills in after, so a reader in that window parses half a JSON object (and
    /// `load` above hands back an empty config, the silent "no credential" run).
    ///
    /// The filler alternates between megabytes and bytes so the truncate-then-write gap is wide
    /// enough to catch regardless of how the kernel schedules the copies.
    #[test]
    fn a_reader_never_sees_a_half_written_config() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = std::env::temp_dir().join(format!("lumberroom-cfg-torn-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"gen":-1}"#).unwrap();
        restrict(&path).unwrap();

        let done = Arc::new(AtomicBool::new(false));
        let writer_done = done.clone();
        let writer_path = path.clone();
        let writer = std::thread::spawn(move || {
            let mut cfg = FileConfig { path: writer_path, value: json!({}) };
            for gen in 0..40i64 {
                let filler = "x".repeat(if gen % 2 == 0 { 3_000_000 } else { 300 });
                // The generation travels as the patch: save merges the patch into whatever is
                // on disk, so a value set only on the in-memory copy would never reach the
                // file and the reader below would be watching a file that never changes.
                let mut patch = Map::new();
                patch.insert("gen".into(), json!(gen));
                patch.insert("filler".into(), json!(filler));
                cfg.save(patch).unwrap();
            }
            writer_done.store(true, Ordering::Relaxed);
        });

        let mut problems: Vec<String> = Vec::new();
        while !done.load(Ordering::Relaxed) {
            match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<Value>(&text) {
                    Ok(v) => {
                        let gen = v.get("gen").and_then(Value::as_i64);
                        let len = v.get("filler").and_then(Value::as_str).map(str::len);
                        // The seed file carries gen -1 and no filler; it is a whole file,
                        // just not one the writer produced.
                        let expected = match gen {
                            Some(-1) => len.is_none(),
                            Some(g) => len == Some(if g % 2 == 0 { 3_000_000 } else { 300 }),
                            None => false,
                        };
                        if !expected {
                            problems.push(format!("generation {gen:?} came through as {len:?}"));
                        }
                    }
                    Err(e) => problems.push(format!("a reader caught a partial file: {e}")),
                },
                Err(e) => problems.push(format!("the live path was unreadable: {e}")),
            }
            // Whatever else sits in the directory mid-save, a temporary file holding credential
            // material being one, nobody but the owner may read it.
            #[cfg(unix)]
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                if entry.file_name().to_string_lossy() == "config.json" {
                    continue;
                }
                // A temporary file renamed over the live path between the directory listing
                // and this stat is gone, not loose; skipping it is not a hole, because the
                // name it carried is gone with it.
                let Ok(meta) = entry.metadata() else { continue };
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    problems.push(format!("{} sat at {mode:04o} mid-save", entry.path().display()));
                }
            }
        }
        writer.join().unwrap();
        assert!(problems.is_empty(), "{}", problems.join("; "));
        std::fs::remove_dir_all(&dir).ok();
    }
}
