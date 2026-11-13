//! The iCloud protocol.
//!
//! The modules are layered so each one can be tested on its own:
//!
//! * [`transport`] — the HTTP boundary and the cookie jar
//! * [`srp`] — Apple's SRP variant
//! * [`auth`] — the sign-in flow and the session
//! * [`drive`] — the drive service
//! * [`client`] — the adapter that implements the client interface

pub mod auth;
pub mod client;
pub mod drive;
pub mod error;
pub mod srp;
pub mod transport;

pub use client::ICloudApi;
pub use error::{Error, Result};
