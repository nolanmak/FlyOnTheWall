//! Owned settings-only UI fixture: temporary encrypted store and real loopback
//! API, with no recorder, provider, model, Keychain or global controls installed.
use fotw_store::{Db, DbKey};
use fotw_web::{MemorySource, WebServer};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("provide an owned empty fixture directory")?,
    );
    std::fs::create_dir_all(&dir)?;
    let marker = dir.join(".owned-capture-settings-fixture");
    if marker.exists() {
        if std::fs::read_to_string(&marker)? != "synthetic settings only\n" {
            return Err("unexpected fixture marker".into());
        }
    } else {
        if std::fs::read_dir(&dir)?.next().is_some() {
            return Err("fixture directory must be empty on first use".into());
        }
        std::fs::write(&marker, "synthetic settings only\n")?;
    }
    // This public fixture key is never used to open an application/user library.
    let db = Db::open(
        dir.join("capture-settings-owned.sqlite3"),
        &DbKey::from_bytes([0x55; 32]),
    )?;
    let server = WebServer::bind(0, Arc::new(MemorySource::new())).await?;
    server
        .state()
        .set_capture(Arc::new(fotwd::capture_settings::SettingsControl::new(db)));
    let url = server.state().launch_url();
    let path = dir.join("launch-url.txt");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    options.open(path)?.write_all(url.as_bytes())?;
    // The parent owns termination; this bound also prevents abandoned fixtures.
    let _ = tokio::time::timeout(Duration::from_secs(45), server.serve()).await?;
    Ok(())
}
