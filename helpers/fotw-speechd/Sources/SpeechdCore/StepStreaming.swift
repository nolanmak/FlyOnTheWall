// Streaming dictation (Murmur #100): one utterance per stream, decoded in
// fixed steps by a cache-aware recognizer such as Nemotron 3.5.
//
// The stream logic follows Muesli (https://github.com/Muesli-HQ/muesli, MIT,
// © 2026 Pranav Hari), native/MuesliNative/Sources/MuesliNativeApp/
// StreamingDictationController.swift at commit
// 97e455794112126a896cae17f5c829e8ffa820c8: audio is decoded a whole step at
// a time while the user speaks, the running transcript is shown as it grows,
// and on stop the last partial step is padded with silence and decoded before
// the final text is taken. Warmup is its "one silent chunk" (here followed by
// a reset, so the warmup audio never reaches a stream).
//
// Changed for the helper: the recognizer is pooled (it holds one stream's
// state), each step's text goes out as a `partial` with utterance 1, the
// final is one `final` with the same utterance, and the recognizer is reset
// when the stream ends however it ends.
import Foundation

/// A recognizer that decodes audio in fixed steps and keeps one utterance's
/// running transcript. Used by one stream at a time.
public protocol StepRecognizer: AnyObject, Sendable {
    /// Samples per decoding step.
    var stepSamples: Int { get }
    /// The language hint for the next utterance; nil auto-detects.
    func setLanguage(_ language: String?) async throws
    /// Take more audio for the shown text; every completed step is decoded now.
    func process(_ samples: [Float]) async throws
    /// Take the same audio for the typed text, when a separate model makes it
    /// (Nemotron's 1120 ms variant). Called after `process`, never
    /// concurrently with another `processFinal` or `finish`.
    func processFinal(_ samples: [Float]) async throws
    /// The utterance's text so far.
    func transcript() async -> String
    /// Pad the last partial step with silence, decode it, and return the text.
    func finish() async throws -> String
    /// Forget the utterance: caches, decoder state and buffered audio.
    func reset() async
    /// Release the models, for a recognizer its engine has unloaded.
    func cleanup() async
}

extension StepRecognizer {
    public func processFinal(_ samples: [Float]) async throws {}
    public func cleanup() async {}

    /// One step of silence, then a reset, so Core ML compiles before the first
    /// real stream and nothing of the warmup leaks into it.
    public func warm() async throws {
        let silence = [Float](repeating: 0, count: stepSamples)
        try await process(silence)
        try await processFinal(silence)
        _ = await transcript()
        await reset()
    }
}

/// One dictation stream on a pooled recognizer. Sends a `partial` whenever a
/// step changes the text, and one `final` on `finish`, both as utterance 1.
public final class StepStreamingSession<Recognizer: StepRecognizer>: EngineSession, @unchecked Sendable {
    // @unchecked: confined to its stream's task.
    private let recognizer: Recognizer
    private let pool: SessionPool<Recognizer>
    private let provenance: String
    private var samples = 0
    private var shown = ""
    private var closed = false
    /// The typed model's steps, chained so they run in order but never hold
    /// up a partial: the shown model's text goes out first.
    private var typing: Task<Void, Error>?
    private let typingFailed = StepFailure()

    /// Take `recognizer` from `pool` for one stream; it goes back on `close`.
    public init(recognizer: Recognizer, pool: SessionPool<Recognizer>, provenance: String) {
        self.recognizer = recognizer
        self.pool = pool
        self.provenance = provenance
    }

    public func append(_ chunk: [Float]) async throws -> [EngineEvent] {
        guard !chunk.isEmpty else { return [] }
        samples += chunk.count
        try typingFailed.check()
        try await recognizer.process(chunk)
        let previous = typing
        let (recognizer, failed) = (recognizer, typingFailed)
        typing = Task {
            try await previous?.value
            do {
                try await recognizer.processFinal(chunk)
            } catch {
                failed.record(error)
                throw error
            }
        }
        let text = await recognizer.transcript().trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty, text != shown else { return [] }
        shown = text
        return [.partial(text: text, startMs: 0, endMs: milliseconds(samples: samples), utterance: 1)]
    }

    public func finish() async throws -> [EngineEvent] {
        try await typing?.value
        let text = samples == 0 ? "" : try await recognizer.finish()
        return [.final(text: text.trimmingCharacters(in: .whitespacesAndNewlines), startMs: 0,
                       endMs: milliseconds(samples: samples), segments: [], model: provenance, utterance: 1)]
    }

    public func close() async {
        guard !closed else { return }
        closed = true
        // A typed step in flight still uses the recognizer.
        _ = try? await typing?.value
        await recognizer.reset()
        // An engine unloaded since this stream opened has drained its pool;
        // the recognizer comes back to be released here.
        if let stale = await pool.checkin(recognizer) {
            await stale.cleanup()
        }
    }
}

/// Lend `pool`'s recognizer to a new stream with `options`' language.
public func openStepStream<Recognizer: StepRecognizer>(
    pool: SessionPool<Recognizer>, options: StreamOptions, provenance: String
) async throws -> StepStreamingSession<Recognizer> {
    // A dictation cancelled just before returns its recognizer from its own
    // task a moment later, so wait for it rather than refuse.
    let recognizer = try await pool.checkout(within: .seconds(1))
    do {
        try await recognizer.setLanguage(options.language)
    } catch {
        if let stale = await pool.checkin(recognizer) { await stale.cleanup() }
        throw error
    }
    return StepStreamingSession(recognizer: recognizer, pool: pool, provenance: provenance)
}

/// The first error of the typed model's background steps.
final class StepFailure: @unchecked Sendable {
    // @unchecked: guarded by `lock`.
    private let lock = NSLock()
    private var error: Error?
    func record(_ error: Error) { lock.withLock { if self.error == nil { self.error = error } } }
    func check() throws { if let error = lock.withLock({ error }) { throw error } }
}

// MARK: - Fake

/// `fake-dictation`: `nemotron-3.5`'s behaviour without a model, for the
/// ordinary test suites (docs/SPEECHD_PROTOCOL.md, "Engines").
public actor FakeDictationEngine: SpeechEngine {
    public static let id = "fake-dictation"
    public nonisolated let descriptor = EngineDescriptor(id: FakeDictationEngine.id, kind: .streaming,
                                                         partials: true)
    private var pool: SessionPool<FakeStepRecognizer>?

    public init() {}

    public func load() async throws -> LoadResult {
        if pool == nil {
            pool = try await SessionPool(capacity: 1, make: { FakeStepRecognizer() }, warm: { try await $0.warm() })
        }
        return LoadResult(model: descriptor.id, revision: "fake")
    }

    public func warmup() async throws {
        guard let pool else { throw SpeechdError.notLoaded(descriptor.id) }
        // A dictation holding it has warmed it already.
        guard await pool.available > 0, let recognizer = try? await pool.checkout() else { return }
        try? await recognizer.warm()
        if let stale = await pool.checkin(recognizer) { await stale.cleanup() }
    }

    public func unload() async { pool = nil }
    public func isLoaded() async -> Bool { pool != nil }

    public func makeSession(options: StreamOptions) async throws -> any EngineSession {
        guard let pool else { throw SpeechdError.notLoaded(descriptor.id) }
        return try await openStepStream(pool: pool, options: options, provenance: "\(descriptor.id)@fake")
    }
}

/// Decodes 250 ms steps into "steps=<n> lang=<language or auto>". A sample
/// at full negative scale fails the stream as an engine error.
public final class FakeStepRecognizer: StepRecognizer, @unchecked Sendable {
    // @unchecked: used by one stream's task at a time (pooled).
    public let stepSamples = 4000
    private var buffered = 0
    private var steps = 0
    private var language = "auto"

    public init() {}

    public func setLanguage(_ language: String?) async throws {
        self.language = language ?? "auto"
    }

    public func process(_ samples: [Float]) async throws {
        if samples.contains(-1) { throw SpeechdError("engine", "fake-dictation was told to fail") }
        buffered += samples.count
        steps += buffered / stepSamples
        buffered %= stepSamples
    }

    public func transcript() async -> String { steps == 0 ? "" : text }

    public func finish() async throws -> String {
        if buffered > 0 { steps += 1 }
        buffered = 0
        return text
    }

    public func reset() async {
        buffered = 0
        steps = 0
        language = "auto"
    }

    private var text: String { "steps=\(steps) lang=\(language)" }
}
