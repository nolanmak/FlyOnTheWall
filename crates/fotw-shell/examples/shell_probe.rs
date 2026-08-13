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
            println!();
            println!(
                "prompt_style_mask         0x{:04x}",
                probe.prompt_style_mask
            );
            println!(
                "prompt_is_nonactivating   {}   <-- the prompt fires mid-call",
                probe.prompt_is_nonactivating
            );
            println!(
                "prompt_can_become_key     {} (must be true)",
                probe.prompt_can_become_key
            );
            println!(
                "prompt_can_become_main    {} (must be false)",
                probe.prompt_can_become_main
            );
            println!(
                "prompt_level              {} (25 = NSStatusWindowLevel)",
                probe.prompt_level
            );
            println!(
                "prompt_sharing_type       {} (0 = NSWindowSharingNone)",
                probe.prompt_sharing_type
            );
            println!(
                "prompt_collection_behav.  0x{:04x}",
                probe.prompt_collection_behavior
            );
            println!(
                "prompt_start_disabled     {} (CON-05: all-party, box unticked)",
                probe.prompt_blocking_start_disabled
            );
            println!(
                "prompt_start_after_tick   {} (and reachable once ticked)",
                probe.prompt_acknowledged_start_enabled
            );
            println!(
                "prompt_content_fits       {} (the warning is not clipped)",
                probe.prompt_content_fits
            );
            println!(
                "prompt_is_on_a_screen     {} (ordered front onto a real display)",
                probe.prompt_is_on_a_screen
            );
            println!(
                "prompt_blocked_click      {} (a real click on the disabled Start does nothing)",
                probe.prompt_disabled_start_swallows_the_click
            );
            println!(
                "prompt_checkbox_click     {} (target/action reaches the panel)",
                probe.prompt_checkbox_dispatches
            );
            println!(
                "prompt_start_click        {} (and carries the acknowledgement)",
                probe.prompt_start_click_dispatches
            );
            println!(
                "prompt_dismissal_clicks   {} (Not now / Never dispatch as themselves)",
                probe.prompt_dismissals_dispatch
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
