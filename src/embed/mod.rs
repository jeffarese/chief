//! A terminal inside chief's own screen, for the built-in layout (ARCHITECTURE §9): a child
//! program on its own pseudo-terminal, emulated by libghostty and drawn into a ratatui buffer.

mod pty;
mod screen;

pub use pty::{Output, Pty};
pub use screen::Screen;
