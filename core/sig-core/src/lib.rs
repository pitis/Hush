uniffi::setup_scaffolding!();

mod actor;
mod client;
mod error;
mod events;
pub mod projection;

pub use client::Client;
pub use error::CoreError;
pub use events::{ConnectionState, CoreEvent, EventListener};

/// Version of the Rust core, shown in the app's About panel.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_cargo() {
        assert_eq!(super::core_version(), env!("CARGO_PKG_VERSION"));
    }
}
