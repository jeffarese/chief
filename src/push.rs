//! Reaching the user when they aren't looking (ARCHITECTURE §13 "Reaching the user"): the
//! desktop notification chief always had, and a command of the user's own that carries a
//! message anywhere else (ntfy, Telegram, mail). Both are fire and forget: a hook or a tool
//! call never waits on them.

use std::process::{Command, Stdio};

use crate::config::{ProjectConfig, PushOn};

/// What a message is about, as `CHIEF_KIND` gives it to the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A routine's run says the user should look or decide now.
    Attention,
    /// A routine's run has news that can wait.
    Update,
    /// A thread is stuck on a permission prompt.
    Permission,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Attention => "attention",
            Kind::Update => "update",
            Kind::Permission => "permission",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub kind: Kind,
    pub title: String,
    pub body: String,
    /// Where to look: the dashboard's page for it, when the dashboard is served.
    pub url: Option<String>,
}

/// Whether the project's settings push a message of this kind.
pub fn wanted(config: &ProjectConfig, kind: Kind) -> bool {
    config.push.command().is_some()
        && match kind {
            Kind::Attention | Kind::Permission => true,
            Kind::Update => config.push.on == PushOn::Updates,
        }
}

/// Runs the project's push command for `message`, when its settings want one of that kind.
/// The text travels in the environment, never in the command line, so nothing in a headline can
/// be read as shell. Returns whether a command was started.
pub fn send(config: &ProjectConfig, message: &Message) -> bool {
    let Some(command) = config
        .push
        .command()
        .filter(|_| wanted(config, message.kind))
    else {
        return false;
    };
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("CHIEF_TITLE", &message.title)
        .env("CHIEF_BODY", &message.body)
        .env("CHIEF_KIND", message.kind.as_str())
        .env("CHIEF_PROJECT", &config.slug)
        .env("CHIEF_URL", message.url.as_deref().unwrap_or(""))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// A desktop notification (macOS `osascript`; nothing where it is missing). `CHIEF_NO_NOTIFY=1`
/// turns it off (tests). Text goes in as argv so quotes in a title can't break the AppleScript.
pub fn desktop(title: &str, message: &str) {
    if std::env::var_os("CHIEF_NO_NOTIFY").is_some_and(|v| v == "1") {
        return;
    }
    let _ = Command::new("osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            title,
            message,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// The dashboard's page for something in a project, when a server runs and shows the project.
pub fn dashboard_url(config: &ProjectConfig, hash: &str) -> Option<String> {
    if !config.dashboard.enabled {
        return None;
    }
    let info = crate::serve::info()?;
    Some(format!("{}/p/{}{hash}", info.url(), config.slug))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(command: Option<&str>, on: PushOn) -> ProjectConfig {
        let mut config = ProjectConfig::new("Home", "home", None, vec![]);
        config.push.command = command.map(str::to_string);
        config.push.on = on;
        config
    }

    #[test]
    fn only_what_the_settings_ask_for_is_pushed() {
        let none = config(None, PushOn::Updates);
        let blank = config(Some("  "), PushOn::Updates);
        let quiet = config(Some("true"), PushOn::Attention);
        let chatty = config(Some("true"), PushOn::Updates);
        for kind in [Kind::Attention, Kind::Update, Kind::Permission] {
            assert!(!wanted(&none, kind));
            assert!(!wanted(&blank, kind));
            assert!(wanted(&chatty, kind));
        }
        assert!(wanted(&quiet, Kind::Attention));
        assert!(wanted(&quiet, Kind::Permission));
        assert!(!wanted(&quiet, Kind::Update));
    }

    #[test]
    fn the_command_gets_the_message_in_its_environment() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        let command = format!(
            "printf '%s|%s|%s|%s|%s' \"$CHIEF_KIND\" \"$CHIEF_TITLE\" \"$CHIEF_BODY\" \
             \"$CHIEF_PROJECT\" \"$CHIEF_URL\" > '{}'",
            out.display()
        );
        let config = config(Some(&command), PushOn::Attention);
        let message = Message {
            kind: Kind::Attention,
            title: "Flights".into(),
            // Nothing in the text is shell.
            body: "BCN-NYC is $412; `rm -rf` \"quoted\"".into(),
            url: Some("http://127.0.0.1:4747/p/home#/r/flights".into()),
        };
        assert!(send(&config, &message));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let text = loop {
            match std::fs::read_to_string(&out) {
                Ok(text) if text.matches('|').count() == 4 => break text,
                _ if std::time::Instant::now() > deadline => panic!("the command never ran"),
                _ => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        };
        assert_eq!(
            text,
            "attention|Flights|BCN-NYC is $412; `rm -rf` \"quoted\"|home|\
             http://127.0.0.1:4747/p/home#/r/flights"
        );
        // An update isn't pushed unless the settings ask for updates.
        let update = Message {
            kind: Kind::Update,
            ..message
        };
        assert!(!send(&config, &update));
    }
}
