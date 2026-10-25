//! `Mux` over the `rex` CLI. Use `--json` outputs; never touch sessions chief didn't create.

use std::path::Path;

use anyhow::Result;

use super::{BlockRef, BlockSpec, Mux, SessionRef};

pub struct Rex;

impl Mux for Rex {
    fn ensure_session(&self, label: &str, cwd: &Path) -> Result<SessionRef> {
        let _ = (label, cwd);
        todo!("M4")
    }

    fn run_block(&self, session: &SessionRef, spec: &BlockSpec) -> Result<BlockRef> {
        let _ = (session, spec);
        todo!("M4")
    }

    fn find_block(&self, session: &SessionRef, label: &str) -> Result<Option<BlockRef>> {
        let _ = (session, label);
        todo!("M4")
    }

    fn send_key(&self, block: &BlockRef, key: &str) -> Result<()> {
        let _ = (block, key);
        todo!("M4")
    }

    fn focus(&self, block: &BlockRef) -> Result<()> {
        let _ = block;
        todo!("M4")
    }

    fn kill_session(&self, session: &SessionRef) -> Result<()> {
        let _ = session;
        todo!("M4")
    }
}
