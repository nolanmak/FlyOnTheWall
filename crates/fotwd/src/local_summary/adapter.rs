use std::{sync::Arc, time::Duration};

use fotw_summarize::{
    AnnotatedBlock, Capabilities, LlmAdapter, LlmRequest, LlmResponse, SummarizeError, Usage,
};
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use super::{
    ipc::{self, Endpoint},
    process::{self, LoadCancellation, Process, RuntimeConfig},
};

/// One owned model process. This low-level adapter is not registered as a
/// product engine until catalog admission, setup and capture scheduling exist.
/// Dropping an in-flight call terminates this runtime; callers must await
/// `shutdown` before releasing its service lease or replacing the model.
pub struct LocalAdapter {
    process: Process,
    config: RuntimeConfig,
    queue: Semaphore,
    serial: Semaphore,
}

struct ActiveCall<'a> {
    process: &'a Process,
    finished: bool,
}
impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.process.stop();
        }
    }
}

impl LocalAdapter {
    /// Verify installed artifacts, launch a network-denied child and wait for
    /// its health response over an authenticated private Unix socket.
    ///
    /// # Errors
    /// Rejects unsupported configuration, artifact mismatch, launch failure or
    /// load deadline. It never downloads models or falls back to a provider.
    pub async fn launch(config: RuntimeConfig) -> Result<Arc<Self>, String> {
        Self::launch_with_cancellation(config, LoadCancellation::default()).await
    }

    /// Load with an explicit cancellation request retained by the service.
    /// Await completion after requesting cancellation to confirm cleanup.
    ///
    /// # Errors
    /// The same verification/load errors as `launch`, including cancellation.
    pub async fn launch_with_cancellation(
        config: RuntimeConfig,
        cancellation: LoadCancellation,
    ) -> Result<Arc<Self>, String> {
        let process = process::launch(&config, &cancellation).await?;
        Ok(Arc::new(Self {
            process,
            config,
            queue: Semaphore::new(5),
            serial: Semaphore::new(1),
        }))
    }

    /// Terminate this owned process and wait for its leader to be reaped.
    ///
    /// # Errors
    /// If exit cannot be confirmed. A replacement must not be admitted yet.
    pub async fn shutdown(&self) -> Result<(), String> {
        self.process.shutdown().await
    }

    async fn generate(&self, request: &LlmRequest) -> Result<LlmResponse, SummarizeError> {
        request.validate(&self.capabilities())?;
        if request.model != self.config.model_id
            || request.max_output_tokens == 0
            || request.max_output_tokens > self.config.output_tokens
        {
            return Err(SummarizeError::Transport(
                "local model/config identity mismatch".into(),
            ));
        }
        let _queued = self
            .queue
            .try_acquire()
            .map_err(|_| SummarizeError::Transport("local summary queue is full".into()))?;
        let _serial = self
            .serial
            .acquire()
            .await
            .map_err(|_| SummarizeError::Transport("local summary queue is closed".into()))?;
        let pid = self.process.pid().map_err(SummarizeError::Transport)?;
        let mut active = ActiveCall {
            process: &self.process,
            finished: false,
        };
        let result = tokio::time::timeout(Duration::from_secs(self.config.call_timeout_seconds), async {
            let messages = messages(request)?;
            let template = ipc::request(&self.process.socket,pid,Endpoint::ApplyTemplate,
                Some(json!({"messages":messages,"chat_template_kwargs":{"enable_thinking":false}})),Duration::from_secs(10)).await.map_err(SummarizeError::Transport)?;
            let prompt = template["prompt"].as_str().ok_or_else(|| SummarizeError::Decode("local template missing prompt".into()))?;
            let tokenized = ipc::request(&self.process.socket,pid,Endpoint::Tokenize,
                Some(json!({"content":prompt,"add_special":false,"parse_special":true})),Duration::from_secs(10)).await.map_err(SummarizeError::Transport)?;
            let tokens = tokenized["tokens"].as_array().filter(|v| !v.is_empty() && v.iter().all(|x| x.as_u64().is_some()))
                .ok_or_else(|| SummarizeError::Decode("local tokenizer returned invalid tokens".into()))?;
            check_context(tokens.len(),request.max_output_tokens,self.config.context_tokens)?;
            let response = ipc::request(&self.process.socket,pid,Endpoint::Completion,
                Some(json!({"prompt":tokens,"n_predict":request.max_output_tokens,"stream":false,
                    "temperature":0,"seed":42,"cache_prompt":false,"id_slot":0})),
                Duration::from_secs(self.config.call_timeout_seconds)).await.map_err(SummarizeError::Transport)?;
            decode(&response,tokens.len())
        }).await.map_err(|_| SummarizeError::Transport("local generation deadline exceeded".into()))?;
        // A clean native response ends ownership of this call. Any failure
        // conservatively stops the process; no hidden generation survives it.
        if result.is_ok() {
            active.finished = true;
        }
        result
    }
}

impl LlmAdapter for LocalAdapter {
    fn capabilities(&self) -> Capabilities {
        let mut capabilities = Capabilities::local_default();
        capabilities.usable_context_tokens = self.config.context_tokens;
        capabilities.max_output_tokens = self.config.output_tokens;
        capabilities
    }
    fn model_id(&self) -> &str {
        &self.config.model_id
    }
    fn complete<'a>(
        &'a self,
        request: &'a LlmRequest,
    ) -> fotw_summarize::transport::BoxFuture<'a, Result<LlmResponse, SummarizeError>> {
        Box::pin(self.generate(request))
    }
}

fn messages(request: &LlmRequest) -> Result<Value, SummarizeError> {
    let mut content = String::new();
    if let Some(payload) = &request.document {
        content.push_str("<transcript>\n");
        for index in &payload.indices {
            let segment = payload
                .document
                .segment(*index)
                .ok_or_else(|| SummarizeError::Decode("unknown local transcript segment".into()))?;
            content.push_str(&segment.block_text());
            content.push('\n');
        }
        content.push_str("</transcript>\n");
    }
    if let Some(notes) = &request.user_notes {
        content.push_str("<user_notes>\n");
        content.push_str(notes);
        content.push_str("\n</user_notes>\n");
    }
    content.push_str(&request.instruction);
    Ok(json!([{"role":"system","content":request.system},{"role":"user","content":content}]))
}

fn check_context(input: usize, output: usize, context: usize) -> Result<(), SummarizeError> {
    // The native tokenizer sees the fully rendered system/notes/template too.
    // Feed those exact IDs into completion, disable context shift at launch.
    let needed = input
        .checked_add(output)
        .and_then(|n| n.checked_add(8))
        .unwrap_or(usize::MAX);
    if needed > context {
        return Err(SummarizeError::ContextOverflow {
            needed,
            usable: context,
        });
    }
    Ok(())
}

fn decode(response: &Value, input: usize) -> Result<LlmResponse, SummarizeError> {
    let content = response["content"]
        .as_str()
        .ok_or_else(|| SummarizeError::Decode("local response missing text".into()))?;
    let output = response["tokens_predicted"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| SummarizeError::Decode("local response missing token accounting".into()))?;
    if content.trim().is_empty() || output == 0 {
        return Err(SummarizeError::Truncated(
            "local model returned no answer".into(),
        ));
    }
    let evaluated = response["tokens_evaluated"]
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or_else(|| SummarizeError::Decode("local response missing prompt accounting".into()))?;
    if response["truncated"].as_bool() != Some(false) || evaluated != input {
        return Err(SummarizeError::Truncated(
            "native context changed or prompt was incomplete".into(),
        ));
    }
    match response["stop_type"].as_str() {
        Some("eos" | "word") => {}
        Some("limit") => {
            return Err(SummarizeError::Truncated(
                "local output token limit reached".into(),
            ));
        }
        _ => {
            return Err(SummarizeError::Decode(
                "local generation did not finish".into(),
            ));
        }
    }
    Ok(LlmResponse {
        blocks: vec![AnnotatedBlock {
            text: content.to_owned(),
            citations: vec![],
        }],
        stop_reason: Some("end_turn".into()),
        usage: Usage {
            input_tokens: input,
            output_tokens: output,
            ..Usage::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_and_template_headroom_must_fit_without_overflow() {
        assert!(check_context(100, 100, 208).is_ok());
        assert!(matches!(
            check_context(101, 100, 208),
            Err(SummarizeError::ContextOverflow { .. })
        ));
        assert!(check_context(usize::MAX, 1, 8192).is_err());
    }
    #[test]
    fn native_truncation_or_missing_tokens_never_become_a_complete_summary() {
        let valid = json!({"content":"synthetic","tokens_predicted":3,"tokens_evaluated":20,"truncated":false,"stop_type":"eos"});
        assert_eq!(decode(&valid, 20).unwrap().text(), "synthetic");
        for (field, value) in [
            ("truncated", json!(true)),
            ("stop_type", json!("limit")),
            ("stop_type", json!("none")),
            ("tokens_evaluated", json!(19)),
            ("tokens_predicted", Value::Null),
            ("content", json!("   ")),
            ("tokens_predicted", json!(0)),
        ] {
            let mut bad = valid.clone();
            bad[field] = value;
            assert!(decode(&bad, 20).is_err(), "{field}");
        }
    }
    #[tokio::test]
    async fn cancelled_generation_stops_its_owned_runtime_and_cannot_be_reused() {
        let (process, marker) = process::stalled_fixture();
        let config = RuntimeConfig {
            model_id: "fixture".into(),
            runtime_files: vec![],
            executable: "llama-server".into(),
            model: process::Artifact {
                path: "/unused".into(),
                sha256: "0".repeat(64),
            },
            model_shards: vec![],
            context_tokens: 8192,
            output_tokens: 512,
            threads: 1,
            load_timeout_seconds: 1,
            call_timeout_seconds: 30,
        };
        let adapter = Arc::new(LocalAdapter {
            process,
            config,
            queue: Semaphore::new(5),
            serial: Semaphore::new(1),
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !adapter.process.socket.exists() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let request = LlmRequest {
            model: "fixture".into(),
            system: "synthetic".into(),
            document: None,
            user_notes: None,
            instruction: "synthetic".into(),
            citations: false,
            output_format: None,
            effort: None,
            max_output_tokens: 128,
        };
        let copy = adapter.clone();
        let req = request.clone();
        let call = tokio::spawn(async move { copy.complete(&req).await });
        while !marker.exists() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        call.abort();
        let _ = call.await;
        assert!(adapter.process.pid().is_err());
        let next = tokio::time::timeout(Duration::from_millis(250), adapter.complete(&request))
            .await
            .unwrap();
        assert!(next.unwrap_err().to_string().contains("stopped"));
        adapter.shutdown().await.unwrap();
    }
}
