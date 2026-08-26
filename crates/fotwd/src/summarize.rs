//! Turning a stored transcript into an augmented document.
//!
//! Runs entirely off what is already in the library: the transcript is an
//! immutable artifact and the summary is a **derived, versioned** one (§8.9).
//! So regenerating with a different model or template costs zero STT spend and
//! never touches the audio — which is exactly why the schema keeps summaries
//! as append-only rows rather than a mutable column on the meeting.

use std::sync::Arc;

use fotw_secrets::KeyStore;
use fotw_store::{Db, NewSummary};
use fotw_stt::{Source, TimestampSource, TranscriptSegment};
use fotw_summarize::document::TranscriptDocument;
use fotw_summarize::pipeline::{Pipeline, PipelineConfig, SummaryOutcome, Warning};
use fotw_summarize::template::Template;
use fotw_summarize::{AnthropicAdapter, Preset};

use crate::transport::AllowlistedTransport;

/// How long one CLI call may take. Generous — a long meeting is a long
/// prompt — and finite, because a hung CLI must not become a meeting whose
/// enrichment never finishes.
const CLI_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// What a summarisation run produced.
#[derive(Debug)]
pub struct SummarizeOutcome {
    /// The stored summary's version number.
    pub version: i64,
    /// The generated markdown.
    pub markdown: String,
    /// Fraction of substantive claims carrying a citation.
    pub coverage: f64,
    /// Extracted items that survived the evidence validator.
    pub kept_items: usize,
    /// Extracted items dropped because their evidence did not check out.
    pub dropped_items: usize,
    /// What the user should be told, already worded for a human.
    pub warnings: Vec<String>,
}

/// Errors from a summarisation run.
#[derive(Debug, thiserror::Error)]
pub enum SummarizeRunError {
    /// The meeting has no transcript to summarise.
    #[error("meeting {0} has no transcript yet")]
    NoTranscript(String),
    /// No engine is configured — neither an API key nor an enabled CLI.
    #[error(
        "no summarize engine configured — run `fotwd key set anthropic` (API), \
         or `fotwd engine claude-cli --i-acknowledge-egress` (Claude \
         subscription) or `fotwd engine codex-cli --i-acknowledge-egress` \
         (ChatGPT/Codex subscription). The transcript is unchanged and can be \
         summarised later."
    )]
    NoKey,
    /// The store failed.
    #[error("store: {0}")]
    Store(#[from] fotw_store::StoreError),
    /// The provider or the pipeline failed.
    #[error("summarize: {0}")]
    Pipeline(#[from] fotw_summarize::SummarizeError),
}

/// The stored transcript for a meeting, reconstructed for the summariser.
///
/// # Errors
///
/// [`SummarizeRunError::NoTranscript`] when the meeting has none, or whatever
/// the store failed with.
pub(crate) fn stored_segments(
    db: &mut Db,
    meeting_id: &str,
) -> Result<Vec<TranscriptSegment>, SummarizeRunError> {
    let Some(transcript_id) = crate::persist::primary_transcript_id(db, meeting_id) else {
        return Err(SummarizeRunError::NoTranscript(meeting_id.to_owned()));
    };
    Ok(load_segments(db, &transcript_id)?)
}

/// Summarise a stored meeting using the daemon's real, allowlisted transport.
pub async fn summarize_meeting(
    db: &mut Db,
    store: &dyn KeyStore,
    meeting_id: &str,
    template: &Template,
) -> Result<SummarizeOutcome, SummarizeRunError> {
    let transport = Arc::new(AllowlistedTransport::new());
    summarize_meeting_with(db, store, meeting_id, template, transport).await
}

/// Summarise a stored meeting over an injected transport.
///
/// Split out so the glue between the database and the pipeline — loading
/// segments, rebuilding the document, versioning the result — is testable
/// without a network or a key. That glue is where the seams are, and it is
/// exactly what the pipeline's own 157 tests cannot reach.
pub async fn summarize_meeting_with<T>(
    db: &mut Db,
    store: &dyn KeyStore,
    meeting_id: &str,
    template: &Template,
    transport: Arc<T>,
) -> Result<SummarizeOutcome, SummarizeRunError>
where
    T: fotw_summarize::transport::HttpTransport + 'static,
{
    let Some(transcript_id) = crate::persist::primary_transcript_id(db, meeting_id) else {
        return Err(SummarizeRunError::NoTranscript(meeting_id.to_owned()));
    };

    let segments = load_segments(db, &transcript_id)?;
    if segments.is_empty() {
        return Err(SummarizeRunError::NoTranscript(meeting_id.to_owned()));
    }

    let engine = crate::engine::resolve_engine(store, db).ok_or(SummarizeRunError::NoKey)?;

    // The user's typed notes, if any. They are a *saliency signal*, not just
    // another input: the prompt contract treats each line as a pointer to
    // expand from the transcript, never as a claim to accept (§8.3).
    let notes = db
        .meetings()
        .note(meeting_id)?
        .map(|(body, _)| body)
        .unwrap_or_default();

    let document = TranscriptDocument::from_segments(&segments);
    // Two adapters. On the API they are two models — extraction is a
    // low-difficulty task, so it runs on the cheap tier while the prose call
    // gets the better model (spec 8.4's split). On the CLI both use the
    // subscription's own configured default: the plan's model choice belongs
    // to the user, and hardcoding a tier here would fight their settings.
    let prose_model = Preset::Balanced.prose_model().unwrap_or("claude-opus-5");
    let extraction_model = Preset::Cheap.prose_model().unwrap_or("claude-haiku-4-5");
    let (prose, extraction, provider_name, recorded_model): (
        Box<dyn fotw_summarize::adapter::LlmAdapter>,
        Box<dyn fotw_summarize::adapter::LlmAdapter>,
        &str,
        String,
    ) = match &engine {
        crate::engine::Engine::Anthropic { key } => (
            Box::new(AnthropicAdapter::new(
                Arc::clone(&transport),
                prose_model,
                key.expose(),
            )),
            Box::new(AnthropicAdapter::new(
                Arc::clone(&transport),
                extraction_model,
                key.expose(),
            )),
            "anthropic",
            prose_model.to_owned(),
        ),
        crate::engine::Engine::ClaudeCli { binary } => {
            let runner = Arc::new(crate::engine::TokioCliRunner::new(
                binary.clone(),
                CLI_DEADLINE,
            ));
            (
                Box::new(fotw_summarize::claude_cli::ClaudeCliAdapter::new(
                    Arc::clone(&runner),
                    None,
                )),
                Box::new(fotw_summarize::claude_cli::ClaudeCliAdapter::new(
                    runner, None,
                )),
                "claude-cli",
                "claude-cli".to_owned(),
            )
        }
        crate::engine::Engine::Codex { binary } => {
            // Same shape as the claude arm: both calls share one runner, and
            // the model choice is the subscription's own default (None), never
            // ours to hardcode into their plan. `shielded` because codex is
            // agentic — it runs shell over the untrusted transcript — so the
            // child gets an empty $HOME (see TokioCliRunner's docs).
            let runner = Arc::new(crate::engine::TokioCliRunner::shielded(
                binary.clone(),
                CLI_DEADLINE,
            ));
            (
                Box::new(fotw_summarize::codex_cli::CodexCliAdapter::new(
                    Arc::clone(&runner),
                    None,
                )),
                Box::new(fotw_summarize::codex_cli::CodexCliAdapter::new(
                    runner, None,
                )),
                "codex-cli",
                "codex-cli".to_owned(),
            )
        }
    };
    let (prose, extraction) = (&*prose, &*extraction);

    // The template arrives as a parsed file (SUM-08), and what crosses this
    // seam is still just its rendered body -- untrusted text that
    // `prompt::assemble` quarantines. Parsing it earlier bought a located
    // error message, not trust.
    let pipeline = Pipeline::new(prose, extraction).with_config(PipelineConfig {
        template_body: template.prompt_body(),
        ..PipelineConfig::default()
    });

    let outcome = pipeline.run(&document, &notes).await?;
    let warnings = user_warnings(&outcome);

    // The admonition rides in *our* rendered markdown, never in
    // `SummaryOutcome::blocks`: spec 8.6 keeps Call A's blocks unmodified, and
    // this is a note about the run rather than something the model wrote. It
    // needs no schema change — `body_md` already reaches the web view, the
    // export and FTS, so the note becomes searchable, which is fine.
    let mut markdown = outcome.markdown();
    for warning in &warnings {
        markdown.push_str(&format!("\n\n> [!WARNING]\n> {warning}\n"));
    }

    let coverage = if outcome.coverage.total_claims == 0 {
        1.0
    } else {
        outcome.coverage.cited_claims as f64 / outcome.coverage.total_claims as f64
    };

    // Versioned, never overwritten. A user regenerating with a different
    // template must be able to compare, and losing the previous version to do
    // that would make the feature actively worse than not having it.
    let summary = db.meetings().insert_summary(
        meeting_id,
        NewSummary {
            transcript_id: Some(transcript_id),
            provider: provider_name.to_owned(),
            model: recorded_model.clone(),
            prompt_hash: outcome.prompt_hash.clone(),
            body_md: markdown.clone(),
            coverage: Some(coverage),
            input_tokens: Some(
                (outcome.usage_a.input_tokens + outcome.usage_b.input_tokens) as i64,
            ),
            output_tokens: Some(
                (outcome.usage_a.output_tokens + outcome.usage_b.output_tokens) as i64,
            ),
            cost_micros: None,
            template_id: None,
            origin_device_id: "local".to_owned(),
        },
    )?;

    Ok(SummarizeOutcome {
        version: summary.version,
        markdown,
        coverage,
        kept_items: outcome.validation.extraction.action_items.len(),
        dropped_items: outcome.validation.dropped.len(),
        warnings,
    })
}

/// The pipeline warnings worth putting in front of a user, worded for one.
///
/// A deliberately chosen subset rather than every [`Warning`]. Rendering the
/// list verbatim would print a cache miss on every CLI run:
/// `Pipeline::expects_cache_hit` never looks at `prompt_cache`, and both CLI
/// adapters declare `PromptCache::None`, so a miss that could not have been a
/// hit fires every time. A warning that is always wrong teaches people to
/// ignore the ones that are not.
fn user_warnings(outcome: &SummaryOutcome) -> Vec<String> {
    // Under map-reduce the chunks that answered well keep their items, so the
    // honest wording depends on whether anything survived (#75).
    let salvaged = outcome.validation.extraction.item_count() > 0;
    outcome
        .warnings
        .iter()
        .filter_map(|warning| match warning {
            Warning::ExtractionFailed { detail } => Some(format!(
                "Extraction failed ({detail}) — {}. The notes above came from a separate call \
                 and are unaffected.",
                if salvaged {
                    "some structured items may be missing"
                } else {
                    "no action items were extracted from this meeting"
                }
            )),
            _ => None,
        })
        .collect()
}

/// Rebuild `TranscriptSegment`s from stored rows.
///
/// The store keeps only what a transcript *is*; the provider-specific fields
/// the summariser wants (`session_id`, `revision`, `timestamp_source`) are
/// reconstructed rather than persisted, because they describe how the text was
/// obtained rather than what it says.
pub(crate) fn load_segments(
    db: &Db,
    transcript_id: &str,
) -> Result<Vec<TranscriptSegment>, fotw_store::StoreError> {
    let mut stmt = db.conn().prepare(
        "SELECT idx, start_ms, end_ms, channel, speaker_label, text, confidence, is_final, words
           FROM segments WHERE transcript_id = ?1 ORDER BY idx",
    )?;
    let rows = stmt.query_map([transcript_id], |r| {
        let channel: String = r.get(3)?;
        let words: Option<Vec<u8>> = r.get(8)?;
        Ok(TranscriptSegment {
            id: format!("seg-{}", r.get::<_, i64>(0)?),
            session_id: transcript_id.to_owned(),
            source: if channel == "mic" {
                Source::Mic
            } else {
                Source::System
            },
            speaker: r.get(4)?,
            text: r.get(5)?,
            start_ms: r.get::<_, i64>(1)? as u64,
            end_ms: r.get::<_, i64>(2)? as u64,
            words: words
                .as_deref()
                .and_then(crate::persist::decode_words)
                .unwrap_or_default(),
            confidence: r.get(6)?,
            language: None,
            is_final: r.get::<_, i64>(7)? != 0,
            revision: 0,
            provider: "stored".to_owned(),
            model: "stored".to_owned(),
            timestamp_source: TimestampSource::Provider,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
}
