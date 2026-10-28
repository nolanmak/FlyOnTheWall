//! Explicit public-artifact setup without a meeting library, credentials or
//! microphone. Installation does not qualify a model or enable local summaries.
#[cfg(unix)]
#[tokio::main]
async fn main() -> Result<(), String> {
    use fotwd::local_summary::{catalog::Catalog, provision};
    let mut args = std::env::args_os().skip(1);
    let id = args
        .next()
        .and_then(|v| v.into_string().ok())
        .ok_or("usage: local_summary_install MODEL_ID OWNED_DESTINATION")?;
    let parent = std::path::PathBuf::from(args.next().ok_or("missing owned destination")?);
    if args.next().is_some() {
        return Err("unexpected arguments".into());
    }
    std::fs::create_dir_all(&parent).map_err(|_| "cannot create requested destination")?;
    let catalog = std::sync::Arc::new(Catalog::bundled()?);
    let mut last = None;
    let mut tick = std::time::Instant::now();
    let job = provision::begin(catalog, id, parent, move |p| {
        if last != Some(p.phase) || tick.elapsed() >= std::time::Duration::from_secs(1) {
            eprintln!("{:?}: {} / {} bytes", p.phase, p.completed, p.total);
            last = Some(p.phase);
            tick = std::time::Instant::now();
        }
    })?;
    let path = job.wait().await?;
    println!(
        "{}",
        serde_json::json!({"installed_artifact_directory":path,"quality_qualified":false,"product_local_summaries_enabled":false})
    );
    Ok(())
}
#[cfg(not(unix))]
fn main() {
    eprintln!("local summary provisioning requires supported macOS");
}
