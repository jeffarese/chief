//! `chief serve` (ARCHITECTURE §14): the one optional long-lived process of a chief home. It
//! fires due routines (§13) for every project and serves the web dashboard for the projects
//! that turned it on. Nothing else depends on it: hooks, the board and the coordinator's tools
//! work the same with it stopped.

mod http;
mod schedule;
mod term;
mod view;

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{ProjectConfig, replace_file};
use crate::{db, generated, paths, routines};

pub use schedule::Scheduler;

pub const DEFAULT_PORT: u16 = 4747;
/// How often the scheduler looks for due routines. A routine fires within this of its time.
pub const TICK: Duration = Duration::from_secs(15);
/// How long `chief serve start` waits for the new process to listen.
const START_WAIT: Duration = Duration::from_secs(10);
const STOP_WAIT: Duration = Duration::from_secs(5);
const LAUNCHD_LABEL: &str = "dev.chief.serve";

/// `<root>/serve.toml`: where the server listens. Optional; flags override it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Settings {
    /// The address to listen on. Anything but a loopback address lets other machines reach the
    /// dashboard (they still need the token), over plain HTTP.
    #[serde(default = "default_bind")]
    pub bind: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
}

fn default_bind() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            bind: default_bind(),
            port: DEFAULT_PORT,
        }
    }
}

impl Settings {
    pub fn load() -> Result<Settings> {
        let path = settings_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("reading {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

pub fn settings_path() -> PathBuf {
    paths::root().join("serve.toml")
}

fn lock_path(root: &Path) -> PathBuf {
    root.join("serve.lock")
}

fn state_path() -> PathBuf {
    paths::root().join("serve.json")
}

fn token_path() -> PathBuf {
    paths::root().join("serve-token")
}

pub fn log_path() -> PathBuf {
    paths::root().join("logs").join("serve.log")
}

/// What the running server wrote about itself (`<root>/serve.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Info {
    pub pid: u32,
    pub bind: IpAddr,
    pub port: u16,
    pub started_at: i64,
}

impl Info {
    /// Where a browser on this machine reaches it.
    pub fn url(&self) -> String {
        let host = match self.bind {
            ip if ip.is_unspecified() => "127.0.0.1".to_string(),
            IpAddr::V6(ip) => format!("[{ip}]"),
            IpAddr::V4(ip) => ip.to_string(),
        };
        format!("http://{host}:{}", self.port)
    }
}

/// The server's lock: held for as long as it runs, released by the kernel however it ends, so
/// "is it running" never trusts a file a crash left behind.
struct Lock(#[allow(dead_code)] File);

fn try_lock(root: &Path) -> Result<Option<Lock>> {
    let path = lock_path(root);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: flock on a descriptor this process owns.
    let got = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if got == 0 {
        return Ok(Some(Lock(file)));
    }
    let err = std::io::Error::last_os_error();
    if err.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(None);
    }
    Err(err).with_context(|| format!("locking {}", path.display()))
}

/// Whether a `chief serve` of this chief home runs: someone holds its lock.
pub fn running() -> bool {
    running_in(&paths::root())
}

/// `running` for the chief home at `root`. A home that doesn't exist has no server, and asking
/// creates nothing in it.
pub fn running_in(root: &Path) -> bool {
    lock_path(root).exists() && matches!(try_lock(root), Ok(None))
}

/// The chief home a project folder lives in (`<root>/projects/<slug>`, §3): where the tools
/// look for the server that fires the project's routines.
pub fn home_of(project_dir: &Path) -> PathBuf {
    project_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(paths::root)
}

/// The running server's address, once it listens.
pub fn info() -> Option<Info> {
    if !running() {
        return None;
    }
    let text = std::fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

/// The secret every dashboard request must carry, made on first use and kept for this chief
/// home (`<root>/serve-token`, readable by the user only).
pub fn token() -> Result<String> {
    let path = token_path();
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if existing.len() >= 32 {
            return Ok(existing.to_string());
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Two v4 uuids: 244 random bits from the OS.
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(token.as_bytes())?;
    Ok(token)
}

/// The link that signs a browser in and lands on `slug`'s dashboard (or the project list).
pub fn login_url(info: &Info, slug: Option<&str>) -> Result<String> {
    let next = slug.map(|s| format!("&next=/p/{s}")).unwrap_or_default();
    Ok(format!("{}/login?token={}{next}", info.url(), token()?))
}

fn log(line: &str) {
    eprintln!("{} {line}", crate::text::local_stamp(db::now_ms()));
}

/// `chief serve`: runs in the foreground until SIGINT or SIGTERM.
pub fn run(bind: Option<IpAddr>, port: Option<u16>) -> Result<()> {
    let settings = Settings::load()?;
    let addr = SocketAddr::new(bind.unwrap_or(settings.bind), port.unwrap_or(settings.port));
    let Some(_lock) = try_lock(&paths::root())? else {
        let at = info()
            .map(|i| format!(" at {}", i.url()))
            .unwrap_or_default();
        bail!("chief serve already runs for this chief home{at}; `chief serve stop` ends it");
    };
    let token = token()?;
    // Open (and migrate) the database before anything is served.
    db::open(&paths::db())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let served = runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("listening on {addr}"))?;
        let local = listener.local_addr()?;
        let info = Info {
            pid: std::process::id(),
            bind: local.ip(),
            port: local.port(),
            started_at: db::now_ms(),
        };
        replace_file(&state_path(), &serde_json::to_string(&info)?)?;
        log(&format!("chief serve listening on {}", info.url()));
        if !local.ip().is_loopback() {
            log(
                "the dashboard is reachable from other machines, over plain HTTP: anyone who \
                 can read the traffic can read the token",
            );
        }
        tokio::spawn(schedule::run());
        http::serve(listener, token).await
    });
    let _ = std::fs::remove_file(state_path());
    // Terminals attached through the dashboard run on blocking threads that end with their
    // sockets; don't wait for them.
    runtime.shutdown_timeout(Duration::from_secs(1));
    served
}

/// `chief serve start`: the server as a background process of its own, logging to
/// `<root>/logs/serve.log`. Returns once it listens. Already running is fine.
pub fn start() -> Result<Info> {
    if let Some(info) = info() {
        return Ok(info);
    }
    let exe = generated::chief_exe()?;
    let log_path = log_path();
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let mut cmd = Command::new(&exe);
    cmd.arg("serve")
        // The resolved root: the child must serve this chief home whatever its cwd.
        .env("CHIEF_HOME", paths::root())
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe; it detaches the server from this terminal, so
    // closing the terminal doesn't end it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting {} serve", exe.display()))?;
    let deadline = Instant::now() + START_WAIT;
    loop {
        if let Some(info) = info() {
            return Ok(info);
        }
        if let Some(status) = child.try_wait()? {
            bail!(
                "chief serve ended at once ({status}); {} says why",
                log_path.display()
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "chief serve didn't start listening within {} s; {} says why",
                START_WAIT.as_secs(),
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `chief serve stop`. False when none ran.
pub fn stop() -> Result<bool> {
    let Some(info) = info() else {
        return Ok(false);
    };
    // SAFETY: a plain signal to the pid the lock holder recorded.
    unsafe { libc::kill(info.pid as i32, libc::SIGTERM) };
    let deadline = Instant::now() + STOP_WAIT;
    while running() {
        if Instant::now() >= deadline {
            bail!(
                "chief serve (pid {}) is still running after {} s",
                info.pid,
                STOP_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(true)
}

/// The projects with routines or the dashboard on: what a running server is for.
fn served() -> Result<Vec<String>> {
    let conn = db::open(&paths::db())?;
    let mut out = Vec::new();
    for (slug, dir) in db::list_projects(&conn)? {
        if db::archived_at(&conn, &slug)?.is_some() {
            continue;
        }
        let dashboard = ProjectConfig::load(&Path::new(&dir).join("project.toml"))
            .is_ok_and(|c| c.dashboard.enabled);
        let list = routines::list(&conn, &slug)?;
        let on = list.iter().filter(|r| r.enabled).count();
        if dashboard || !list.is_empty() {
            let mut parts = Vec::new();
            if dashboard {
                parts.push("dashboard on".to_string());
            }
            if !list.is_empty() {
                parts.push(format!("{on} of {} routines enabled", list.len()));
            }
            out.push(format!("  {slug}: {}", parts.join(", ")));
        }
    }
    Ok(out)
}

/// `chief serve status`.
pub fn status() -> Result<()> {
    match info() {
        Some(info) => println!(
            "chief serve is running (pid {}) at {} since {}.",
            info.pid,
            info.url(),
            crate::text::local_stamp(info.started_at)
        ),
        None if running() => println!("chief serve is starting."),
        None => println!(
            "chief serve isn't running: no routine fires and no dashboard is served. \
             `chief serve start` starts it."
        ),
    }
    let served = served()?;
    if served.is_empty() {
        println!("No project has a routine or its dashboard on.");
    } else {
        println!("{}", served.join("\n"));
    }
    Ok(())
}

/// Starts the server when `slug` needs one (an enabled routine, or its dashboard on) and none
/// runs: what `chief up` does, so a project's routines fire once it has been opened. Returns
/// what to tell the user, if anything.
pub fn ensure_for(conn: &rusqlite::Connection, config: &ProjectConfig) -> Result<Option<String>> {
    let has_routine = routines::list(conn, &config.slug)?
        .iter()
        .any(|r| r.enabled);
    if !(has_routine || config.dashboard.enabled) || running() {
        return Ok(None);
    }
    let info = start()?;
    Ok(Some(format!(
        "Started chief serve at {} (routines{}).",
        info.url(),
        if config.dashboard.enabled {
            " and the dashboard"
        } else {
            ""
        }
    )))
}

/// `chief dashboard <slug> [--off] [--no-open]`: turns the project's dashboard on (or off),
/// makes sure the server runs, and prints the link that signs a browser in.
pub fn dashboard(slug: &str, off: bool, open: bool) -> Result<()> {
    let dir = crate::project::project_dir(slug);
    let path = dir.join("project.toml");
    if !path.exists() {
        bail!("no chief project {slug:?} (looked in {})", dir.display());
    }
    let mut config = ProjectConfig::load(&path)?;
    if off {
        if config.dashboard.enabled {
            config.dashboard.enabled = false;
            config.save(&path)?;
        }
        println!(
            "The dashboard of {} is off. chief serve keeps running for routines and other \
             projects; `chief serve stop` ends it.",
            config.name
        );
        return Ok(());
    }
    if !config.dashboard.enabled {
        config.dashboard.enabled = true;
        config.save(&path)?;
    }
    let info = start()?;
    let url = login_url(&info, Some(slug))?;
    println!("The dashboard of {} is on:", config.name);
    println!("  {url}");
    println!(
        "The link signs this browser in; keep it to yourself: whoever has it can run this \
         project's sessions."
    );
    if !info.bind.is_loopback() {
        println!(
            "chief serve listens on {} (serve.toml), so other machines can open it with that \
             link, over plain HTTP.",
            info.bind
        );
    }
    if open {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = Command::new(opener)
            .arg(&url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    Ok(())
}

fn launchd_plist_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("no home folder")?
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist")))
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The launchd agent that keeps `chief serve` running: started at login, restarted when it
/// ends. It carries this shell's PATH (launchd's own has neither `claude` nor `agp`) and the
/// variables chief's generated files carry (§3).
pub fn launchd_plist(exe: &Path, root: &Path, path_var: &str, env: &[(String, String)]) -> String {
    let mut vars = vec![
        ("PATH".to_string(), path_var.to_string()),
        ("CHIEF_HOME".to_string(), root.display().to_string()),
    ];
    vars.extend(
        env.iter()
            .filter(|(k, _)| k != "CHIEF_HOME" && k != "CLAUDE_CONFIG_DIR")
            .cloned(),
    );
    let vars: String = vars
        .iter()
        .map(|(k, v)| {
            format!(
                "    <key>{}</key>\n    <string>{}</string>\n",
                xml(k),
                xml(v)
            )
        })
        .collect();
    let log = root.join("logs").join("serve.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>serve</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
{vars}  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        exe = xml(&exe.display().to_string()),
        log = xml(&log.display().to_string()),
    )
}

/// `chief serve install`: keeps the server running across logins and restarts (macOS launchd).
pub fn install() -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!(
            "chief serve install sets up a launchd agent, which is macOS only. Elsewhere, run \
             `chief serve` from your own service manager (a systemd user unit with \
             Restart=always works)."
        );
    }
    let exe = generated::chief_exe()?;
    if generated::in_cargo_target(&exe) {
        bail!(
            "{} is inside a cargo target/ folder, which `cargo clean` removes; install chief \
             with `cargo install --path .` first",
            exe.display()
        );
    }
    let plist = launchd_plist_path()?;
    if let Some(dir) = plist.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let path_var = std::env::var("PATH").unwrap_or_default();
    let text = launchd_plist(
        &exe,
        &paths::root(),
        &path_var,
        &generated::passthrough_env(None),
    );
    // launchd would start a second server that fails on the lock, forever: hand over first.
    if stop()? {
        println!("Stopped the running chief serve; launchd starts it again.");
    }
    replace_file(&plist, &text)?;
    // SAFETY: getuid has no preconditions.
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LAUNCHD_LABEL}")])
        .stderr(Stdio::null())
        .status();
    let status = Command::new("launchctl")
        .args(["bootstrap", &domain])
        .arg(&plist)
        .status()
        .context("running launchctl")?;
    if !status.success() {
        bail!(
            "launchctl bootstrap {domain} {} failed ({status})",
            plist.display()
        );
    }
    println!(
        "Installed {}: chief serve now starts at login and restarts when it ends.\n\
         `chief serve uninstall` removes it.",
        plist.display()
    );
    Ok(())
}

/// `chief serve uninstall`.
pub fn uninstall() -> Result<()> {
    let plist = launchd_plist_path()?;
    if !plist.exists() {
        println!("No launchd agent of chief's is installed.");
        return Ok(());
    }
    // SAFETY: getuid has no preconditions.
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LAUNCHD_LABEL}")])
        .status();
    std::fs::remove_file(&plist).with_context(|| format!("removing {}", plist.display()))?;
    println!(
        "Removed {}; chief serve no longer starts by itself.",
        plist.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launchd_agent_carries_the_path_and_the_chief_home() {
        let plist = launchd_plist(
            Path::new("/Users/me/.cargo/bin/chief"),
            Path::new("/Users/me/.local/share/chief"),
            "/opt/homebrew/bin:/usr/bin",
            &[
                ("CHIEF_HOME".to_string(), "/elsewhere".to_string()),
                ("CHIEF_AGP_BIN".to_string(), "/x/a&b".to_string()),
                ("CLAUDE_CONFIG_DIR".to_string(), "/acct".to_string()),
            ],
        );
        assert!(
            plist.contains(
                "<string>/Users/me/.cargo/bin/chief</string>\n    <string>serve</string>"
            )
        );
        assert!(plist.contains("<key>PATH</key>\n    <string>/opt/homebrew/bin:/usr/bin</string>"));
        assert!(
            plist.contains(
                "<key>CHIEF_HOME</key>\n    <string>/Users/me/.local/share/chief</string>"
            )
        );
        // Escaped, and the server never runs in one account: each project names its own.
        assert!(plist.contains("<string>/x/a&amp;b</string>"));
        assert!(!plist.contains("/elsewhere"));
        assert!(!plist.contains("CLAUDE_CONFIG_DIR"));
        assert!(plist.contains("/Users/me/.local/share/chief/logs/serve.log"));
    }

    #[test]
    fn a_wildcard_bind_is_reached_on_loopback() {
        let info = |bind: &str| Info {
            pid: 1,
            bind: bind.parse().unwrap(),
            port: 4747,
            started_at: 0,
        };
        assert_eq!(info("0.0.0.0").url(), "http://127.0.0.1:4747");
        assert_eq!(info("127.0.0.1").url(), "http://127.0.0.1:4747");
        assert_eq!(info("100.64.0.7").url(), "http://100.64.0.7:4747");
        assert_eq!(info("::1").url(), "http://[::1]:4747");
    }

    #[test]
    fn settings_default_to_loopback() {
        let settings: Settings = toml::from_str("").unwrap();
        assert_eq!(settings, Settings::default());
        assert!(settings.bind.is_loopback());
        let settings: Settings = toml::from_str("bind = \"0.0.0.0\"\nport = 8080").unwrap();
        assert_eq!(settings.port, 8080);
        assert!(settings.bind.is_unspecified());
    }
}
