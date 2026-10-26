//! The `chief up` layout over any `Mux` (ARCHITECTURE §9): session `chief-<slug>`, block `main`
//! running `chief pane`, block `board` split right at 32 % running `chief board`.

use std::path::Path;

use anyhow::Result;

use crate::mux::{BlockRef, BlockSpec, Mux, SessionRef, Split};

pub const MAIN: &str = "main";
pub const BOARD: &str = "board";
pub const BOARD_RATIO: u8 = 32;

pub fn session_label(slug: &str) -> String {
    format!("chief-{slug}")
}

/// What `open` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    Created,
    /// The session was there, but a block was missing or its program had ended.
    Repaired,
    Reused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub session: SessionRef,
    pub opened: Opened,
    /// Whether the multiplexer brought it into the user's view.
    pub shown: bool,
}

/// `chief <args>` for a block: through `env` with the passthrough variables, because a block
/// starts from the login shell's environment, not ours.
pub fn chief_command(exe: &Path, env: &[(String, String)], args: &[&str]) -> Vec<String> {
    let mut cmd = Vec::new();
    if !env.is_empty() {
        cmd.push("env".to_string());
        cmd.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    }
    cmd.push(exe.to_string_lossy().into_owned());
    cmd.extend(args.iter().map(|a| a.to_string()));
    cmd
}

fn main_spec(slug: &str, dir: &Path, exe: &Path, env: &[(String, String)]) -> BlockSpec {
    BlockSpec {
        label: MAIN.to_string(),
        anchor: None,
        ratio: 100,
        cwd: Some(dir.to_path_buf()),
        command: chief_command(exe, env, &["pane", "--project", slug]),
        focus: true,
    }
}

fn board_spec(slug: &str, dir: &Path, exe: &Path, env: &[(String, String)]) -> BlockSpec {
    BlockSpec {
        label: BOARD.to_string(),
        anchor: None,
        ratio: BOARD_RATIO,
        cwd: Some(dir.to_path_buf()),
        command: chief_command(exe, env, &["board", "--project", slug]),
        focus: false,
    }
}

/// The block labelled `label` whether or not its program runs, and whether it does.
fn block_state(mux: &dyn Mux, s: &SessionRef, label: &str) -> Result<Option<(BlockRef, bool)>> {
    match mux.find_block(s, label)? {
        Some(b) => {
            let running = mux.is_running(&b)?;
            Ok(Some((b, running)))
        }
        None => Ok(None),
    }
}

/// Opens the layout: reuses `chief-<slug>` when both blocks run, replaces a missing or ended
/// block, and creates the session when there is none (or neither block is left). Focuses `main`
/// and asks the multiplexer to show the session.
pub fn open(
    mux: &dyn Mux,
    slug: &str,
    dir: &Path,
    exe: &Path,
    env: &[(String, String)],
) -> Result<Layout> {
    let label = session_label(slug);
    let main = main_spec(slug, dir, exe, env);
    let board = board_spec(slug, dir, exe, env);
    let beside = |anchor: &BlockRef, split: Split, spec: &BlockSpec, ratio: u8| BlockSpec {
        anchor: Some((anchor.clone(), split)),
        ratio,
        ..spec.clone()
    };

    let existing = match mux.find_session(&label)? {
        Some(s) => {
            let m = block_state(mux, &s, MAIN)?;
            let b = block_state(mux, &s, BOARD)?;
            Some((s, m, b))
        }
        None => None,
    };
    let (session, main_block, opened) = match existing {
        Some((s, Some((m, true)), Some((_, true)))) => (s, m, Opened::Reused),
        Some((s, Some((m, true)), b)) => {
            if let Some((dead, _)) = b {
                mux.close_block(&dead)?;
            }
            mux.run_block(&s, &beside(&m, Split::Right, &board, BOARD_RATIO))?;
            (s, m, Opened::Repaired)
        }
        Some((s, m, Some((b, true)))) => {
            if let Some((dead, _)) = m {
                mux.close_block(&dead)?;
            }
            let m = mux.run_block(&s, &beside(&b, Split::Left, &main, 100 - BOARD_RATIO))?;
            (s, m, Opened::Repaired)
        }
        other => {
            // Nothing of ours runs there; closing the last block could end the session under
            // us, so start over.
            if let Some((s, _, _)) = other {
                mux.kill_session(&s)?;
            }
            let (s, m) = mux.create_session(&label, &main)?;
            mux.run_block(&s, &beside(&m, Split::Right, &board, BOARD_RATIO))?;
            (s, m, Opened::Created)
        }
    };
    mux.focus(&main_block)?;
    let shown = mux.show(&session)?;
    Ok(Layout {
        session,
        opened,
        shown,
    })
}

/// The `main` block of the project's layout, if the layout is open.
pub fn main_block(mux: &dyn Mux, slug: &str) -> Result<Option<BlockRef>> {
    match mux.find_session(&session_label(slug))? {
        Some(s) => mux.find_block(&s, MAIN),
        None => Ok(None),
    }
}

/// Kills the project's layout session; returns it, or None when there was none.
pub fn close(mux: &dyn Mux, slug: &str) -> Result<Option<SessionRef>> {
    let Some(s) = mux.find_session(&session_label(slug))? else {
        return Ok(None);
    };
    mux.kill_session(&s)?;
    Ok(Some(s))
}

#[cfg(test)]
pub mod testing {
    //! A `Mux` that records calls, for layout and pane tests.

    use std::cell::RefCell;

    use super::*;
    use crate::mux::BlockInfo;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        Create(String, BlockSpec),
        Run(String, BlockSpec),
        Close(String),
        Key(String, String),
        Focus(String),
        Show(String),
        Kill(String),
    }

    /// (block id, label, running)
    pub type FakeBlock = (String, String, bool);

    #[derive(Default)]
    pub struct FakeMux {
        /// (session id, label, blocks)
        pub sessions: RefCell<Vec<(String, String, Vec<FakeBlock>)>>,
        pub calls: RefCell<Vec<Call>>,
        pub can_show: bool,
        pub next: RefCell<usize>,
    }

    impl FakeMux {
        pub fn with_session(label: &str, blocks: &[(&str, bool)]) -> Self {
            let mux = FakeMux::default();
            let blocks = blocks
                .iter()
                .enumerate()
                .map(|(i, (l, r))| (format!("b{i}"), l.to_string(), *r))
                .collect();
            mux.sessions
                .borrow_mut()
                .push(("s0".into(), label.into(), blocks));
            mux
        }

        fn id(&self, prefix: &str) -> String {
            let mut n = self.next.borrow_mut();
            *n += 1;
            format!("{prefix}-new{n}")
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }
    }

    impl Mux for FakeMux {
        fn find_session(&self, label: &str) -> Result<Option<SessionRef>> {
            Ok(self
                .sessions
                .borrow()
                .iter()
                .find(|s| s.1 == label)
                .map(|s| SessionRef(s.0.clone())))
        }

        fn create_session(&self, label: &str, first: &BlockSpec) -> Result<(SessionRef, BlockRef)> {
            let (s, b) = (self.id("s"), self.id("b"));
            self.calls
                .borrow_mut()
                .push(Call::Create(label.into(), first.clone()));
            self.sessions.borrow_mut().push((
                s.clone(),
                label.into(),
                vec![(b.clone(), first.label.clone(), true)],
            ));
            Ok((SessionRef(s), BlockRef(b)))
        }

        fn run_block(&self, session: &SessionRef, spec: &BlockSpec) -> Result<BlockRef> {
            let b = self.id("b");
            self.calls
                .borrow_mut()
                .push(Call::Run(session.0.clone(), spec.clone()));
            if let Some(s) = self
                .sessions
                .borrow_mut()
                .iter_mut()
                .find(|s| s.0 == session.0)
            {
                s.2.push((b.clone(), spec.label.clone(), true));
            }
            Ok(BlockRef(b))
        }

        fn blocks(&self, session: &SessionRef) -> Result<Vec<BlockInfo>> {
            Ok(self
                .sessions
                .borrow()
                .iter()
                .filter(|s| s.0 == session.0)
                .flat_map(|s| s.2.iter())
                .map(|(id, label, _)| BlockInfo {
                    block: BlockRef(id.clone()),
                    label: label.clone(),
                })
                .collect())
        }

        fn is_running(&self, block: &BlockRef) -> Result<bool> {
            Ok(self
                .sessions
                .borrow()
                .iter()
                .flat_map(|s| s.2.iter())
                .any(|b| b.0 == block.0 && b.2))
        }

        fn close_block(&self, block: &BlockRef) -> Result<()> {
            self.calls.borrow_mut().push(Call::Close(block.0.clone()));
            for s in self.sessions.borrow_mut().iter_mut() {
                s.2.retain(|b| b.0 != block.0);
            }
            Ok(())
        }

        fn send_key(&self, block: &BlockRef, key: &str) -> Result<()> {
            self.calls
                .borrow_mut()
                .push(Call::Key(block.0.clone(), key.into()));
            Ok(())
        }

        fn focus(&self, block: &BlockRef) -> Result<()> {
            self.calls.borrow_mut().push(Call::Focus(block.0.clone()));
            Ok(())
        }

        fn show(&self, session: &SessionRef) -> Result<bool> {
            self.calls.borrow_mut().push(Call::Show(session.0.clone()));
            Ok(self.can_show)
        }

        fn kill_session(&self, session: &SessionRef) -> Result<()> {
            self.calls.borrow_mut().push(Call::Kill(session.0.clone()));
            self.sessions.borrow_mut().retain(|s| s.0 != session.0);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::testing::{Call, FakeMux};
    use super::*;

    const EXE: &str = "/bin/chief";
    const DIR: &str = "/p/demo";

    fn open_demo(mux: &FakeMux) -> Layout {
        let env = vec![("CHIEF_HOME".to_string(), "/h".to_string())];
        open(mux, "demo", Path::new(DIR), Path::new(EXE), &env).unwrap()
    }

    fn cmd(sub: &str) -> Vec<String> {
        ["env", "CHIEF_HOME=/h", EXE, sub, "--project", "demo"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn creates_main_then_board_beside_it() {
        let mux = FakeMux {
            can_show: true,
            ..FakeMux::default()
        };
        let layout = open_demo(&mux);
        assert_eq!(layout.opened, Opened::Created);
        assert!(layout.shown);
        let main = BlockSpec {
            label: "main".into(),
            anchor: None,
            ratio: 100,
            cwd: Some(PathBuf::from(DIR)),
            command: cmd("pane"),
            focus: true,
        };
        let board = BlockSpec {
            label: "board".into(),
            anchor: Some((BlockRef("b-new2".into()), Split::Right)),
            ratio: 32,
            cwd: Some(PathBuf::from(DIR)),
            command: cmd("board"),
            focus: false,
        };
        assert_eq!(
            mux.calls(),
            [
                Call::Create("chief-demo".into(), main),
                Call::Run("s-new1".into(), board),
                Call::Focus("b-new2".into()),
                Call::Show("s-new1".into()),
            ]
        );
    }

    #[test]
    fn reuses_a_running_layout() {
        let mux = FakeMux::with_session("chief-demo", &[("main", true), ("board", true)]);
        let layout = open_demo(&mux);
        assert_eq!(layout.opened, Opened::Reused);
        assert!(!layout.shown);
        assert_eq!(
            mux.calls(),
            [Call::Focus("b0".into()), Call::Show("s0".into())]
        );
    }

    #[test]
    fn replaces_an_ended_or_missing_block() {
        let mux = FakeMux::with_session("chief-demo", &[("main", true), ("board", false)]);
        assert_eq!(open_demo(&mux).opened, Opened::Repaired);
        let calls = mux.calls();
        assert_eq!(calls[0], Call::Close("b1".into()));
        let Call::Run(s, spec) = &calls[1] else {
            panic!("{calls:?}")
        };
        assert_eq!(s, "s0");
        assert_eq!(spec.label, "board");
        assert_eq!(spec.anchor, Some((BlockRef("b0".into()), Split::Right)));

        // Board alone: main goes back on its left with the rest of the width.
        let mux = FakeMux::with_session("chief-demo", &[("board", true)]);
        assert_eq!(open_demo(&mux).opened, Opened::Repaired);
        let calls = mux.calls();
        let Call::Run(_, spec) = &calls[0] else {
            panic!("{calls:?}")
        };
        assert_eq!(
            (spec.label.as_str(), spec.anchor.clone(), spec.ratio),
            ("main", Some((BlockRef("b0".into()), Split::Left)), 68)
        );
        assert_eq!(calls[1], Call::Focus("b-new1".into()));
    }

    #[test]
    fn starts_over_when_neither_block_runs() {
        let mux = FakeMux::with_session("chief-demo", &[("main", false), ("shell", true)]);
        assert_eq!(open_demo(&mux).opened, Opened::Created);
        let calls = mux.calls();
        assert_eq!(calls[0], Call::Kill("s0".into()));
        assert!(matches!(&calls[1], Call::Create(l, _) if l == "chief-demo"));
    }

    #[test]
    fn other_sessions_are_left_alone() {
        let mux = FakeMux::with_session("chief-demo-2", &[("main", true), ("board", true)]);
        assert_eq!(open_demo(&mux).opened, Opened::Created);
        assert_eq!(close(&mux, "other").unwrap(), None);
        assert!(!mux.calls().iter().any(|c| matches!(c, Call::Kill(_))));
        assert_eq!(
            close(&mux, "demo").unwrap(),
            Some(SessionRef("s-new1".into()))
        );
        assert_eq!(mux.calls().last(), Some(&Call::Kill("s-new1".into())));
    }

    #[test]
    fn no_env_runs_chief_directly() {
        assert_eq!(
            chief_command(Path::new(EXE), &[], &["pane", "--project", "x"]),
            [EXE, "pane", "--project", "x"]
        );
    }
}
