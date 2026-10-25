//! Terminal multiplexer access (ARCHITECTURE §9). Rex first; tmux can implement the same trait.

pub mod rex;

use std::path::{Path, PathBuf};

use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef(pub String);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRef(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Debug, Clone)]
pub struct BlockSpec {
    pub label: String,
    /// Place beside this block; None opens a new window.
    pub anchor: Option<(BlockRef, Split)>,
    /// Percent of the split given to the new block.
    pub ratio: u8,
    pub cwd: Option<PathBuf>,
    pub command: Vec<String>,
    pub focus: bool,
}

pub trait Mux {
    fn ensure_session(&self, label: &str, cwd: &Path) -> Result<SessionRef>;
    fn run_block(&self, session: &SessionRef, spec: &BlockSpec) -> Result<BlockRef>;
    fn find_block(&self, session: &SessionRef, label: &str) -> Result<Option<BlockRef>>;
    fn send_key(&self, block: &BlockRef, key: &str) -> Result<()>;
    fn focus(&self, block: &BlockRef) -> Result<()>;
    fn kill_session(&self, session: &SessionRef) -> Result<()>;
}
