// Ported from Muesli (https://github.com/Muesli-HQ/muesli, MIT, © 2026 Pranav
// Hari), native/MuesliNative/Sources/MuesliCore/ManagedASRModelDownloads.swift
// at commit 97e455794112126a896cae17f5c829e8ffa820c8: the completion marker
// written only after a whole install validates, the size-checked readiness
// test, and one shared operation per model.
//
// Changed for FlyOnTheWall: every plan is an exact revision with each file's
// SHA-256 (PinnedModels.swift) instead of `main` plus discovery; there is no
// mirror and no legacy-cache promotion; an install stages into a fresh
// directory and is selected only once every file has matched its hash, so a
// corrupt or interrupted install leaves the previous selection in place; and
// loading reads local files only, never downloading or repairing.
import CryptoKit
import Foundation

/// Where a model's files come from during an install.
public protocol ModelFetcher: Sendable {
    /// Put `file`'s bytes at `destination`. The store validates them after.
    func fetch(_ file: PinnedFile, of model: PinnedModel, to destination: URL) async throws
}

/// The network fetcher. The only code in the helper that opens a socket, and
/// only an explicit install reaches it.
public struct HuggingFaceFetcher: ModelFetcher {
    public init() {}

    public func fetch(_ file: PinnedFile, of model: PinnedModel, to destination: URL) async throws {
        let path = model.remoteDirectory.isEmpty ? file.path : model.remoteDirectory + "/" + file.path
        guard let url = URL(string: "https://huggingface.co/\(model.repository)/resolve/\(model.revision)/\(path)") else {
            throw SpeechdError("install_failed", "bad URL for \(file.path)")
        }
        let (temporary, response) = try await URLSession.shared.download(from: url)
        guard let http = response as? HTTPURLResponse, http.statusCode == 200 else {
            try? FileManager.default.removeItem(at: temporary)
            throw SpeechdError("install_failed", "\(file.path): HTTP \((response as? HTTPURLResponse)?.statusCode ?? 0)")
        }
        try FileManager.default.moveItem(at: temporary, to: destination)
    }
}

/// Imports from a local directory laid out like the repository's
/// `remoteDirectory`. Opens no socket.
public struct DirectoryFetcher: ModelFetcher {
    public let source: URL
    public init(source: URL) {
        self.source = source
    }

    public func fetch(_ file: PinnedFile, of model: PinnedModel, to destination: URL) async throws {
        // copyItem clones on APFS, so importing 600 MB costs no space or time.
        try FileManager.default.copyItem(at: source.appendingPathComponent(file.path), to: destination)
    }
}

/// Installed models under one root: `<root>/<model id>/<revision>/`, a
/// completion marker inside it, and `<root>/<model id>/selected` naming the
/// revision in use.
public struct ModelStore: Sendable {
    public let root: URL
    static let markerName = ".fotw-speechd-complete.json"

    private struct Marker: Codable, Equatable {
        struct File: Codable, Equatable {
            let path: String
            let size: Int64
            let sha256: String
        }
        let model: String
        let revision: String
        let files: [File]

        init(_ model: PinnedModel) {
            self.model = model.id
            self.revision = model.revision
            self.files = model.files.map { File(path: $0.path, size: $0.size, sha256: $0.sha256) }
        }
    }

    public init(root: URL) {
        self.root = root
    }

    public func revisionDirectory(_ model: PinnedModel) -> URL {
        root.appendingPathComponent(model.id, isDirectory: true)
            .appendingPathComponent(model.revision, isDirectory: true)
    }

    /// The revision selected for `id`, if any.
    public func selectedRevision(_ id: String) -> String? {
        let url = root.appendingPathComponent(id).appendingPathComponent("selected")
        guard let text = try? String(contentsOf: url, encoding: .utf8) else { return nil }
        let revision = text.trimmingCharacters(in: .whitespacesAndNewlines)
        return revision.isEmpty ? nil : revision
    }

    /// Whether `model` is selected and complete, by the cheap check.
    public func isInstalled(_ model: PinnedModel) -> Bool {
        (try? loadableDirectory(model)) != nil
    }

    /// The directory to load `model` from. Cheap enough for every load: the
    /// selection must name this revision, the marker must match the pin, and
    /// every file must exist with its pinned size. Hashes are checked at
    /// install and by `verify`.
    public func loadableDirectory(_ model: PinnedModel) throws -> URL {
        guard selectedRevision(model.id) == model.revision else {
            throw SpeechdError("model_missing", "\(model.id) is not installed; install it from settings first")
        }
        let directory = revisionDirectory(model)
        let markerURL = directory.appendingPathComponent(Self.markerName)
        guard let data = try? Data(contentsOf: markerURL),
              let marker = try? JSONDecoder().decode(Marker.self, from: data),
              marker == Marker(model)
        else {
            throw SpeechdError("model_invalid", "\(model.id) has no valid completion marker")
        }
        for file in model.files {
            let size = (try? FileManager.default.attributesOfItem(
                atPath: directory.appendingPathComponent(file.path).path)[.size] as? NSNumber)?.int64Value
            guard size == file.size else {
                throw SpeechdError("model_invalid", "\(model.id): \(file.path) is missing or the wrong size")
            }
        }
        return directory
    }

    /// The full check: the cheap one, then every file's SHA-256.
    public func verify(_ model: PinnedModel) throws {
        let directory = try loadableDirectory(model)
        for file in model.files {
            guard try Self.sha256(of: directory.appendingPathComponent(file.path)) == file.sha256 else {
                throw SpeechdError("model_invalid", "\(model.id): \(file.path) does not match its pinned hash")
            }
        }
    }

    /// Install `model` from `fetcher`, then select it. Each file is checked
    /// against its pinned size and hash in a staging directory; any mismatch
    /// fails the install and leaves the previous selection, and the previous
    /// revision's files, untouched.
    public func install(
        _ model: PinnedModel,
        from fetcher: any ModelFetcher,
        progress: @Sendable (_ bytes: Int64, _ total: Int64) -> Void = { _, _ in }
    ) async throws -> URL {
        let manager = FileManager.default
        let modelDirectory = root.appendingPathComponent(model.id, isDirectory: true)
        try manager.createDirectory(at: modelDirectory, withIntermediateDirectories: true)
        let staging = modelDirectory.appendingPathComponent(".staging-\(UUID().uuidString)", isDirectory: true)
        try manager.createDirectory(at: staging, withIntermediateDirectories: false)
        defer { try? manager.removeItem(at: staging) }

        let total = model.totalBytes
        var done: Int64 = 0
        progress(0, total)
        for file in model.files {
            try Task.checkCancellation()
            let components = file.path.split(separator: "/")
            guard !components.isEmpty, !components.contains(where: { $0 == ".." || $0 == "." }),
                  !file.path.hasPrefix("/")
            else {
                throw SpeechdError("install_failed", "unsafe path in the pin: \(file.path)")
            }
            let destination = staging.appendingPathComponent(file.path)
            try manager.createDirectory(at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
            do {
                try await fetcher.fetch(file, of: model, to: destination)
            } catch let error as SpeechdError {
                throw error
            } catch {
                throw SpeechdError("install_failed", "\(file.path): \(error.localizedDescription)")
            }
            let size = (try manager.attributesOfItem(atPath: destination.path)[.size] as? NSNumber)?.int64Value
            guard size == file.size, try Self.sha256(of: destination) == file.sha256 else {
                throw SpeechdError("install_failed", "\(model.id): \(file.path) failed validation; the previous selection is unchanged")
            }
            done += file.size
            progress(done, total)
        }
        try JSONEncoder().encode(Marker(model)).write(to: staging.appendingPathComponent(Self.markerName))

        // Only now does anything outside the staging directory change.
        let destination = revisionDirectory(model)
        if manager.fileExists(atPath: destination.path) {
            let aside = modelDirectory.appendingPathComponent(".replaced-\(UUID().uuidString)")
            try manager.moveItem(at: destination, to: aside)
            try? manager.removeItem(at: aside)
        }
        try manager.moveItem(at: staging, to: destination)
        try Data((model.revision + "\n").utf8).write(
            to: modelDirectory.appendingPathComponent("selected"), options: .atomic)
        return destination
    }

    static func sha256(of url: URL) throws -> String {
        let handle = try FileHandle(forReadingFrom: url)
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try handle.read(upToCount: 4 << 20), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }
}

/// One install per model at a time; a second caller shares the first's
/// result (Muesli's ManagedASRModelOperations, reduced to what this needs).
public actor InstallCoordinator {
    private var running: [String: Task<URL, Error>] = [:]

    public init() {}

    public func install(
        _ model: PinnedModel,
        store: ModelStore,
        fetcher: any ModelFetcher,
        progress: @escaping @Sendable (Int64, Int64) -> Void
    ) async throws -> URL {
        if let task = running[model.id] { return try await task.value }
        let task = Task { try await store.install(model, from: fetcher, progress: progress) }
        running[model.id] = task
        defer { running[model.id] = nil }
        return try await task.value
    }
}
