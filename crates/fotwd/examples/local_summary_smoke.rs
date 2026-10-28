//! Explicit development smoke: owned artifacts + synthetic input, no library,
//! microphone, keychain, hosted engine or implicit model download.
#[cfg(target_os = "macos")]
#[tokio::main]
async fn main() -> Result<(), String> {
    use fotw_summarize::{LlmAdapter, LlmRequest};
    use fotwd::local_summary::{LocalAdapter, RuntimeConfig};
    let path = std::env::args_os()
        .nth(1)
        .ok_or("usage: local_summary_smoke CONFIG.json")?;
    let config: RuntimeConfig =
        serde_json::from_slice(&std::fs::read(path).map_err(|_| "configuration unavailable")?)
            .map_err(|_| "invalid configuration")?;
    let model = config.model_id.clone();
    let started = std::time::Instant::now();
    let adapter = LocalAdapter::launch(config).await?;
    let load_seconds = started.elapsed().as_secs_f64();
    let request = LlmRequest { model:model.clone(),
        system:"Summarize only the supplied synthetic meeting. Do not invent names, dates or actions. /no_think".into(),
        document:None,user_notes:None,
        instruction:"Synthetic meeting: Ada said: I will send the draft on Friday. Ben said: We decided to keep the existing release date. Reply in two short bullet points stating the action and decision. /no_think".into(),
        citations:false,output_format:None,effort:None,max_output_tokens:256 };
    let call = std::time::Instant::now();
    let result = adapter.complete(&request).await;
    let call_seconds = call.elapsed().as_secs_f64();
    adapter.shutdown().await?;
    let result = result.map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::json!({"model":model,"load_seconds":load_seconds,"call_seconds":call_seconds,
        "input_tokens":result.usage.input_tokens,"output_tokens":result.usage.output_tokens,
        "text":result.text(),"owned_process_exit_confirmed":true,"quality_qualification":false})
    );
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("local summary runtime requires supported macOS");
}
