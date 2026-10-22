//! `uring-http`: a minimal HTTP/1.1 server driven directly by `io_uring`.
//!
//! One thread, one ring, no async runtime. Every socket operation (accept,
//! recv, send, close) is submitted to the ring and completed from the CQ.

pub mod http;
pub mod server;

pub use http::{Limits, ParseError, Request, Response};
pub use server::{Config, Server, ShutdownHandle, Stats};
