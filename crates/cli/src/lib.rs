mod args;
pub mod commands;
mod run;
pub mod wire;

pub use args::parse;
pub use run::{local, not_running, run};
