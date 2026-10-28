// Command line of fotw-speechd.
//
//   fotw-speechd serve   --models-root <abs dir> [--fake-engines]
//   fotw-speechd install <model> --models-root <abs dir> [--from <abs dir>]
//   fotw-speechd verify  <model> --models-root <abs dir>
//   fotw-speechd models  [--models-root <abs dir>]
//
// `serve` speaks docs/SPEECHD_PROTOCOL.md on stdin/stdout. `install` is the
// only path that downloads, and the parent runs it only from an explicit setup
// action. The helper never opens a capture device: the parent sends it PCM.
import Darwin
import Foundation

public enum SpeechdMain {
    public static let version = "0.1.0"

    /// Run the command in `arguments` (without the program name). `realEngines`
    /// builds the model-backed engines; the build without FluidAudio passes
    /// none.
    public static func run(_ arguments: [String],
                           realEngines: @escaping @Sendable (ModelStore) -> [any SpeechEngine]) -> Never {
        guard let command = arguments.first else { usage() }
        var rest = Array(arguments.dropFirst())
        var root: URL?
        var from: URL?
        var fake = false
        var positional: [String] = []
        while !rest.isEmpty {
            let arg = rest.removeFirst()
            switch arg {
            case "--models-root", "--from":
                guard let value = rest.first, value.hasPrefix("/") else { usage("\(arg) takes an absolute path") }
                rest.removeFirst()
                if arg == "--from" { from = URL(fileURLWithPath: value) } else { root = URL(fileURLWithPath: value) }
            case "--fake-engines":
                fake = true
            default:
                if arg.hasPrefix("-") { usage("unknown option \(arg)") }
                positional.append(arg)
            }
        }
        switch (command, positional.count) {
        case ("serve", 0):
            guard let root else { usage("serve needs --models-root") }
            serve(store: ModelStore(root: root), fake: fake, realEngines: realEngines)
        case ("install", 1), ("verify", 1):
            guard let root else { usage("\(command) needs --models-root") }
            guard let model = PinnedModels.named(positional[0]) else { usage("no pinned model \(positional[0])") }
            let store = ModelStore(root: root)
            if command == "verify" {
                do {
                    try store.verify(model)
                    print("\(model.id)@\(model.revision): verified")
                    exit(0)
                } catch {
                    fputs("\(error)\n", stderr)
                    exit(1)
                }
            }
            let fetcher: any ModelFetcher = from.map { DirectoryFetcher(source: $0) } ?? HuggingFaceFetcher()
            Task {
                do {
                    let directory = try await store.install(model, from: fetcher) { bytes, total in
                        fputs("\r\(model.id): \(bytes * 100 / max(total, 1))%", stderr)
                    }
                    fputs("\n", stderr)
                    print(directory.path)
                    exit(0)
                } catch {
                    fputs("\n\(error)\n", stderr)
                    exit(1)
                }
            }
            dispatchMain()
        case ("models", 0):
            let store = root.map { ModelStore(root: $0) }
            let rows: [[String: Any]] = PinnedModels.all.map { model in
                ["id": model.id, "repository": model.repository, "revision": model.revision,
                 "license": model.license, "bytes": model.totalBytes,
                 "installed": store?.isInstalled(model) ?? false]
            }
            let json = try! JSONSerialization.data(withJSONObject: rows, options: [.prettyPrinted, .sortedKeys])
            print(String(decoding: json, as: UTF8.self))
            exit(0)
        default:
            usage()
        }
    }

    private static func serve(store: ModelStore, fake: Bool,
                              realEngines: @Sendable (ModelStore) -> [any SpeechEngine]) -> Never {
        signal(SIGPIPE, SIG_IGN)
        // The protocol gets a private copy of stdout; fd 1 becomes stderr, so a
        // framework that prints cannot corrupt the frame stream.
        let protocolFD = dup(STDOUT_FILENO)
        guard protocolFD >= 0, dup2(STDERR_FILENO, STDOUT_FILENO) >= 0 else {
            fputs("fotw-speechd: cannot set up the protocol pipe\n", stderr)
            exit(71)
        }
        let out = FrameWriter(fd: protocolFD)
        let engines = fake ? FakeEngine.all() : realEngines(store)
        let server = Server(out: out, engines: engines, store: store)
        let (frames, continuation) = AsyncStream.makeStream(of: Frame.self)

        let reader = Thread {
            let reader = FrameReader(fd: STDIN_FILENO)
            while true {
                do {
                    guard let frame = try reader.next() else {
                        continuation.finish()
                        return
                    }
                    continuation.yield(frame)
                } catch {
                    out.fatal("\(error)")
                }
            }
        }
        reader.name = "fotw-speechd stdin"
        reader.start()

        Task {
            for await frame in frames {
                await server.receive(frame)
            }
            // EOF on stdin: the parent is done with us.
            exit(0)
        }
        dispatchMain()
    }

    private static func usage(_ message: String? = nil) -> Never {
        if let message { fputs("fotw-speechd: \(message)\n", stderr) }
        fputs("""
        usage: fotw-speechd serve --models-root <dir> [--fake-engines]
               fotw-speechd install <model> --models-root <dir> [--from <dir>]
               fotw-speechd verify <model> --models-root <dir>
               fotw-speechd models [--models-root <dir>]

        """, stderr)
        exit(64)
    }
}
