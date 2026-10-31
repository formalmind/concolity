//! sol-debug: a local Solidity debugger. solc's source maps tie every instruction to the AST node it came from (and
//! back: every AST node to its instructions); revm runs the code with an inspector that records each step; the
//! storage layout and the recorded keccak preimages name every storage slot a step touches. See desktop/DEBUGGER.md.

pub mod agent;
pub mod fork;
pub mod cheats;
pub mod diff;
pub mod highlight;
pub mod layout;
pub mod nav;
pub mod session;
pub mod slots;
pub mod solc;
pub mod source;
pub mod srcmap;
pub mod statediff;
pub mod theme;
pub mod trace;
pub mod tui;

pub use session::{Run, Session, StorageWrite, TestRun, SENDER};
