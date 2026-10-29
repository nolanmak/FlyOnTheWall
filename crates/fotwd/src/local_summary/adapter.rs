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

    /// Fault injection for isolated process fixtures, excluded from shipped builds.
    #[cfg(feature = "test-guards")]
    #[doc(hidden)]
    pub async fn crash_owner_for_testing(&self) -> Result<(), String> {
        self.process.crash_owner_for_testing().await
    }

    async fn generate(&self, request: &LlmRequest) -> Result<LlmResponse, SummarizeError> {
        request.validate(&self.capabilities())?;
        // Only the application's frozen extraction vocabulary is supported.
        // Do not silently drop constraints from arbitrary JSON Schema dialects.
        if request
            .output_format
            .as_ref()
            .is_some_and(|schema| !super::extraction_schema::supports(schema))
        {
            return Err(SummarizeError::UnsupportedCapability {
                capability: "local_extraction_schema",
            });
        }
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
            let mut completion = json!({"prompt":tokens,"n_predict":request.max_output_tokens,"stream":false,
                    "temperature":0,"seed":42,"cache_prompt":false,"id_slot":0});
            if let Some(schema) = &request.output_format {
                completion["json_schema"] = schema.clone();
            }
            let response = ipc::request(&self.process.socket,pid,Endpoint::Completion,
                Some(completion),
                Duration::from_secs(self.config.call_timeout_seconds)).await.map_err(SummarizeError::Transport)?;
            let decoded = decode(&response,tokens.len())?;
            if let Some(schema) = &request.output_format {
                super::extraction_schema::validate(&decoded.text(), schema)?;
            }
            Ok(decoded)
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
        capabilities.strict_json_schema = true;
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
    let mut system = request.system.clone();
    if let Some(schema) = &request.output_format {
        // Native json_schema constrains sampling; supply the field meanings
        // in the rendered prompt too. This is our supported application schema,
        // kept outside the transcript and notes' untrusted-data blocks.
        system.push_str("\n\nReturn only JSON matching this application output schema:\n");
        system.push_str(&schema.to_string());
    }
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
    Ok(json!([{"role":"system","content":system},{"role":"user","content":content}]))
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
    use fotw_summarize::EXTRACTION_SCHEMA;

    fn empty_extraction() -> Value {
        json!({"action_items":[],"decisions":[],"open_questions":[],"follow_ups":[],"topics":[]})
    }

    async fn protocol_adapter(content: &str) -> (Arc<LocalAdapter>, std::path::PathBuf) {
        let (process, listening, received) = process::protocol_fixture(content);
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
            call_timeout_seconds: 5,
        };
        let adapter = Arc::new(LocalAdapter {
            process,
            config,
            queue: Semaphore::new(5),
            serial: Semaphore::new(1),
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !listening.exists() {
            adapter
                .process
                .pid()
                .expect("owned fixture exited before readiness");
            assert!(
                tokio::time::Instant::now() < deadline,
                "owned fixture did not listen"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (adapter, received)
    }

    fn extraction_request() -> LlmRequest {
        LlmRequest {
            model: "fixture".into(),
            system: "synthetic".into(),
            document: None,
            user_notes: None,
            instruction: "Extract this synthetic meeting.".into(),
            citations: false,
            output_format: Some(EXTRACTION_SCHEMA.clone()),
            effort: None,
            max_output_tokens: 128,
        }
    }

    #[tokio::test]
    async fn extraction_schema_reaches_the_owned_native_completion_endpoint() {
        let answer = empty_extraction().to_string();
        let (adapter, received) = protocol_adapter(&answer).await;
        let result = adapter.complete(&extraction_request()).await;
        adapter.shutdown().await.unwrap();
        assert_eq!(result.unwrap().text(), answer);
        let native: Value = serde_json::from_slice(&std::fs::read(&received).unwrap()).unwrap();
        assert_eq!(native["json_schema"], *EXTRACTION_SCHEMA);
        assert_eq!(native["prompt"], json!([1, 2, 3]));
        for unsupported in [
            "output_format",
            "response_format",
            "citations",
            "tools",
            "effort",
            "messages",
        ] {
            assert!(native.get(unsupported).is_none(), "{unsupported}");
        }
        assert!(adapter.capabilities().strict_json_schema);
        assert!(!adapter.capabilities().native_citations);
        let template: Value = serde_json::from_slice(
            &std::fs::read(received.with_file_name("apply-template.json")).unwrap(),
        )
        .unwrap();
        assert!(
            template["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains(&EXTRACTION_SCHEMA.to_string()),
            "the model must see field definitions, not just a sampling constraint"
        );
    }

    #[tokio::test]
    async fn ordinary_prose_does_not_request_json_grammar() {
        let (adapter, received) = protocol_adapter("Ordinary synthetic prose.").await;
        let mut request = extraction_request();
        request.output_format = None;
        let result = adapter.complete(&request).await;
        adapter.shutdown().await.unwrap();
        assert_eq!(result.unwrap().text(), "Ordinary synthetic prose.");
        let native: Value = serde_json::from_slice(&std::fs::read(&received).unwrap()).unwrap();
        assert!(native.get("json_schema").is_none());
        let template: Value = serde_json::from_slice(
            &std::fs::read(received.with_file_name("apply-template.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(template["messages"][0]["content"], request.system);
    }

    #[tokio::test]
    async fn unsupported_schemas_are_refused_before_any_owned_runtime_request() {
        let (adapter, received) = protocol_adapter("not used").await;
        let mut request = extraction_request();
        request.output_format = Some(json!({"type":"string","pattern":"unimplemented"}));
        let result = adapter.complete(&request).await;
        adapter.shutdown().await.unwrap();
        assert!(result.is_err());
        assert!(!received.exists());
        assert!(!received.with_file_name("requests.log").exists());
    }

    #[tokio::test]
    async fn a_complete_native_reply_cannot_bypass_independent_extraction_validation() {
        let valid = json!({
            "action_items":[{
                "text":"Send the fixture draft","owner":null,"due":null,"due_raw":null,
                "confidence":"explicit","evidence_segment_ids":[0],
                "evidence_quote":"Send the fixture draft"
            }],"decisions":[],"open_questions":[],"follow_ups":[],"topics":[]
        });
        let (adapter, _) = protocol_adapter(&valid.to_string()).await;
        let result = adapter.complete(&extraction_request()).await;
        adapter.shutdown().await.unwrap();
        assert!(result.is_ok(), "nullable uncertainty must remain supported");

        let mut all_kinds = valid.clone();
        all_kinds["action_items"][0]["owner"] = json!("Fixture speaker");
        all_kinds["action_items"][0]["due"] = json!("2026-01-02");
        all_kinds["action_items"][0]["due_raw"] = json!("Friday");
        all_kinds["decisions"] = json!([{
            "text":"Keep the fixture date","alternatives_considered":["Move it"],
            "confidence":"implied","evidence_segment_ids":[0],"evidence_quote":"Keep the fixture date"
        }]);
        all_kinds["open_questions"] = json!([{
            "text":"Which fixture?","raised_by":"Fixture speaker","confidence":"explicit",
            "evidence_segment_ids":[0],"evidence_quote":"Which fixture?"
        }]);
        all_kinds["follow_ups"] = json!([{
            "text":"Revisit the fixture","blocked_on":null,"confidence":"explicit",
            "evidence_segment_ids":[0],"evidence_quote":"Revisit the fixture"
        }]);
        all_kinds["topics"] = json!([{"label":"Fixture","start_segment_id":0}]);
        let (adapter, _) = protocol_adapter(&all_kinds.to_string()).await;
        let result = adapter.complete(&extraction_request()).await;
        adapter.shutdown().await.unwrap();
        assert!(
            result.is_ok(),
            "every extraction collection must remain usable"
        );

        let mut faults = Vec::new();
        let mut extra = valid.clone();
        extra["unexpected"] = json!(true);
        faults.push(("unknown root key", extra.to_string()));
        let mut missing = valid.clone();
        missing["action_items"][0]
            .as_object_mut()
            .unwrap()
            .remove("due");
        faults.push(("absent nullable key", missing.to_string()));
        for (field, value) in [
            ("confidence", json!("invented")),
            ("evidence_segment_ids", json!([])),
            ("evidence_segment_ids", json!([-1])),
            ("owner", json!(false)),
            ("unexpected", json!(true)),
        ] {
            let mut bad = valid.clone();
            bad["action_items"][0][field] = value;
            faults.push((field, bad.to_string()));
        }
        let mut bad_topic = valid.clone();
        bad_topic["topics"] = json!([{"label":"fixture","start_segment_id":0.5}]);
        faults.push(("noninteger source id", bad_topic.to_string()));
        let mut bad_array = valid.clone();
        bad_array["decisions"] = json!("not a list");
        faults.push(("nonarray collection", bad_array.to_string()));
        faults.push(("nonobject root", "null".into()));
        faults.push((
            "text wrapped around JSON",
            format!("Here is the result: {valid}"),
        ));
        for (label, answer) in faults {
            let (adapter, received) = protocol_adapter(&answer).await;
            let result = adapter.complete(&extraction_request()).await;
            adapter.shutdown().await.unwrap();
            assert!(
                received.exists(),
                "{label}: must reach the actual native endpoint"
            );
            assert!(
                matches!(result, Err(SummarizeError::SchemaViolation(_))),
                "{label}: a native success flag is insufficient: {result:?}"
            );
        }
    }

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
        // Deliberately slower than the former shared two-second deadline:
        // fixture boot under CI load must not consume the cancellation budget.
        let (process, listening, marker) = process::stalled_fixture(Duration::from_secs(3));
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
        let startup_deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !adapter.process.socket.exists() {
            adapter
                .process
                .pid()
                .expect("owned fixture exited before readiness");
            assert!(
                tokio::time::Instant::now() < startup_deadline,
                "owned fixture did not start"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // bind() creates the path before listen(). The fixture deliberately
        // pauses between them, so path existence cannot serve as readiness.
        while !listening.exists() {
            adapter
                .process
                .pid()
                .expect("owned fixture exited before listening");
            assert!(
                tokio::time::Instant::now() < startup_deadline,
                "owned fixture did not listen"
            );
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
        let dispatch_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !marker.exists() {
            assert!(
                !call.is_finished(),
                "request ended before reaching the fixture"
            );
            assert!(
                tokio::time::Instant::now() < dispatch_deadline,
                "request did not reach the ready fixture"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        call.abort();
        let _ = tokio::time::timeout(Duration::from_secs(1), call)
            .await
            .expect("cancellation must complete within its own one-second budget");
        assert!(adapter.process.pid().is_err());
        let next = tokio::time::timeout(Duration::from_millis(250), adapter.complete(&request))
            .await
            .unwrap();
        assert!(next.unwrap_err().to_string().contains("stopped"));
        adapter.shutdown().await.unwrap();
    }
}
