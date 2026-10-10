//! The `p1 acp` driver: the stdio session loop over the front-end port (D7).

mod front_end;
mod hold;
pub mod io;
mod session;

pub use front_end::{AcpFrontEnd, Reader, Writer};
