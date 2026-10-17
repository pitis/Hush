uniffi::setup_scaffolding!();

mod actor;
mod calls;
mod client;
mod download;
mod error;
mod events;
mod history;
mod link_preview;
mod logging;
mod net;
pub mod projection;
mod send;
mod stickers;
mod storage;
mod typing;

pub use calls::{AudioDevice, AudioDevices, CallPhase};
pub use client::Client;
pub use error::CoreError;
pub use events::{ConnectionState, CoreEvent, EventListener};
pub use history::HistoryStage;

/// Version of the Rust core, shown in the app's About panel.
#[uniffi::export]
pub fn core_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Signal's bubble colour presets, for the chat colour picker.
#[uniffi::export]
pub fn chat_color_presets() -> Vec<projection::ChatColor> {
    projection::chat_colors::presets()
}

/// Signal's wallpaper presets, for the picker.
#[uniffi::export]
pub fn wallpaper_presets() -> Vec<projection::ChatColor> {
    projection::chat_colors::wallpapers()
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_cargo() {
        assert_eq!(super::core_version(), env!("CARGO_PKG_VERSION"));
    }
}
