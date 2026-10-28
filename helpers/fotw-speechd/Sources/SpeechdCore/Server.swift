// The helper's side of docs/SPEECHD_PROTOCOL.md.
//
// Frames arrive in order on one task. Nothing slow runs there: loads, warmups
// and installs get a task each, and every stream gets a task that owns its
// engine session and consumes that stream's audio in order. So `health`,
// `cancel` and the other streams are answered while an engine is busy, which
// the parent's heartbeat relies on. Every stream event goes back through the
// server, which drops it if the stream has since been cancelled, so nothing is
// sent for a stream after its `closed`.
import Darwin
import Foundation

enum StreamInput: Sendable {
    case audio([Int16])
    case finish
}

final class StreamWorker: @unchecked Sendable {
    // @unchecked: `backlog` is guarded by `lock`; the rest is immutable.
    let token = UUID()
    let engine: String
    let input: AsyncStream<StreamInput>.Continuation
    private(set) var task: Task<Void, Never>?
    private let lock = NSLock()
    private var queued = 0

    init(engine: String, input: AsyncStream<StreamInput>.Continuation) {
        self.engine = engine
        self.input = input
    }

    func start(_ task: Task<Void, Never>) {
        self.task = task
    }

    /// Samples queued but not yet handed to the engine.
    func adjustBacklog(by samples: Int) -> Int {
        lock.lock()
        defer { lock.unlock() }
        queued += samples
        return queued
    }
}

public actor Server {
    public static let maxStreams = 8
    /// Audio a stream may have queued ahead of its engine: 2 minutes.
    static let maxBacklogSamples = 120 * Int(Wire.sampleRate)

    private let out: FrameWriter
    private let engines: [String: any SpeechEngine]
    private let order: [String]
    private let store: ModelStore?
    private let installs = InstallCoordinator()
    private let started = ContinuousClock.now
    private var handshaken = false
    private var loaded: Set<String> = []
    private var streams: [UInt32: StreamWorker] = [:]
    /// Ids that were open once: single-use, and late audio for them is dropped.
    private var closed: Set<UInt32> = []

    public init(out: FrameWriter, engines: [any SpeechEngine], store: ModelStore?) {
        self.out = out
        self.engines = Dictionary(engines.map { ($0.descriptor.id, $0) }, uniquingKeysWith: { first, _ in first })
        self.order = engines.map(\.descriptor.id)
        self.store = store
    }

    public func receive(_ frame: Frame) {
        switch frame {
        case let .control(json):
            let request: Request
            do {
                request = try Request.decode(json)
            } catch {
                out.fatal("malformed control message: \(error)")
            }
            handle(request)
        case let .audio(stream, pcm):
            guard handshaken else { out.fatal("the first frame must be hello") }
            audio(stream, pcm)
        }
    }

    private func handle(_ request: Request) {
        guard handshaken else {
            guard case let .hello(id, version, _) = request else { out.fatal("the first frame must be hello") }
            guard version == Wire.protocolVersion else {
                out.send(["type": "error", "id": id, "code": "version_mismatch", "fatal": true,
                          "message": "this helper speaks protocol \(Wire.protocolVersion)",
                          "supported": [Wire.protocolVersion]])
                exit(0)
            }
            handshaken = true
            out.send(["type": "hello", "id": id, "protocol": Wire.protocolVersion,
                      "helper": "fotw-speechd/\(SpeechdMain.version)", "capabilities": capabilities()])
            return
        }
        switch request {
        case .hello:
            out.fatal("hello after the handshake")
        case let .load(id, name):
            guard let engine = engine(name, request: id) else { return }
            let out = out
            Task {
                let clock = ContinuousClock.now
                // A first load includes Core ML's compile: tens of seconds.
                out.send(["type": "progress", "id": id, "phase": "load"])
                do {
                    let result = try await engine.load()
                    self.markLoaded(name, true)
                    out.send(["type": "loaded", "id": id, "engine": name, "model": result.model,
                              "revision": result.revision, "elapsed_ms": clock.elapsedMilliseconds])
                } catch {
                    Self.fail(out, id: id, error)
                }
            }
        case let .warmup(id, name):
            guard let engine = engine(name, request: id) else { return }
            let out = out
            Task {
                let clock = ContinuousClock.now
                out.send(["type": "progress", "id": id, "phase": "warmup"])
                do {
                    try await engine.warmup()
                    out.send(["type": "warmed", "id": id, "engine": name, "elapsed_ms": clock.elapsedMilliseconds])
                } catch {
                    Self.fail(out, id: id, error)
                }
            }
        case let .unload(id, name):
            guard let engine = engine(name, request: id) else { return }
            loaded.remove(name)
            let out = out
            Task {
                await engine.unload()
                out.send(["type": "unloaded", "id": id, "engine": name])
            }
        case let .openStream(id, stream, name, sampleRate, format, channels, options):
            open(id: id, stream: stream, name: name, format: (sampleRate, format, channels), options: options)
        case let .finish(stream):
            if let worker = streams[stream] {
                worker.input.yield(.finish)
            } else if !closed.contains(stream) {
                streamError(stream, SpeechdError("unknown_stream", "no stream \(stream)"))
            }
        case let .cancel(stream):
            guard let worker = streams.removeValue(forKey: stream) else { return }
            closed.insert(stream)
            worker.input.finish()
            worker.task?.cancel()
            out.send(["type": "closed", "stream": stream, "reason": "cancelled"])
        case let .health(id):
            var reply: [String: Any] = ["type": "health", "id": id, "uptime_ms": started.elapsedMilliseconds,
                                        "streams": streams.count, "loaded": loaded.sorted()]
            if let rss = residentBytes() { reply["rss_bytes"] = rss }
            out.send(reply)
        case let .install(id, model, from):
            install(id: id, model: model, from: from)
        case let .shutdown(id):
            out.send(["type": "bye", "id": id])
            exit(0)
        }
    }

    private func engine(_ name: String, request id: UInt64) -> (any SpeechEngine)? {
        guard let engine = engines[name] else {
            Self.fail(out, id: id, SpeechdError("unknown_engine", "no engine \(name)"))
            return nil
        }
        return engine
    }

    private func markLoaded(_ name: String, _ isLoaded: Bool) {
        if isLoaded { loaded.insert(name) } else { loaded.remove(name) }
    }

    private func capabilities() -> [String: Any] {
        let engines: [[String: Any]] = order.compactMap { id in
            guard let descriptor = self.engines[id]?.descriptor else { return nil }
            var info: [String: Any] = ["id": id, "kind": descriptor.kind.rawValue, "partials": descriptor.partials,
                                       "languages": descriptor.languages, "sample_rates": descriptor.sampleRates]
            if let maxAudioMs = descriptor.maxAudioMs { info["max_audio_ms"] = maxAudioMs }
            if let model = descriptor.model {
                info["model"] = ["id": model.id, "repository": model.repository, "revision": model.revision,
                                 "license": model.license, "installed": store?.isInstalled(model) ?? false]
            }
            return info
        }
        return ["max_frame_bytes": Wire.maxFrame, "max_streams": Self.maxStreams, "engines": engines]
    }

    // MARK: - Streams

    private func open(id: UInt64, stream: UInt32, name: String, format: (UInt32, String, UInt16),
                      options: StreamOptions) {
        guard let engine = engine(name, request: id) else { return }
        let refusal: SpeechdError? =
            if !engine.descriptor.sampleRates.contains(format.0) || format.1 != "s16le" || format.2 != 1 {
                SpeechdError("unsupported_format", "\(name) takes mono s16le at \(engine.descriptor.sampleRates) Hz")
            } else if streams[stream] != nil || closed.contains(stream) {
                SpeechdError("stream_exists", "stream ids are single-use")
            } else if streams.count >= Self.maxStreams {
                SpeechdError("busy", "at most \(Self.maxStreams) streams")
            } else {
                nil
            }
        if let refusal {
            Self.fail(out, id: id, refusal)
            return
        }
        let (inputs, continuation) = AsyncStream.makeStream(of: StreamInput.self)
        let worker = StreamWorker(engine: name, input: continuation)
        streams[stream] = worker
        let token = worker.token
        worker.start(Task {
            await Self.run(server: self, token: token, request: id, stream: stream, engine: engine,
                           options: options, inputs: inputs, worker: worker)
        })
    }

    private static func run(server: Server, token: UUID, request: UInt64, stream: UInt32,
                            engine: any SpeechEngine, options: StreamOptions,
                            inputs: AsyncStream<StreamInput>, worker: StreamWorker) async {
        let session: any EngineSession
        do {
            session = try await engine.makeSession(options: options)
        } catch {
            await server.openFailed(token: token, request: request, stream: stream, error)
            return
        }
        guard await server.opened(token: token, request: request, stream: stream) else {
            await session.close()
            return
        }
        do {
            loop: for await input in inputs {
                switch input {
                case let .audio(pcm):
                    let events = try await session.append(pcm.map { Float($0) / 32768 })
                    _ = worker.adjustBacklog(by: -pcm.count)
                    await server.emit(token: token, stream: stream, events)
                case .finish:
                    let events = try await session.finish()
                    await server.complete(token: token, stream: stream, events)
                    break loop
                }
            }
        } catch {
            await server.streamFailed(token: token, stream: stream, error)
        }
        // Finished, cancelled (the input ended) or failed: a pooled engine
        // takes its instance back now.
        await session.close()
    }

    private func audio(_ stream: UInt32, _ pcm: [Int16]) {
        guard let worker = streams[stream] else {
            // Audio already in flight when the stream ended is dropped; audio
            // for an id never opened means the parent's framing is broken.
            if closed.contains(stream) { return }
            out.fatal("audio for a stream that was never opened")
        }
        if worker.adjustBacklog(by: pcm.count) > Self.maxBacklogSamples {
            end(stream)
            worker.input.finish()
            worker.task?.cancel()
            streamError(stream, SpeechdError("busy", "the engine fell more than 2 minutes behind"))
            return
        }
        worker.input.yield(.audio(pcm))
    }

    private func isCurrent(_ token: UUID, _ stream: UInt32) -> Bool {
        streams[stream]?.token == token
    }

    private func end(_ stream: UInt32) {
        streams[stream] = nil
        closed.insert(stream)
    }

    private func opened(token: UUID, request: UInt64, stream: UInt32) -> Bool {
        guard isCurrent(token, stream) else { return false }
        out.send(["type": "stream_opened", "id": request, "stream": stream])
        return true
    }

    private func openFailed(token: UUID, request: UInt64, stream: UInt32, _ error: Error) {
        guard isCurrent(token, stream) else { return }
        end(stream)
        Self.fail(out, id: request, error)
    }

    private func emit(token: UUID, stream: UInt32, _ events: [EngineEvent]) {
        guard isCurrent(token, stream) else { return }
        for event in events {
            out.send(Self.encode(event, stream: stream))
        }
    }

    private func complete(token: UUID, stream: UInt32, _ events: [EngineEvent]) {
        guard isCurrent(token, stream), let worker = streams[stream] else { return }
        emit(token: token, stream: stream, events)
        end(stream)
        out.send(["type": "closed", "stream": stream, "reason": "finished"])
        if worker.engine == "fake-exit-after-final" {
            // A helper that dies right after its last result (the client must
            // still deliver it).
            exit(70)
        }
    }

    private func streamFailed(token: UUID, stream: UInt32, _ error: Error) {
        guard isCurrent(token, stream) else { return }
        end(stream)
        streamError(stream, error)
    }

    private func streamError(_ stream: UInt32, _ error: Error) {
        let (code, message) = Self.describe(error)
        out.send(["type": "error", "stream": stream, "code": code, "message": message])
    }

    static func encode(_ event: EngineEvent, stream: UInt32) -> [String: Any] {
        switch event {
        case let .partial(text, startMs, endMs, utterance):
            var reply: [String: Any] = ["type": "partial", "stream": stream, "text": text, "start_ms": startMs,
                                        "end_ms": endMs]
            if let utterance { reply["utterance"] = utterance }
            return reply
        case let .final(text, startMs, endMs, segments, model, utterance):
            var reply: [String: Any] = [
                "type": "final", "stream": stream, "text": text, "start_ms": startMs, "end_ms": endMs,
                "segments": segments.map { segment -> [String: Any] in
                    var row: [String: Any] = ["start_ms": segment.startMs, "end_ms": segment.endMs]
                    if let text = segment.text { row["text"] = text }
                    if let speaker = segment.speaker { row["speaker"] = speaker }
                    return row
                },
            ]
            if let model { reply["model"] = model }
            if let utterance { reply["utterance"] = utterance }
            return reply
        case let .vad(kind, sample, probability):
            return ["type": "vad", "stream": stream, "event": kind.rawValue, "sample": sample,
                    "probability": Double(probability)]
        }
    }

    // MARK: - Install

    private func install(id: UInt64, model name: String, from: String?) {
        guard let store else {
            Self.fail(out, id: id, SpeechdError("install_failed", "this helper has no models root"))
            return
        }
        guard let model = PinnedModels.named(name) else {
            Self.fail(out, id: id, SpeechdError("unknown_engine", "no pinned model \(name)"))
            return
        }
        let fetcher: any ModelFetcher = from.map { DirectoryFetcher(source: URL(fileURLWithPath: $0)) }
            ?? HuggingFaceFetcher()
        let out = out
        let installs = installs
        let phase = fetcher is DirectoryFetcher ? "verify" : "download"
        Task {
            do {
                _ = try await installs.install(model, store: store, fetcher: fetcher) { bytes, total in
                    out.send(["type": "progress", "id": id, "phase": phase,
                              "model": name, "bytes": bytes, "total": total])
                }
                out.send(["type": "installed", "id": id, "model": name, "revision": model.revision])
            } catch {
                Self.fail(out, id: id, error)
            }
        }
    }

    // MARK: - Errors

    static func describe(_ error: Error) -> (String, String) {
        switch error {
        case let error as SpeechdError: (error.code, error.message)
        case is CancellationError: ("engine", "cancelled")
        default: ("engine", String(String(describing: error).prefix(500)))
        }
    }

    static func fail(_ out: FrameWriter, id: UInt64, _ error: Error) {
        let (code, message) = describe(error)
        out.send(["type": "error", "id": id, "code": code, "message": message])
    }
}

extension ContinuousClock.Instant {
    var elapsedMilliseconds: UInt64 {
        let elapsed = ContinuousClock.now - self
        let (seconds, attoseconds) = elapsed.components
        return UInt64(max(0, seconds)) * 1000 + UInt64(max(0, attoseconds)) / 1_000_000_000_000_000
    }
}

/// Current resident memory, for `health`.
func residentBytes() -> UInt64? {
    var info = rusage_info_v2()
    let result = withUnsafeMutablePointer(to: &info) { pointer in
        pointer.withMemoryRebound(to: rusage_info_t?.self, capacity: 1) {
            proc_pid_rusage(getpid(), RUSAGE_INFO_V2, $0)
        }
    }
    return result == 0 ? info.ri_resident_size : nil
}
