//! Fixture crate: a small multi-file Rust crate used by the integration
//! suite. Contents mirror a real dependency chain plus files that must be
//! excluded, unsupported, or test-associated.

pub mod util;

pub fn run_app() -> u8 {
    util::read_config()
}
