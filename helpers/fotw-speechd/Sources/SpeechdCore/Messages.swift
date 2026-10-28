// Control messages parent → helper (docs/SPEECHD_PROTOCOL.md). Replies are
// built as dictionaries where they are sent, in Server.swift.
import Foundation

public struct StreamOptions: Decodable, Sendable, Equatable {
    public var probabilities: Bool?
    public var language: String?
    public init(probabilities: Bool? = nil, language: String? = nil) {
        self.probabilities = probabilities
        self.language = language
    }
}

public enum Request: Sendable, Equatable {
    case hello(id: UInt64, protocolVersion: UInt32, client: String)
    case load(id: UInt64, engine: String)
    case warmup(id: UInt64, engine: String)
    case unload(id: UInt64, engine: String)
    case openStream(id: UInt64, stream: UInt32, engine: String, sampleRate: UInt32,
                    format: String, channels: UInt16, options: StreamOptions)
    case finish(stream: UInt32)
    case cancel(stream: UInt32)
    case health(id: UInt64)
    case install(id: UInt64, model: String, from: String?)
    case shutdown(id: UInt64)

    /// Parse one control payload. An unknown `type`, a missing field or a
    /// payload that is not an object throws, which the helper treats as a
    /// protocol error. Unknown fields are ignored.
    public static func decode(_ json: Data) throws -> Request {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        func body<T: Decodable>(_: T.Type) throws -> T { try decoder.decode(T.self, from: json) }
        switch try body(Envelope.self).type {
        case "hello":
            let m = try body(Hello.self)
            return .hello(id: m.id, protocolVersion: m.protocol, client: m.client)
        case "load":
            let m = try body(EngineRequest.self)
            return .load(id: m.id, engine: m.engine)
        case "warmup":
            let m = try body(EngineRequest.self)
            return .warmup(id: m.id, engine: m.engine)
        case "unload":
            let m = try body(EngineRequest.self)
            return .unload(id: m.id, engine: m.engine)
        case "open_stream":
            let m = try body(OpenStream.self)
            return .openStream(id: m.id, stream: m.stream, engine: m.engine, sampleRate: m.sampleRate,
                               format: m.format, channels: m.channels, options: m.options ?? StreamOptions())
        case "finish":
            return .finish(stream: try body(StreamRequest.self).stream)
        case "cancel":
            return .cancel(stream: try body(StreamRequest.self).stream)
        case "health":
            return .health(id: try body(IdOnly.self).id)
        case "install":
            let m = try body(Install.self)
            return .install(id: m.id, model: m.model, from: m.from)
        case "shutdown":
            return .shutdown(id: try body(IdOnly.self).id)
        case let other:
            throw DecodingError.dataCorrupted(.init(codingPath: [], debugDescription: "unknown type \(other)"))
        }
    }

    private struct Envelope: Decodable { let type: String }
    private struct IdOnly: Decodable { let id: UInt64 }
    private struct Hello: Decodable {
        let id: UInt64
        let `protocol`: UInt32
        let client: String
    }
    private struct EngineRequest: Decodable {
        let id: UInt64
        let engine: String
    }
    private struct OpenStream: Decodable {
        let id: UInt64
        let stream: UInt32
        let engine: String
        let sampleRate: UInt32
        let format: String
        let channels: UInt16
        let options: StreamOptions?
    }
    private struct StreamRequest: Decodable { let stream: UInt32 }
    private struct Install: Decodable {
        let id: UInt64
        let model: String
        let from: String?
    }
}

/// A request that failed, with its wire code.
public struct SpeechdError: Error, Sendable, Equatable, CustomStringConvertible {
    public let code: String
    public let message: String
    public init(_ code: String, _ message: String) {
        self.code = code
        self.message = message
    }
    public var description: String { "\(code): \(message)" }

    public static func notLoaded(_ engine: String) -> SpeechdError {
        SpeechdError("not_loaded", "\(engine) is not loaded")
    }
}
