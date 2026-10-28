// Live captions (#245) with Parakeet realtime EOU 120M, 320 ms chunks, through
// FluidAudio's StreamingEouAsrManager.
//
// Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
// Hari), native/MuesliNative/Sources/MuesliNativeApp/
// MeetingStreamingPartialSession.swift at commit
// 97e455794112126a896cae17f5c829e8ffa820c8: its ParakeetEOUMeetingPartialEngine
// (one StreamingEouAsrManager per live source, PCM wrapped in a 16 kHz
// AVAudioPCMBuffer, the partial callback as the visible text) and its
// segment handling (each finalized segment restarts the engine for the next).
//
// Changed for the helper: the managers are pooled and warmed at load, since a
// manager holds one stream's state; utterances are cut at FluidAudio's own
// end-of-utterance signal, each numbered so the parent can replace a caption
// in place; and models load only from the pinned, validated directory.
@preconcurrency import AVFoundation
@preconcurrency import CoreML
import FluidAudio
import Foundation
import SpeechdCore

public actor ParakeetEouEngine: SpeechEngine {
    public nonisolated let descriptor: EngineDescriptor
    /// One per concurrent caption stream: FlyOnTheWall's mic and system legs.
    public static let poolSize = 2
    /// End of utterance once the model has signalled it for this long. Lower
    /// than FluidAudio's 1280 ms default so a caption commits soon after a
    /// pause; the persisted text comes from the final engine regardless.
    static let eouDebounceMs = 640
    private let store: ModelStore
    private let model: PinnedModel
    private let chunkSize: StreamingChunkSize
    private let lifecycle = ModelLifecycle<SessionPool<StreamingEouAsrManager>>()

    /// `model` is the pinned variant matching `chunkSize`: 320 ms (Muesli's
    /// choice) or 160 ms (half the latency, about twice the work).
    public init(store: ModelStore, model: PinnedModel, chunkSize: StreamingChunkSize) {
        self.store = store
        self.model = model
        self.chunkSize = chunkSize
        descriptor = EngineDescriptor(id: model.id, kind: .streaming, partials: true, model: model)
    }

    public func load() async throws -> LoadResult {
        let directory = try store.loadableDirectory(model)
        let chunkSize = chunkSize
        _ = try await lifecycle.load({
            try await SessionPool(capacity: Self.poolSize, make: {
                let manager = StreamingEouAsrManager(chunkSize: chunkSize, eouDebounceMs: Self.eouDebounceMs)
                // The explicit-directory overload reads local files only.
                try await manager.loadModels(from: directory)
                return manager
            }, warm: { manager in
                // Every pooled instance compiles once here, not on a live stream.
                _ = try await manager.process(audioBuffer: try pcmBuffer(
                    [Float](repeating: 0, count: chunkSize.chunkSamples)))
                _ = try await manager.finish()
                await manager.reset()
            })
        }, discard: { pool in
            for manager in await pool.drain() { await manager.cleanup() }
        })
        return LoadResult(model: model.id, revision: model.revision)
    }

    /// Each instance was warmed when the pool was built; run one more silent
    /// chunk through an idle one so an explicit warmup still does work.
    public func warmup() async throws {
        guard let pool = await lifecycle.current else { throw SpeechdError.notLoaded(descriptor.id) }
        // Both instances may be streaming; they were warmed at load, so a
        // warmup that finds none idle has nothing left to do.
        guard let manager = try? await pool.checkout(within: .milliseconds(200)) else { return }
        do {
            _ = try await manager.process(audioBuffer: try pcmBuffer(
                [Float](repeating: 0, count: chunkSize.chunkSamples)))
            _ = try await manager.finish()
        } catch {
            await manager.reset()
            if let stale = await pool.checkin(manager) { await stale.cleanup() }
            throw error
        }
        await manager.reset()
        if let stale = await pool.checkin(manager) { await stale.cleanup() }
    }

    public func unload() async {
        if let pool = await lifecycle.shutdown() {
            for manager in await pool.drain() { await manager.cleanup() }
        }
    }

    public func isLoaded() async -> Bool { await lifecycle.current != nil }

    public func makeSession(options: StreamOptions) async throws -> any EngineSession {
        guard let pool = await lifecycle.current else { throw SpeechdError.notLoaded(descriptor.id) }
        let manager = try await pool.checkout(within: .seconds(1))
        let session = EouSession(manager: manager, pool: pool, provenance: "\(model.id)@\(model.revision)")
        await session.attach()
        return session
    }
}

/// Text the manager's callbacks produced during one `process` call.
private final class EouOutbox: @unchecked Sendable {
    // @unchecked: every access is under `lock`.
    private let lock = NSLock()
    private var partial: String?
    private var endOfUtterance: String?

    func setPartial(_ text: String) {
        lock.lock()
        partial = text
        lock.unlock()
    }

    func setEndOfUtterance(_ text: String) {
        lock.lock()
        endOfUtterance = text
        lock.unlock()
    }

    func take() -> (partial: String?, endOfUtterance: String?) {
        lock.lock()
        defer {
            partial = nil
            endOfUtterance = nil
            lock.unlock()
        }
        return (partial, endOfUtterance)
    }
}

/// One caption stream on a pooled manager. Utterances are numbered from 1;
/// each ends at the model's end-of-utterance signal (or `finish`), and the
/// manager is reset for the next.
final class EouSession: EngineSession, @unchecked Sendable {
    // @unchecked: confined to its stream's task.
    private let manager: StreamingEouAsrManager
    private let pool: SessionPool<StreamingEouAsrManager>
    private let provenance: String
    private let outbox = EouOutbox()
    private var utterance: UInt64 = 1
    private var utteranceStart = 0
    private var samples = 0
    private var lastPartial = ""
    private var closed = false

    init(manager: StreamingEouAsrManager, pool: SessionPool<StreamingEouAsrManager>, provenance: String) {
        self.manager = manager
        self.pool = pool
        self.provenance = provenance
    }

    func attach() async {
        let outbox = outbox
        await manager.setPartialCallback { outbox.setPartial($0) }
        await manager.setEouCallback { outbox.setEndOfUtterance($0) }
    }

    func append(_ chunk: [Float]) async throws -> [EngineEvent] {
        guard !chunk.isEmpty else { return [] }
        samples += chunk.count
        _ = try await manager.process(audioBuffer: try pcmBuffer(chunk))
        let (partial, endOfUtterance) = outbox.take()
        if let text = endOfUtterance {
            return await commit(text)
        }
        guard let text = partial?.trimmingCharacters(in: .whitespacesAndNewlines), !text.isEmpty,
              text != lastPartial
        else { return [] }
        lastPartial = text
        return [.partial(text: text, startMs: milliseconds(samples: utteranceStart),
                         endMs: milliseconds(samples: samples), utterance: utterance)]
    }

    func finish() async throws -> [EngineEvent] {
        let text = try await manager.finish()
        return await commit(text)
    }

    func close() async {
        guard !closed else { return }
        closed = true
        await manager.setPartialCallback { _ in }
        await manager.setEouCallback { _ in }
        await manager.reset()
        // After an unload the pool keeps nothing: release the model here.
        if let stale = await pool.checkin(manager) { await stale.cleanup() }
    }

    /// End the current utterance with `text` and start the next one.
    private func commit(_ text: String) async -> [EngineEvent] {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        var events: [EngineEvent] = []
        if !trimmed.isEmpty {
            events.append(.final(text: trimmed, startMs: milliseconds(samples: utteranceStart),
                                 endMs: milliseconds(samples: samples), segments: [], model: provenance,
                                 utterance: utterance))
            utterance += 1
        }
        // The manager keeps its tokens and end-of-utterance flag until reset.
        await manager.reset()
        _ = outbox.take()
        lastPartial = ""
        utteranceStart = samples
        return events
    }
}

/// 16 kHz mono Float32 samples as the buffer FluidAudio's streaming API takes.
func pcmBuffer(_ samples: [Float]) throws -> AVAudioPCMBuffer {
    guard let format = AVAudioFormat(commonFormat: .pcmFormatFloat32, sampleRate: 16_000, channels: 1,
                                     interleaved: false),
          let buffer = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: AVAudioFrameCount(samples.count)),
          let channel = buffer.floatChannelData?[0]
    else {
        throw SpeechdError("engine", "could not allocate a 16 kHz buffer")
    }
    buffer.frameLength = AVAudioFrameCount(samples.count)
    samples.withUnsafeBufferPointer { channel.update(from: $0.baseAddress!, count: samples.count) }
    return buffer
}
