// Streaming dictation (Murmur #100) with Nemotron 3.5 ASR Streaming 0.6B,
// multilingual, through FluidAudio's StreamingNemotronMultilingualAsrManager.
//
// Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
// Hari), native/MuesliNative/Sources/MuesliNativeApp/
// Nemotron35StreamingBackend.swift at commit
// 97e455794112126a896cae17f5c829e8ffa820c8: the cache-aware Nemotron 3.5
// model fed whole encoder steps with its stream state carried between them,
// the language passed as the encoder's prompt (auto-detect when unset), the
// load-waiter/generation-guarded load and shutdown (SpeechdCore's
// ModelLifecycle), and single-flight inference (one stream holds the pooled
// recognizer). The stream loop and warmup are StreamingDictationController.swift's
// (SpeechdCore/StepStreaming.swift).
//
// Changed for the helper: FluidAudio's manager replaces Muesli's own RNNT
// loop; models load only from the pinned, validated directory; and two
// variants run in lockstep on the same audio. 560 ms steps give the partials,
// 1120 ms steps give the final. Muesli uses 2240 ms steps, which cannot show
// a first word within a second, and 560 ms alone misses the WER gate
// (measurements on Murmur #100).
import FluidAudio
import Foundation
import SpeechdCore

public actor NemotronStreamingEngine: SpeechEngine {
    public static let id = "nemotron-3.5"
    /// Where the pin keeps each variant, under the model's directory.
    static let partialVariant = "560ms"
    static let finalVariant = "1120ms"
    public nonisolated let descriptor: EngineDescriptor
    private let store: ModelStore
    private let model: PinnedModel
    private let lifecycle = ModelLifecycle<SessionPool<NemotronRecognizer>>()

    public init(store: ModelStore, model: PinnedModel) {
        self.store = store
        self.model = model
        // Languages with a prompt in the model's metadata; anything else
        // auto-detects. Dictation holds one stream at a time.
        descriptor = EngineDescriptor(id: Self.id, kind: .streaming, partials: true,
                                      languages: Self.languages, model: model)
    }

    public func load() async throws -> LoadResult {
        let directory = try store.loadableDirectory(model)
        _ = try await lifecycle.load({
            try await SessionPool(capacity: 1, make: {
                try await NemotronRecognizer.load(
                    partial: directory.appendingPathComponent(Self.partialVariant, isDirectory: true),
                    final: directory.appendingPathComponent(Self.finalVariant, isDirectory: true))
            }, warm: { try await $0.warm() })
        }, discard: { pool in
            for recognizer in await pool.drain() { await recognizer.cleanup() }
        })
        return LoadResult(model: model.id, revision: model.revision)
    }

    /// The recognizer was warmed when the pool was built; warm it again on
    /// request (after a restart the client replays load and warmup).
    public func warmup() async throws {
        guard let pool = await lifecycle.current else { throw SpeechdError.notLoaded(descriptor.id) }
        // A dictation holding the recognizer has warmed it already.
        guard await pool.available > 0, let recognizer = try? await pool.checkout() else { return }
        do {
            try await recognizer.warm()
        } catch {
            await recognizer.reset()
            if let stale = await pool.checkin(recognizer) { await stale.cleanup() }
            throw error
        }
        if let stale = await pool.checkin(recognizer) { await stale.cleanup() }
    }

    public func unload() async {
        if let pool = await lifecycle.shutdown() {
            for recognizer in await pool.drain() { await recognizer.cleanup() }
        }
    }

    public func isLoaded() async -> Bool { await lifecycle.current != nil }

    public func makeSession(options: StreamOptions) async throws -> any EngineSession {
        guard let pool = await lifecycle.current else { throw SpeechdError.notLoaded(descriptor.id) }
        return try await openStepStream(pool: pool, options: options,
                                        provenance: "\(model.id)@\(model.revision)")
    }

    /// The languages the model has a prompt for (its metadata.json), as
    /// bare codes; regional codes such as "en-GB" are accepted too.
    static let languages = [
        "en", "es", "fr", "de", "it", "pt", "nl", "pl", "ru", "uk", "cs", "sk", "hu", "ro", "bg", "hr", "sl",
        "el", "tr", "sv", "da", "fi", "no", "nb", "nn", "lt", "lv", "et", "ar", "hi", "ja", "ko", "zh",
    ]
}

/// Two FluidAudio managers fed the same audio: the 560 ms one's running text
/// is shown, the 1120 ms one's finished text is typed.
final class NemotronRecognizer: StepRecognizer, @unchecked Sendable {
    // @unchecked: pooled, so one stream's task uses it at a time; the
    // managers are actors.
    let stepSamples: Int
    private let partial: StreamingNemotronMultilingualAsrManager
    private let final: StreamingNemotronMultilingualAsrManager

    private init(partial: StreamingNemotronMultilingualAsrManager, final: StreamingNemotronMultilingualAsrManager,
                 stepSamples: Int) {
        self.partial = partial
        self.final = final
        self.stepSamples = stepSamples
    }

    static func load(partial: URL, final: URL) async throws -> NemotronRecognizer {
        let shown = StreamingNemotronMultilingualAsrManager()
        // The directory overload reads local files only.
        try await shown.loadModels(from: partial)
        let typed = StreamingNemotronMultilingualAsrManager()
        do {
            try await typed.loadModels(from: final)
        } catch {
            await shown.cleanup()
            throw error
        }
        // The larger step is the one that matters for warmup: one of it runs
        // both models at least once.
        let step = max(await shown.config.chunkSamples, await typed.config.chunkSamples)
        return NemotronRecognizer(partial: shown, final: typed, stepSamples: step)
    }

    func setLanguage(_ language: String?) async throws {
        // Unknown codes fall back to the model's auto-detect prompt.
        await partial.setLanguage(language)
        await final.setLanguage(language)
    }

    func process(_ samples: [Float]) async throws {
        _ = try await partial.process(samples: samples)
    }

    func processFinal(_ samples: [Float]) async throws {
        _ = try await final.process(samples: samples)
    }

    func transcript() async -> String { await partial.getPartialTranscript() }

    func finish() async throws -> String {
        // Only the final model needs its last step padded and decoded.
        try await final.finish()
    }

    func reset() async {
        await partial.reset()
        await final.reset()
    }

    func cleanup() async {
        await partial.cleanup()
        await final.cleanup()
    }
}
