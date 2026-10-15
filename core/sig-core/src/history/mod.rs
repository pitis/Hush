//! Importing the phone's message history at link time (link-and-sync).

/// The provisioning URL with `capabilities=backup5` appended, which makes the phone offer
/// "Transfer message history" when it scans the QR code.
pub(crate) fn with_backup_capability(url: &str) -> String {
    if url.contains('?') {
        format!("{url}&capabilities=backup5")
    } else {
        format!("{url}?capabilities=backup5")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_qr_url_asks_for_history() {
        assert_eq!(
            super::with_backup_capability("sgnl://linkdevice?uuid=u&pub_key=k"),
            "sgnl://linkdevice?uuid=u&pub_key=k&capabilities=backup5"
        );
        assert_eq!(
            super::with_backup_capability("sgnl://linkdevice"),
            "sgnl://linkdevice?capabilities=backup5"
        );
    }
}
