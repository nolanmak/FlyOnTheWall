//! Bring the shell's three surfaces up, print what AppKit made of them, exit.
//!
//! This is the check that cannot be a test: it needs a window server and a
//! menu bar. Run it on a real desktop.
//!
//! ```sh
//! cargo run -p fotw-shell --example shell_probe
//! ```
//!
//! The line that matters is `panel_is_nonactivating`. It is read back from the
//! live `NSPanel`, not echoed from the constant, so it is the only evidence
//! that AppKit kept the mask through initialization (FB16484811).

fn main() {
    match fotw_shell::platform::probe() {
        Ok(probe) => {
            println!(
                "activation_policy         {} (1 = Accessory)",
                probe.activation_policy
            );
            println!("status_item_retained      {}", probe.status_item_retained);
            println!("hotkeys_registered        {}", probe.hotkeys_registered);
            println!("panel_style_mask          0x{:04x}", probe.panel_style_mask);
            println!(
                "panel_is_nonactivating    {}   <-- the whole ballgame",
                probe.panel_is_nonactivating
            );
            println!(
                "panel_can_become_key      {} (must be true)",
                probe.panel_can_become_key
            );
            println!(
                "panel_can_become_main     {} (must be false)",
                probe.panel_can_become_main
            );
            println!(
                "panel_level               {} (25 = NSStatusWindowLevel)",
                probe.panel_level
            );
            println!(
                "panel_sharing_type        {} (0 = NSWindowSharingNone)",
                probe.panel_sharing_type
            );
            println!(
                "panel_collection_behavior 0x{:04x}",
                probe.panel_collection_behavior
            );
            println!("\nhealthy: {}", probe.is_healthy());
            if !probe.is_healthy() {
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("shell probe failed: {e}");
            std::process::exit(1);
        }
    }
}
