//! Terminal multiplexer access (ARCHITECTURE §9): Rex, and tmux behind the same trait.

pub mod rex;
pub mod tmux;

use std::path::PathBuf;

use anyhow::Result;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRef(pub String);

/// A block's full id, unique across sessions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRef(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    Left,
    Right,
    Above,
    Below,
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

/// A block and its label, as `blocks` lists them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockInfo {
    pub block: BlockRef,
    pub label: String,
}

pub trait Mux {
    /// Whether the session still exists. Errors when the multiplexer isn't installed or isn't
    /// running. Sessions are found by id, never by label: a label proves nothing about who
    /// created the session.
    fn has_session(&self, session: &SessionRef) -> Result<bool>;
    /// Creates a session labelled `label` whose first block is `first` (its anchor is ignored).
    fn create_session(&self, label: &str, first: &BlockSpec) -> Result<(SessionRef, BlockRef)>;
    fn run_block(&self, session: &SessionRef, spec: &BlockSpec) -> Result<BlockRef>;
    fn blocks(&self, session: &SessionRef) -> Result<Vec<BlockInfo>>;
    /// Whether the block's program still runs (a kept-open block outlives it).
    fn is_running(&self, block: &BlockRef) -> Result<bool>;
    fn close_block(&self, block: &BlockRef) -> Result<()>;
    fn focus(&self, block: &BlockRef) -> Result<()>;
    /// Brings the session into the user's view. Ok(false) when the multiplexer can't (no app
    /// connected, or several to choose from).
    fn show(&self, session: &SessionRef) -> Result<bool>;
    fn kill_session(&self, session: &SessionRef) -> Result<()>;

    fn find_block(&self, session: &SessionRef, label: &str) -> Result<Option<BlockRef>> {
        Ok(self
            .blocks(session)?
            .into_iter()
            .find(|b| b.label == label)
            .map(|b| b.block))
    }
}
