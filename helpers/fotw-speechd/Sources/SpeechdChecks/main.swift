// Model-free checks of the helper, runnable with Command Line Tools (no
// XCTest), like tools/speech-eval/parakeet's protocol check. The protocol
// itself is checked from Rust: crates/fotw-speechd-client/tests/protocol.rs
// runs against this helper with FOTW_SPEECHD_BIN set (`just test` on macOS).
import CryptoKit
import Foundation
import SpeechdCore

struct CheckFailure: Error, CustomStringConvertible {
    let description: String
}

func expect(_ condition: Bool, _ message: @autoclosure () -> String,
            file: StaticString = #fileID, line: UInt = #line) throws {
    if !condition { throw CheckFailure(description: "\(file):\(line): \(message())") }
}

func temporaryDirectory() throws -> URL {
    let url = FileManager.default.temporaryDirectory
        .appendingPathComponent("speechd-checks-\(UUID().uuidString)", isDirectory: true)
    try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    return url
}

// MARK: - Frames

func frameRoundTripAndRefusals() throws {
    let audio = FrameCodec.encodeAudio(stream: 7, pcm: [Int16.min, -1, 0, 1, Int16.max])
    try expect(Array(audio.prefix(5)) == [0, 0, 0, 15, Wire.kindAudio], "audio header \(Array(audio.prefix(5)))")
    try expect(try FrameReader(data: audio).next() == .audio(stream: 7, pcm: [Int16.min, -1, 0, 1, Int16.max]),
               "audio round trip")
    let control = FrameCodec.encodeControl(Data(#"{"type":"health","id":1}"#.utf8))
    guard case let .control(json)? = try FrameReader(data: control).next() else {
        throw CheckFailure(description: "control round trip")
    }
    try expect(try Request.decode(json) == .health(id: 1), "health decodes")

    let oversized = Data(withUnsafeBytes(of: UInt32(Wire.maxFrame + 1).bigEndian) { Array($0) } + [Wire.kindAudio])
    let cases: [(Data, FrameError)] = [
        (oversized, .tooLarge(Wire.maxFrame + 1)),
        (Data([0, 0, 0, 0]), .empty),
        (Data([0, 0, 0, 2, 0x7f, 0]), .unknownKind(0x7f)),
        (Data([0, 0, 0, 3, Wire.kindAudio, 0, 0]), .badAudio("no stream id")),
        (Data([0, 0, 0, 5, Wire.kindAudio, 0, 0, 0, 1]), .badAudio("no samples")),
        (Data([0, 0, 0, 6, Wire.kindAudio, 0, 0, 0, 1, 7]), .badAudio("odd byte count")),
        (Data([0, 0, 0, 9, Wire.kindControl, 0x7b]), .truncated),
    ]
    for (bytes, expected) in cases {
        do {
            _ = try FrameReader(data: bytes).next()
            throw CheckFailure(description: "accepted \(Array(bytes.prefix(8)))")
        } catch let error as FrameError {
            try expect(error == expected, "\(error) != \(expected)")
        }
    }
    try expect(try FrameReader(data: Data()).next() == nil, "clean end of input")
    for bad in [#"{"type":"teleport","id":1}"#, #"{"type":"load","id":1}"#, "[1]", #"{"id":1}"#] {
        try expect((try? Request.decode(Data(bad.utf8))) == nil, "decoded \(bad)")
    }
}

// MARK: - Lifecycle (Muesli's generation guard)

final class Manager: Sendable {
    let name: String
    init(_ name: String) { self.name = name }
}

actor Recorder {
    var discarded: [String] = []
    func discard(_ manager: Manager) { discarded.append(manager.name) }
}

/// A load that waits until released, so a shutdown can land in the middle.
actor Gate {
    private var waiting: CheckedContinuation<Void, Never>?
    private var isOpen = false
    private var entered: CheckedContinuation<Void, Never>?
    private var hasEntered = false
    func enter() async {
        hasEntered = true
        entered?.resume()
        entered = nil
        if isOpen { return }
        await withCheckedContinuation { waiting = $0 }
    }
    func untilEntered() async {
        if hasEntered { return }
        await withCheckedContinuation { entered = $0 }
    }
    func open() {
        isOpen = true
        waiting?.resume()
        waiting = nil
    }
}

func loadShutdownRaceDiscardsTheStaleManager() async throws {
    let lifecycle = ModelLifecycle<Manager>()
    let recorder = Recorder()
    let gate = Gate()
    let slowLoad = Task {
        try await lifecycle.load({
            await gate.enter()
            return Manager("stale")
        }, discard: { await recorder.discard($0) })
    }
    await gate.untilEntered()
    await lifecycle.shutdown()
    await gate.open()
    do {
        _ = try await slowLoad.value
        throw CheckFailure(description: "a load overtaken by shutdown returned its manager")
    } catch is CancellationError {}
    let current = await lifecycle.current
    try expect(current == nil, "the stale manager was resurrected")
    let discarded = await recorder.discarded
    try expect(discarded == ["stale"], "the stale manager was not released: \(discarded)")

    let fresh = try await lifecycle.load({ Manager("fresh") }, discard: { await recorder.discard($0) })
    try expect(fresh.name == "fresh", "a later load works")
    let kept = await lifecycle.current?.name
    try expect(kept == "fresh", "and is kept")
}

actor Counter {
    var value = 0
    func increment() { value += 1 }
}

func concurrentLoadsShareOneLoad() async throws {
    let lifecycle = ModelLifecycle<Manager>()
    let loads = Counter()
    let gate = Gate()
    let make: @Sendable () async throws -> Manager = {
        await loads.increment()
        await gate.enter()
        return Manager("only")
    }
    let first = Task { try await lifecycle.load(make, discard: { _ in }) }
    await gate.untilEntered()
    let second = Task { try await lifecycle.load(make, discard: { _ in }) }
    try await Task.sleep(nanoseconds: 20_000_000)
    await gate.open()
    let (a, b) = try await (first.value, second.value)
    try expect(a === b, "waiters get the same manager")
    let count = await loads.value
    try expect(count == 1, "loaded \(count) times")
}

// MARK: - Model store

func file(_ path: String, _ bytes: Data) -> PinnedFile {
    PinnedFile(path: path, size: Int64(bytes.count),
               sha256: SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined())
}

func corruptModelFailsValidationAndKeepsThePreviousSelection() async throws {
    let root = try temporaryDirectory()
    defer { try? FileManager.default.removeItem(at: root) }
    let source = root.appendingPathComponent("source")
    let weights = Data(repeating: 7, count: 4096), vocab = Data(#"{"0":"a"}"#.utf8)
    try FileManager.default.createDirectory(at: source.appendingPathComponent("m.mlmodelc/weights"),
                                            withIntermediateDirectories: true)
    try weights.write(to: source.appendingPathComponent("m.mlmodelc/weights/weight.bin"))
    try vocab.write(to: source.appendingPathComponent("vocab.json"))

    let files = [file("m.mlmodelc/weights/weight.bin", weights), file("vocab.json", vocab)]
    let good = PinnedModel(id: "test", repository: "test/test", revision: String(repeating: "a", count: 40),
                           license: "MIT", remoteDirectory: "", files: files)
    let store = ModelStore(root: root.appendingPathComponent("models"))
    do {
        _ = try store.loadableDirectory(good)
        throw CheckFailure(description: "an uninstalled model was loadable")
    } catch let error as SpeechdError {
        try expect(error.code == "model_missing", "uninstalled: \(error)")
    }
    let installed = try await store.install(good, from: DirectoryFetcher(source: source))
    try expect(store.selectedRevision("test") == good.revision, "selected after install")
    try store.verify(good)

    // A new revision whose weights do not match their pinned hash.
    var tampered = weights
    tampered[100] ^= 0xff
    try tampered.write(to: source.appendingPathComponent("m.mlmodelc/weights/weight.bin"))
    let next = PinnedModel(id: "test", repository: "test/test", revision: String(repeating: "b", count: 40),
                           license: "MIT", remoteDirectory: "", files: [file("m.mlmodelc/weights/weight.bin", weights),
                                                                         file("vocab.json", vocab)])
    do {
        _ = try await store.install(next, from: DirectoryFetcher(source: source))
        throw CheckFailure(description: "a corrupt install succeeded")
    } catch let error as SpeechdError {
        try expect(error.code == "install_failed", "corrupt install: \(error)")
    }
    try expect(store.selectedRevision("test") == good.revision, "the previous selection was kept")
    try expect(try store.loadableDirectory(good) == installed, "the previous revision still loads")
    try store.verify(good)
    try expect(!FileManager.default.fileExists(atPath: store.revisionDirectory(next).path),
               "no partial revision left behind")
    let leftovers = try FileManager.default.contentsOfDirectory(atPath: root.appendingPathComponent("models/test").path)
    try expect(Set(leftovers) == [good.revision, "selected"], "staging cleaned up: \(leftovers)")

    // Corruption after install: same size passes the cheap load check and
    // fails the full one; a size change fails both. Neither moves the selection.
    let installedWeights = installed.appendingPathComponent("m.mlmodelc/weights/weight.bin")
    try tampered.write(to: installedWeights)
    do {
        try store.verify(good)
        throw CheckFailure(description: "verify accepted a corrupt file")
    } catch let error as SpeechdError {
        try expect(error.code == "model_invalid", "verify: \(error)")
    }
    try Data(repeating: 7, count: 10).write(to: installedWeights)
    do {
        _ = try store.loadableDirectory(good)
        throw CheckFailure(description: "a truncated model was loadable")
    } catch let error as SpeechdError {
        try expect(error.code == "model_invalid", "load: \(error)")
    }
    try expect(store.selectedRevision("test") == good.revision, "selection unchanged by a failed check")
}

func aFailedSwapKeepsTheWorkingRevision() throws {
    // Reinstalling the selected revision (a repair after `verify` fails, say)
    // must not lose the working copy if putting the new one in place fails.
    let root = try temporaryDirectory()
    defer { try? FileManager.default.removeItem(at: root) }
    let destination = root.appendingPathComponent("rev"), staging = root.appendingPathComponent(".staging")
    let aside = root.appendingPathComponent(".replaced")
    try FileManager.default.createDirectory(at: destination, withIntermediateDirectories: false)
    try FileManager.default.createDirectory(at: staging, withIntermediateDirectories: false)
    try Data("old".utf8).write(to: destination.appendingPathComponent("weight.bin"))
    try Data("new".utf8).write(to: staging.appendingPathComponent("weight.bin"))
    do {
        try ModelStore.swapIn(staging, at: destination, aside: aside) { from, to in
            if from == staging { throw CocoaError(.fileWriteOutOfSpace) }
            try FileManager.default.moveItem(at: from, to: to)
        }
        throw CheckFailure(description: "the failing swap did not throw")
    } catch is CocoaError {}
    let kept = try? Data(contentsOf: destination.appendingPathComponent("weight.bin"))
    try expect(kept == Data("old".utf8), "the working revision is gone after a failed swap")

    try ModelStore.swapIn(staging, at: destination, aside: aside)
    try expect(try Data(contentsOf: destination.appendingPathComponent("weight.bin")) == Data("new".utf8),
               "a successful swap puts the new revision in place")
    try expect(!FileManager.default.fileExists(atPath: aside.path), "and removes the old one after")
}

func aDictationStreamShowsEachStepOnceAndReturnsItsRecognizerReset() async throws {
    let pool = try await SessionPool(capacity: 1, make: { FakeStepRecognizer() }, warm: { try await $0.warm() })
    let first = try await openStepStream(pool: pool, options: StreamOptions(language: "de"), provenance: "f@1")
    var events = try await first.append([Float](repeating: 0.1, count: 3000))
    try expect(events.isEmpty, "nothing to show before a step completes: \(events)")
    events = try await first.append([Float](repeating: 0.1, count: 2000))
    try expect(events == [.partial(text: "steps=1 lang=de", startMs: 0, endMs: 312, utterance: 1)], "\(events)")
    events = try await first.append([Float](repeating: 0.1, count: 100))
    try expect(events.isEmpty, "unchanged text is not sent again: \(events)")
    // Ended mid-step (cancelled): the recognizer goes back, reset.
    await first.close()
    await first.close()
    let available = await pool.available
    try expect(available == 1, "returned once, however often close runs")

    let second = try await openStepStream(pool: pool, options: StreamOptions(), provenance: "f@1")
    let busy = ContinuousClock.now
    do {
        _ = try await openStepStream(pool: pool, options: StreamOptions(), provenance: "f@1")
        throw CheckFailure(description: "two dictations shared one recognizer")
    } catch let error as SpeechdError {
        try expect(error.code == "busy", "\(error)")
    }
    try expect(ContinuousClock.now - busy >= .milliseconds(900), "waited for the recognizer before refusing")
    events = try await second.finish()
    try expect(events == [.final(text: "", startMs: 0, endMs: 0, segments: [], model: "f@1", utterance: 1)],
               "an empty dictation still ends with a final: \(events)")
    _ = try await second.append([Float](repeating: 0.1, count: 4500))
    events = try await second.finish()
    try expect(events.first == .final(text: "steps=2 lang=auto", startMs: 0, endMs: 281, segments: [],
                                       model: "f@1", utterance: 1),
               "the next stream starts from zero, in its own language, and pads its last step: \(events)")
    await second.close()
}

// MARK: - Streaming dictation review fixes (#263)

/// A recognizer whose steps block their thread, as a Core ML prediction does
/// (not cancellable). `process` is the shown model, `processFinal` the typed one.
final class BlockingRecognizer: StepRecognizer, @unchecked Sendable {
    let stepSamples = 1600
    let partialMs: UInt32
    let finalMs: UInt32
    private let lock = NSLock()
    private var _calls = 0
    private var steps = 0
    init(partialMs: UInt32, finalMs: UInt32) {
        self.partialMs = partialMs
        self.finalMs = finalMs
    }
    var calls: Int { lock.withLock { _calls } }
    func setLanguage(_ language: String?) async throws {}
    func process(_ samples: [Float]) async throws {
        usleep(partialMs * 1000)
        lock.withLock {
            _calls += 1
            steps += 1
        }
    }
    func processFinal(_ samples: [Float]) async throws { usleep(finalMs * 1000) }
    func transcript() async -> String { lock.withLock { "steps=\(steps)" } }
    func finish() async throws -> String { await transcript() }
    func reset() async { lock.withLock { steps = 0 } }
}

/// One pooled BlockingRecognizer behind the engine interface.
actor BlockingEngine: SpeechEngine {
    nonisolated let descriptor = EngineDescriptor(id: "blocking-dictation", kind: .streaming, partials: true)
    let recognizer: BlockingRecognizer
    private var pool: SessionPool<BlockingRecognizer>?
    init(_ recognizer: BlockingRecognizer) { self.recognizer = recognizer }
    func load() async throws -> LoadResult {
        let recognizer = recognizer
        pool = try await SessionPool(capacity: 1, make: { recognizer }, warm: { _ in })
        return LoadResult(model: "blocking", revision: "test")
    }
    func warmup() async throws {}
    func unload() async { pool = nil }
    func isLoaded() async -> Bool { pool != nil }
    func makeSession(options: StreamOptions) async throws -> any EngineSession {
        guard let pool else { throw SpeechdError.notLoaded(descriptor.id) }
        return try await openStepStream(pool: pool, options: options, provenance: "blocking@test")
    }
    func available() async -> Int { await pool?.available ?? -1 }
}

func control(_ object: [String: Any]) throws -> Frame {
    .control(try JSONSerialization.data(withJSONObject: object))
}

func aCancelledStreamStopsDecodingItsBacklogAndFreesTheRecognizer() async throws {
    let recognizer = BlockingRecognizer(partialMs: 100, finalMs: 0)
    let engine = BlockingEngine(recognizer)
    let devNull = open("/dev/null", O_WRONLY)
    defer { close(devNull) }
    let server = Server(out: FrameWriter(fd: devNull), engines: [engine], store: nil)
    await server.receive(try control(["type": "hello", "id": 1, "protocol": Int(Wire.protocolVersion), "client": "checks"]))
    // Loaded through the server, as the parent would.
    await server.receive(try control(["type": "load", "id": 2, "engine": "blocking-dictation"]))
    try await Task.sleep(for: .milliseconds(50))
    await server.receive(try control(["type": "open_stream", "id": 3, "stream": 1, "engine": "blocking-dictation",
                                      "sample_rate": 16000, "format": "s16le", "channels": 1]))
    try await Task.sleep(for: .milliseconds(50))
    // Twenty steps queue up behind a 100 ms decode each: 2 s of backlog.
    for _ in 0..<20 {
        await server.receive(.audio(stream: 1, pcm: [Int16](repeating: 100, count: 1600)))
    }
    try await Task.sleep(for: .milliseconds(30))
    await server.receive(try control(["type": "cancel", "stream": 1]))
    try await Task.sleep(for: .milliseconds(400))
    let calls = recognizer.calls
    try expect(calls <= 3, "a cancelled stream kept decoding its backlog: \(calls) of 20 steps")
    let available = await engine.available()
    try expect(available == 1, "the recognizer is back for the next dictation within 400 ms")
}

func aPartialIsNotHeldBehindTheFinalModelsStep() async throws {
    // The shown (560 ms) step is fast; the typed (1120 ms) step is slow. The
    // partial must go out before the slow step, not after it.
    let pool = try await SessionPool(capacity: 1, make: { BlockingRecognizer(partialMs: 5, finalMs: 300) },
                                     warm: { _ in })
    let session = try await openStepStream(pool: pool, options: StreamOptions(), provenance: "b@1")
    let started = ContinuousClock.now
    let events = try await session.append([Float](repeating: 0.1, count: 1600))
    let took = ContinuousClock.now - started
    try expect(events.count == 1, "the step's partial: \(events)")
    try expect(took < .milliseconds(150), "the partial waited \(took) for the final model's step")
    // The final still includes every step, and close waits for the step in flight.
    let final = try await session.finish()
    try expect(final.first == .final(text: "steps=1", startMs: 0, endMs: 100, segments: [], model: "b@1", utterance: 1),
               "\(final)")
    await session.close()
    let available = await pool.available
    try expect(available == 1, "returned after the step in flight")
}

func warmingUpWhileADictationHoldsTheRecognizerIsANoOp() async throws {
    let engine = FakeDictationEngine()
    _ = try await engine.load()
    let session = try await engine.makeSession(options: StreamOptions())
    do {
        try await engine.warmup()
    } catch {
        throw CheckFailure(description: "warmup during a dictation failed: \(error)")
    }
    _ = try await session.append([Float](repeating: 0.1, count: 4000))
    await session.close()
}

func pinsAreComplete() throws {
    let ids = PinnedModels.all.map(\.id)
    try expect(ids == ["parakeet-unified", "silero-vad", "parakeet-eou-320ms", "nemotron-3.5"], "\(ids)")
    for model in PinnedModels.all {
        try expect(model.revision.count == 40 && model.revision.allSatisfy(\.isHexDigit), "\(model.id) revision")
        try expect(!model.files.isEmpty && !model.license.isEmpty, "\(model.id) files and license")
        for file in model.files {
            try expect(file.sha256.count == 64 && file.sha256.allSatisfy(\.isHexDigit), "\(model.id) \(file.path)")
            try expect(file.size > 0 && !file.path.contains(".."), "\(model.id) \(file.path)")
        }
    }
    try expect(PinnedModels.named("parakeet-unified")?.totalBytes == 614_086_229 - 5_309,
               "Parakeet Unified is the #118 snapshot without its README")
}

// MARK: - Runner

let cases: [(String, () async throws -> Void)] = [
    ("frame round trip and refusals", frameRoundTripAndRefusals),
    ("a load-shutdown race discards the stale manager", loadShutdownRaceDiscardsTheStaleManager),
    ("concurrent loads share one load", concurrentLoadsShareOneLoad),
    ("a corrupt model fails validation and keeps the previous selection",
     corruptModelFailsValidationAndKeepsThePreviousSelection),
    ("a failed swap keeps the working revision", aFailedSwapKeepsTheWorkingRevision),
    ("a dictation stream shows each step once and returns its recognizer reset",
     aDictationStreamShowsEachStepOnceAndReturnsItsRecognizerReset),
    ("a cancelled stream stops decoding its backlog and frees the recognizer",
     aCancelledStreamStopsDecodingItsBacklogAndFreesTheRecognizer),
    ("a partial is not held behind the final model's step", aPartialIsNotHeldBehindTheFinalModelsStep),
    ("warming up while a dictation holds the recognizer is a no-op",
     warmingUpWhileADictationHoldsTheRecognizerIsANoOp),
    ("every pin is a full revision with per-file hashes", pinsAreComplete),
]
var failures = 0
for (name, check) in cases {
    do {
        try await check()
        print("PASS: " + name)
    } catch {
        failures += 1
        print("FAIL: \(name) (\(error))")
    }
}
exit(failures == 0 ? 0 : 1)
