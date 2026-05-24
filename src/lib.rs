//! `remo-broker` library — on-instance credential broker for Remo.
//!
//! The binary in `src/main.rs` is a thin shim over this library; everything of
//! substance lives here so it can be unit-tested without going through the
//! binary's CLI surface.

pub mod manifest;
