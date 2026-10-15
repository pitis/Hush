uniffi::setup_scaffolding!();

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
