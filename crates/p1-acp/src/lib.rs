//! The ACP v1 agent side of p1: the mapping, the single-session driver and the
//! router of several sessions. The host owns the agents and starts the session processes.

pub mod capabilities;
pub mod codec;
pub mod commands;
pub mod config_options;
pub mod driver;
pub mod plan;
pub mod policy;
pub mod router;
pub mod sink;
pub mod turn;
pub mod usage;
pub mod workflow_card;

mod wire;
