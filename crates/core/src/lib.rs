//! UI-independent logic: settings now, scanners/models later. Keep Slint out of this crate so a CLI can reuse it.
pub mod gpu;
pub mod services;
pub mod settings;
pub mod sys;
pub mod tree;
