//! Transport for Kitten Migrate: the [`Link`] trait the sender and receiver talk over, and the
//! in-memory [`FaultyLink`] that stands in for it in tests, able to drop, duplicate and cut frames
//! and reconnect — the faults a TCP link suffers.
//!
//! SECURITY: the current transport (`tcp.rs`) is plaintext TCP with NO authentication or
//! encryption. Mutual authentication and TLS are not implemented yet and are required before
//! migration ships (see the security audit). An authenticated/encrypted `Link` plugs into this
//! same trait, so tests written against `FaultyLink` still hold.

pub mod link;
pub mod tcp;

pub use link::{Control, Endpoint, Fault, FaultyLink, Frame, Link, LinkError, Side};
pub use tcp::TcpLink;
