//! Explicit development smoke: owned artifacts + synthetic input, no library,
//! microphone, keychain, hosted engine or implicit model download.
#[cfg(target_os = "macos")]
fn main() -> Result<(), String> {
    if let Some(result) = fotwd::local_summary::run_worker_if_requested() {
        return result;
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|_| "local smoke runtime unavailable")?
        .block_on(smoke())
}
#[cfg(target_os = "macos")]
async fn smoke() -> Result<(), String> {
    use fotw_summarize::{LlmAdapter, LlmRequest};
    use fotwd::local_summary::{LocalAdapter, RuntimeConfig};
    let mut args = std::env::args_os().skip(1);
    let usage = "usage: local_summary_smoke CONFIG.json [--extraction]";
    let path = args.next().ok_or(usage)?;
    let extraction = match args.next() {
        None => false,
        Some(flag) if flag == "--extraction" => true,
        Some(_) => return Err(usage.into()),
    };
    if args.next().is_some() {
        return Err(usage.into());
    }
    let config: RuntimeConfig =
        serde_json::from_slice(&std::fs::read(path).map_err(|_| "configuration unavailable")?)
            .map_err(|_| "invalid configuration")?;
    let model = config.model_id.clone();
    let output_tokens = config
        .output_tokens
        .min(if extraction { 1024 } else { 256 });
    let started = std::time::Instant::now();
    let adapter = LocalAdapter::launch(config).await?;
    let load_seconds = started.elapsed().as_secs_f64();
    let instruction = if extraction {
        "Synthetic meeting [#0]: This was a sound check. Nobody took an action, made a decision, asked a question or proposed a follow-up. Return the extraction JSON, keeping all lists empty. /no_think"
    } else {
        "Synthetic meeting: Ada said: I will send the draft on Friday. Ben said: We decided to keep the existing release date. Reply in two short bullet points stating the action and decision. /no_think"
    };
    let request = LlmRequest { model:model.clone(),
        system:"Use only the supplied synthetic meeting. Do not invent names, dates or actions. Follow the requested output format. /no_think".into(),
        document:None,user_notes:None,
        instruction:instruction.into(),
        citations:false,output_format:extraction.then(|| fotw_summarize::EXTRACTION_SCHEMA.clone()),
        effort:None,max_output_tokens:output_tokens };
    let call = std::time::Instant::now();
    let result = adapter.complete(&request).await;
    let call_seconds = call.elapsed().as_secs_f64();
    adapter.shutdown().await?;
    let result = result.map_err(|e| e.to_string())?;
    println!(
        "{}",
        serde_json::json!({"model":model,"load_seconds":load_seconds,"call_seconds":call_seconds,
        "input_tokens":result.usage.input_tokens,"output_tokens":result.usage.output_tokens,
        "text":result.text(),"schema_constrained_extraction":extraction,
        "owned_process_exit_confirmed":true,"quality_qualification":false})
    );
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("local summary runtime requires supported macOS");
}
