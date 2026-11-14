pub mod catalog;
/// The simulator lives in its own crate, which hosts can call; this keeps
/// the CLI's former module path.
pub use aq_playbooks_simulation as simulation;

pub mod configuration;
