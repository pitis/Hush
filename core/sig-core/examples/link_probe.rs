//! Prints core events while linking into a throwaway data dir: `cargo run --example link_probe`.
//! The link URL is shortened so the provisioning secret isn't printed.
use std::sync::Arc;
use std::time::Duration;

use sig_core::{Client, CoreEvent, EventListener};

struct Print;
impl EventListener for Print {
    fn on_event(&self, event: CoreEvent) {
        match event {
            CoreEvent::LinkUrl { url } => println!("LinkUrl({}…)", &url[..url.len().min(24)]),
            other => println!("{other:?}"),
        }
    }
}

fn main() {
    let dir = std::env::temp_dir().join(format!("hush-link-probe-{}", std::process::id()));
    let client = Client::open(dir.to_string_lossy().into(), "ab".repeat(32), Arc::new(Print)).expect("open");
    println!("linked: {}", client.is_linked());
    client.link("Hush probe".into()).expect("link");
    std::thread::sleep(Duration::from_secs(20));
    drop(client);
    let _ = std::fs::remove_dir_all(dir);
}
