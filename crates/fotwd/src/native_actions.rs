//! Menu work runs away from the AppKit thread and never logs handoff URLs.
use crate::native_host::NativeAction;
use crate::session::StopSignal;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError};
use std::time::Duration;

pub(crate) async fn run(
    requests: Receiver<NativeAction>,
    feedback: SyncSender<String>,
    state: fotw_web::AppState,
    root: PathBuf,
    stop: StopSignal,
) {
    while !stop.is_stopped() {
        match requests.try_recv() {
            Ok(action) => {
                if let Err(error) = open_action(action, &state, &root).await {
                    let _ = feedback.try_send(error);
                }
            }
            Err(TryRecvError::Empty) => tokio::time::sleep(Duration::from_millis(25)).await,
            Err(TryRecvError::Disconnected) => break,
        }
    }
}
async fn open_action(
    action: NativeAction,
    state: &fotw_web::AppState,
    root: &std::path::Path,
) -> Result<(), String> {
    let argument = match action {
        NativeAction::OpenDashboard => state.launch_url(),
        NativeAction::OpenSettings => format!("{}#speech-settings", state.launch_url()),
        NativeAction::OpenDisclosure | NativeAction::OpenAbout => {
            let content = info_page(action);
            let folder = root.parent().unwrap_or(root).join("native-info");
            std::fs::create_dir_all(&folder)
                .map_err(|_| "could not prepare application information".to_owned())?;
            let name = if action == NativeAction::OpenDisclosure {
                "disclosure.html"
            } else {
                "about.html"
            };
            let destination = folder.join(name);
            let mut file = tempfile::NamedTempFile::new_in(&folder)
                .map_err(|_| "could not prepare application information".to_owned())?;
            std::io::Write::write_all(&mut file, content.as_bytes())
                .map_err(|_| "could not write application information".to_owned())?;
            file.persist(&destination)
                .map_err(|_| "could not save application information".to_owned())?;
            destination.to_string_lossy().into_owned()
        }
    };
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::process::Command::new("open")
            .arg(argument)
            .kill_on_drop(true)
            .status(),
    )
    .await;
    if matches!(result, Ok(Ok(status)) if status.success()) {
        Ok(())
    } else {
        Err("could not open this view; open the dashboard or try again".into())
    }
}
fn info_page(action: NativeAction) -> String {
    let (title, text) = if action == NativeAction::OpenDisclosure {
        let kit = crate::consent::DisclosureKit::default();
        (
            "Disclosure Kit",
            format!(
                "Meeting chat\n{}\n\nCalendar invite\n{}\n\nSay aloud\n{}",
                kit.chat_notice(),
                kit.calendar_blurb(),
                kit.verbal_script()
            ),
        )
    } else {
        (
            "About FlyOnTheWall",
            format!(
                "FlyOnTheWall {}\n\nRecord meetings, retain audio and write notes.\n\nTranscription follows your configured cloud/local settings. The recording indicator displays the actual active recorder state.\n\nhttps://github.com/nolanmak/FlyOnTheWall",
                env!("CARGO_PKG_VERSION")
            ),
        )
    };
    let text = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'><title>{title}</title><style>body{{font:16px system-ui;max-width:48rem;margin:3rem auto;padding:1rem;line-height:1.6;color:#182733;background:#f5f2eb}}pre{{white-space:pre-wrap;font:inherit}}</style><h1>{title}</h1><pre>{text}</pre>"
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_information_pages_contain_the_actual_disclosure_scripts_and_no_capture_action() {
        let page = info_page(NativeAction::OpenDisclosure);
        assert!(page.contains("Meeting chat"));
        assert!(page.contains("Calendar invite"));
        assert!(page.contains("Say aloud"));
        assert!(!page.contains("<script"));
        assert!(!page.contains("/api/record"));
        assert!(info_page(NativeAction::OpenAbout).contains(env!("CARGO_PKG_VERSION")));
    }
}
