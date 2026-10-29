//! The `chief up` layout over any `Mux` (ARCHITECTURE §9): session `chief-<slug>`, block `main`
//! running `chief pane`, block `board` split right at 32 % running `chief board`. chief only ever
//! reuses or kills the session it created itself (its id is stored in `ui_state.rex_session` or
//! `ui_state.tmux_window`), never one that merely carries the label.

use std::path::Path;

use anyhow::Result;

use crate::mux::{BlockRef, BlockSpec, Mux, SessionRef, Split};

pub const MAIN: &str = "main";
pub const BOARD: &str = "board";
pub const BOARD_RATIO: u8 = 32;

pub fn session_label(slug: &str) -> String {
    format!("chief-{slug}")
}

/// The board's block label when it runs beside the user's own block (`open_beside`): unlike
/// `board` in chief's own session, it must not clash with the user's blocks.
pub fn board_label(slug: &str) -> String {
    format!("chief-{slug} board")
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

/// What `close` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Closed {
    /// The session held only `main` and `board`, and was killed.
    Killed(SessionRef),
    /// The session holds other blocks too (the user's): only `main` and `board` were closed, and
    /// it stays open.
    Blocks(SessionRef),
    /// No session of chief's is open.
    Nothing,
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
        command: chief_command(exe, env, &["pane", "--project", slug, "--layout"]),
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

/// `ours` (the stored session id) while the multiplexer still has it.
fn live(mux: &dyn Mux, ours: Option<&SessionRef>) -> Result<Option<SessionRef>> {
    match ours {
        Some(s) if mux.has_session(s)? => Ok(Some(s.clone())),
        _ => Ok(None),
    }
}

/// Opens the layout in `ours`, the session `chief up` created before: reuses it when both blocks
/// run and replaces a missing or ended block. When neither runs, `main` starts in a new window
/// of the same session before the dead blocks close (closing the last block could end the
/// session, and the session may hold the user's blocks, so it is never killed). Without a live
/// `ours` it creates `chief-<slug>`; a session that only carries that label isn't chief's and is
/// left alone. Focuses `main` and asks the multiplexer to show the session; the caller stores the
/// returned session's id as the new `ours`.
pub fn open(
    mux: &dyn Mux,
    slug: &str,
    dir: &Path,
    exe: &Path,
    env: &[(String, String)],
    ours: Option<&SessionRef>,
) -> Result<Layout> {
    let main = main_spec(slug, dir, exe, env);
    let board = board_spec(slug, dir, exe, env);
    let beside = |anchor: &BlockRef, split: Split, spec: &BlockSpec, ratio: u8| BlockSpec {
        anchor: Some((anchor.clone(), split)),
        ratio,
        ..spec.clone()
    };

    let (session, main_block, opened) = match live(mux, ours)? {
        None => {
            let (s, m) = mux.create_session(&session_label(slug), &main)?;
            mux.run_block(&s, &beside(&m, Split::Right, &board, BOARD_RATIO))?;
            (s, m, Opened::Created)
        }
        Some(s) => match (block_state(mux, &s, MAIN)?, block_state(mux, &s, BOARD)?) {
            (Some((m, true)), Some((_, true))) => (s, m, Opened::Reused),
            (Some((m, true)), b) => {
                if let Some((dead, _)) = b {
                    mux.close_block(&dead)?;
                }
                mux.run_block(&s, &beside(&m, Split::Right, &board, BOARD_RATIO))?;
                (s, m, Opened::Repaired)
            }
            (m, Some((b, true))) => {
                if let Some((dead, _)) = m {
                    mux.close_block(&dead)?;
                }
                let m = mux.run_block(&s, &beside(&b, Split::Left, &main, 100 - BOARD_RATIO))?;
                (s, m, Opened::Repaired)
            }
            (m, b) => {
                let new_main = mux.run_block(&s, &main)?;
                for (dead, _) in [m, b].into_iter().flatten() {
                    mux.close_block(&dead)?;
                }
                mux.run_block(&s, &beside(&new_main, Split::Right, &board, BOARD_RATIO))?;
                (s, new_main, Opened::Repaired)
            }
        },
    };
    // Cosmetic, and a client action Rex refuses when several apps are connected: the layout is
    // there either way, and failing here would lose the session's id.
    let _ = mux.focus(&main_block);
    let shown = mux.show(&session)?;
    Ok(Layout {
        session,
        opened,
        shown,
    })
}

/// Inside a Rex block (`here`, in the user's `session`): the board split right of it, so the
/// block itself shows the conversation and nothing opens elsewhere (§9). A board chief started
/// beside a block of this session before is reused while it runs, and replaced once ended. Focus
/// stays where it is.
pub fn open_beside(
    mux: &dyn Mux,
    slug: &str,
    (dir, exe, env): (&Path, &Path, &[(String, String)]),
    session: &SessionRef,
    here: &BlockRef,
) -> Result<(BlockRef, Opened)> {
    let label = board_label(slug);
    let mut opened = Opened::Created;
    if let Some((block, running)) = block_state(mux, session, &label)? {
        if running {
            return Ok((block, Opened::Reused));
        }
        mux.close_block(&block)?;
        opened = Opened::Repaired;
    }
    let spec = BlockSpec {
        label,
        anchor: Some((here.clone(), Split::Right)),
        focus: false,
        ..board_spec(slug, dir, exe, env)
    };
    Ok((mux.run_block(session, &spec)?, opened))
}

/// Closes the project's layout (`ours`): kills the session when it holds only `main` and
/// `board`, otherwise closes just those two blocks so the user's blocks stay.
pub fn close(mux: &dyn Mux, ours: Option<&SessionRef>) -> Result<Closed> {
    let Some(s) = live(mux, ours)? else {
        return Ok(Closed::Nothing);
    };
    let blocks = mux.blocks(&s)?;
    let chiefs = |label: &str| label == MAIN || label == BOARD;
    if blocks.iter().all(|b| chiefs(&b.label)) {
        mux.kill_session(&s)?;
        return Ok(Closed::Killed(s));
    }
    for b in blocks.iter().filter(|b| chiefs(&b.label)) {
        mux.close_block(&b.block)?;
    }
    Ok(Closed::Blocks(s))
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

    /// The id `FakeMux::with_session` gives its session.
    pub fn s0() -> SessionRef {
        SessionRef("s0".into())
    }

    impl Mux for FakeMux {
        fn has_session(&self, session: &SessionRef) -> Result<bool> {
            Ok(self.sessions.borrow().iter().any(|s| s.0 == session.0))
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

    use super::testing::{Call, FakeMux, s0};
    use super::*;

    const EXE: &str = "/bin/chief";
    const DIR: &str = "/p/demo";

    /// `open` with `s0` stored as chief's session.
    fn open_demo(mux: &FakeMux) -> Layout {
        open_in(mux, Some(&s0()))
    }

    /// Inside the user's own Rex block b0: the board goes beside it, once.
    #[test]
    fn beside_the_users_block_the_board_is_split_off_once() {
        let mux = FakeMux::with_session("bold-alcove", &[("shell", true), ("logs", true)]);
        let env = vec![("CHIEF_HOME".to_string(), "/h".to_string())];
        let paths = (Path::new(DIR), Path::new(EXE), env.as_slice());
        let here = BlockRef("b0".into());
        let (board, opened) = open_beside(&mux, "demo", paths, &s0(), &here).unwrap();
        assert_eq!(opened, Opened::Created);
        let spec = BlockSpec {
            label: "chief-demo board".into(),
            anchor: Some((here.clone(), Split::Right)),
            ratio: 32,
            cwd: Some(PathBuf::from(DIR)),
            command: cmd("board"),
            focus: false,
        };
        assert_eq!(mux.calls(), [Call::Run("s0".into(), spec)]);

        let (again, opened) = open_beside(&mux, "demo", paths, &s0(), &here).unwrap();
        assert_eq!((again, opened), (board.clone(), Opened::Reused));
        assert_eq!(mux.calls().len(), 1, "nothing new");

        // Ended (its block kept open): replaced.
        for s in mux.sessions.borrow_mut().iter_mut() {
            for b in s.2.iter_mut().filter(|b| b.0 == board.0) {
                b.2 = false;
            }
        }
        let (_, opened) = open_beside(&mux, "demo", paths, &s0(), &here).unwrap();
        assert_eq!(opened, Opened::Repaired);
        assert_eq!(mux.calls()[1], Call::Close(board.0.clone()));
        assert!(matches!(&mux.calls()[2], Call::Run(..)));
        assert!(
            !mux.calls()
                .iter()
                .any(|c| matches!(c, Call::Focus(_) | Call::Show(_))),
            "focus stays put"
        );
    }

    fn open_in(mux: &FakeMux, ours: Option<&SessionRef>) -> Layout {
        let env = vec![("CHIEF_HOME".to_string(), "/h".to_string())];
        open(mux, "demo", Path::new(DIR), Path::new(EXE), &env, ours).unwrap()
    }

    fn cmd(sub: &str) -> Vec<String> {
        ["env", "CHIEF_HOME=/h", EXE, sub, "--project", "demo"]
            .map(String::from)
            .to_vec()
    }

    fn pane_cmd() -> Vec<String> {
        let mut cmd = cmd("pane");
        cmd.push("--layout".into());
        cmd
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
            command: pane_cmd(),
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

    /// The session may hold the user's blocks: never killed. `main` starts in a new window first
    /// so closing the dead blocks can't end the session.
    #[test]
    fn rebuilds_inside_the_session_when_neither_block_runs() {
        let mux = FakeMux::with_session(
            "chief-demo",
            &[("main", false), ("shell", true), ("board", false)],
        );
        assert_eq!(open_demo(&mux).opened, Opened::Repaired);
        let calls = mux.calls();
        let Call::Run(s, main) = &calls[0] else {
            panic!("{calls:?}")
        };
        assert_eq!(
            (s.as_str(), main.label.as_str(), main.anchor.clone()),
            ("s0", "main", None)
        );
        assert_eq!(
            calls[1..3],
            [Call::Close("b0".into()), Call::Close("b2".into())]
        );
        let Call::Run(_, board) = &calls[3] else {
            panic!("{calls:?}")
        };
        assert_eq!(
            (board.label.as_str(), board.anchor.clone()),
            ("board", Some((BlockRef("b-new1".into()), Split::Right)))
        );
        assert_eq!(calls[4], Call::Focus("b-new1".into()));
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, Call::Kill(_) | Call::Create(..)))
        );
        let blocks = mux.sessions.borrow()[0].2.clone();
        let labels: Vec<_> = blocks.iter().map(|b| b.1.as_str()).collect();
        assert_eq!(labels, ["shell", "main", "board"]);

        // Nothing of chief's left at all: the same.
        let mux = FakeMux::with_session("chief-demo", &[("shell", true)]);
        assert_eq!(open_demo(&mux).opened, Opened::Repaired);
        assert!(!mux.calls().iter().any(|c| matches!(c, Call::Kill(_))));
    }

    /// Only the stored id is chief's: a session that merely carries the label, or chief's own
    /// after it was killed, is never reused or killed.
    #[test]
    fn only_the_session_chief_created_is_touched() {
        let theirs = FakeMux::with_session("chief-demo", &[("main", true), ("board", true)]);
        let layout = open_in(&theirs, None);
        assert_eq!(
            (layout.opened, layout.session.clone()),
            (Opened::Created, SessionRef("s-new1".into()))
        );
        assert_eq!(close(&theirs, None).unwrap(), Closed::Nothing);
        let gone = SessionRef("s-gone".into());
        assert_eq!(open_in(&theirs, Some(&gone)).opened, Opened::Created);
        assert_eq!(close(&theirs, Some(&gone)).unwrap(), Closed::Nothing);
        assert!(!theirs.calls().iter().any(|c| matches!(c, Call::Kill(_))));
        let untouched = theirs.sessions.borrow()[0].clone();
        assert_eq!((untouched.0.as_str(), untouched.2.len()), ("s0", 2));

        // chief's own, holding only its blocks: killed.
        assert_eq!(
            close(&theirs, Some(&layout.session)).unwrap(),
            Closed::Killed(layout.session.clone())
        );
        assert_eq!(theirs.calls().last(), Some(&Call::Kill("s-new1".into())));
    }

    /// The user added a block to chief's session: `chief down` closes only chief's two.
    #[test]
    fn close_keeps_a_session_holding_the_users_blocks() {
        let mux = FakeMux::with_session(
            "chief-demo",
            &[("main", true), ("logs", true), ("board", true)],
        );
        assert_eq!(close(&mux, Some(&s0())).unwrap(), Closed::Blocks(s0()));
        assert_eq!(
            mux.calls(),
            [Call::Close("b0".into()), Call::Close("b2".into())]
        );
        assert_eq!(mux.find_block(&s0(), MAIN).unwrap(), None);
    }

    #[test]
    fn no_env_runs_chief_directly() {
        assert_eq!(
            chief_command(Path::new(EXE), &[], &["pane", "--project", "x"]),
            [EXE, "pane", "--project", "x"]
        );
    }
}
