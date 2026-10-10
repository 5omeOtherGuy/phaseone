//! Transport-free ACP v1 mapping. The host owns sessions, I/O and grant storage.

pub mod capabilities;
pub mod codec;
pub mod driver;
pub mod policy;
pub mod sink;
pub mod turn;

mod wire;
