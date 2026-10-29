//! A terminal inside chief's own screen, for the built-in layout (ARCHITECTURE §9): a child
//! program on its own pseudo-terminal, emulated by alacritty_terminal and drawn into a ratatui
//! buffer; or relayed to the user's terminal as it is (`Pty::open`, the pane).

mod pty;
mod screen;

pub use pty::{Output, Pty};
pub use screen::Screen;
