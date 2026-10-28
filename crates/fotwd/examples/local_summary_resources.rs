//! Content-free machine probe. Reads no meeting library or credentials.
#[cfg(unix)]
fn main() -> Result<(), String> {
    let installation = std::env::args_os()
        .nth(1)
        .ok_or("usage: local_summary_resources INSTALLATION_DIRECTORY")?;
    let result = fotwd::local_summary::probe::probe(std::path::Path::new(&installation));
    println!(
        "{}",
        serde_json::json!({
            "supported_platform":result.memory.supported_platform,
            "physical_memory_bytes":result.memory.physical_bytes,
            "available_memory_estimate_bytes":result.memory.available_bytes,
            "memory_pressure":result.memory.pressure,
            "logical_cpus":result.logical_cpus,
            "performance_cpus":result.performance_cpus,
            "available_disk_bytes":result.available_disk_bytes,
            "macos_version":result.macos_version,
            "model_or_hardware_qualification":false
        })
    );
    Ok(())
}
#[cfg(not(unix))]
fn main() {
    eprintln!("local summary resource probing requires a supported Mac");
}
