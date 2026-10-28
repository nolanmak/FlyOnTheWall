// Frames of docs/SPEECHD_PROTOCOL.md, the helper's half. The Rust half is
// crates/fotw-speechd-client/src/frame.rs; the two must agree byte for byte,
// which the Rust protocol tests check by running against this helper.
import Foundation

public enum Wire {
    public static let protocolVersion: UInt32 = 1
    /// Largest frame, counting the kind byte but not the length prefix.
    public static let maxFrame = 1 << 20
    public static let maxControl = 64 << 10
    public static let kindControl: UInt8 = 0x01
    public static let kindAudio: UInt8 = 0x02
    public static let sampleRate: UInt32 = 16_000
}

public enum Frame: Sendable, Equatable {
    case control(Data)
    case audio(stream: UInt32, pcm: [Int16])
}

public enum FrameError: Error, Equatable, CustomStringConvertible {
    case empty
    case tooLarge(Int)
    case controlTooLarge(Int)
    case unknownKind(UInt8)
    case badAudio(String)
    case truncated
    case io(Int32)

    public var description: String {
        switch self {
        case .empty: "empty frame"
        case let .tooLarge(n): "frame of \(n) bytes exceeds the \(Wire.maxFrame)-byte limit"
        case let .controlTooLarge(n): "control message of \(n) bytes exceeds the \(Wire.maxControl)-byte limit"
        case let .unknownKind(k): "unknown frame kind \(k)"
        case let .badAudio(why): "malformed audio frame: \(why)"
        case .truncated: "stream ended inside a frame"
        case let .io(code): "read failed: errno \(code)"
        }
    }
}

public enum FrameCodec {
    /// Validate a length prefix before reading what it describes.
    public static func checkLength(_ prefix: [UInt8]) throws(FrameError) -> Int {
        let length = prefix.reduce(0) { $0 << 8 | Int($1) }
        if length == 0 { throw .empty }
        if length > Wire.maxFrame { throw .tooLarge(length) }
        return length
    }

    public static func decodeBody(_ body: [UInt8]) throws(FrameError) -> Frame {
        guard let kind = body.first else { throw .empty }
        let payload = body.dropFirst()
        switch kind {
        case Wire.kindControl:
            if payload.count > Wire.maxControl { throw .controlTooLarge(payload.count) }
            return .control(Data(payload))
        case Wire.kindAudio:
            guard payload.count >= 4 else { throw .badAudio("no stream id") }
            let samples = payload.dropFirst(4)
            if samples.isEmpty { throw .badAudio("no samples") }
            if samples.count % 2 != 0 { throw .badAudio("odd byte count") }
            let id = payload.prefix(4).reduce(UInt32(0)) { $0 << 8 | UInt32($1) }
            var pcm = [Int16]()
            pcm.reserveCapacity(samples.count / 2)
            var index = samples.startIndex
            while index < samples.endIndex {
                pcm.append(Int16(bitPattern: UInt16(samples[index]) | UInt16(samples[index + 1]) << 8))
                index += 2
            }
            return .audio(stream: id, pcm: pcm)
        default:
            throw .unknownKind(kind)
        }
    }

    public static func encodeControl(_ json: Data) -> Data {
        precondition(json.count <= Wire.maxControl, "control message over the limit")
        return header(kind: Wire.kindControl, payloadCount: json.count) + json
    }

    public static func encodeAudio(stream: UInt32, pcm: [Int16]) -> Data {
        var payload = Data(capacity: 4 + pcm.count * 2)
        withUnsafeBytes(of: stream.bigEndian) { payload.append(contentsOf: $0) }
        for sample in pcm {
            withUnsafeBytes(of: sample.littleEndian) { payload.append(contentsOf: $0) }
        }
        return header(kind: Wire.kindAudio, payloadCount: payload.count) + payload
    }

    private static func header(kind: UInt8, payloadCount: Int) -> Data {
        var data = Data()
        withUnsafeBytes(of: UInt32(payloadCount + 1).bigEndian) { data.append(contentsOf: $0) }
        data.append(kind)
        return data
    }
}

/// Reads frames from a byte source: a file descriptor in the helper, a
/// buffer in the checks.
public final class FrameReader {
    /// Fill as much of the buffer as is available. 0 is end of input; a
    /// negative value is `-errno`.
    public typealias ReadSome = (UnsafeMutableRawBufferPointer) -> Int
    private let readSome: ReadSome

    public init(readSome: @escaping ReadSome) {
        self.readSome = readSome
    }

    public convenience init(fd: Int32) {
        self.init { buffer in
            while true {
                let n = read(fd, buffer.baseAddress, buffer.count)
                if n >= 0 { return n }
                if errno != EINTR { return -Int(errno) }
            }
        }
    }

    public convenience init(data: Data) {
        var offset = 0
        self.init { buffer in
            let count = min(buffer.count, data.count - offset)
            data.copyBytes(to: buffer.bindMemory(to: UInt8.self), from: offset..<(offset + count))
            offset += count
            return count
        }
    }

    /// The next frame, or nil at a clean end of input between frames.
    public func next() throws(FrameError) -> Frame? {
        guard let prefix = try fill(4, endOK: true) else { return nil }
        let length = try FrameCodec.checkLength(prefix)
        guard let body = try fill(length, endOK: false) else { throw .truncated }
        return try FrameCodec.decodeBody(body)
    }

    private func fill(_ count: Int, endOK: Bool) throws(FrameError) -> [UInt8]? {
        var bytes = [UInt8](repeating: 0, count: count)
        var filled = 0
        while filled < count {
            let n = bytes.withUnsafeMutableBytes { raw in
                readSome(UnsafeMutableRawBufferPointer(rebasing: raw[filled...]))
            }
            if n < 0 { throw .io(Int32(-n)) }
            if n == 0 {
                if filled == 0 && endOK { return nil }
                throw .truncated
            }
            filled += n
        }
        return bytes
    }
}

/// Writes whole frames to a file descriptor, one at a time.
public final class FrameWriter: @unchecked Sendable {
    // @unchecked: every write happens under `lock`.
    private let fd: Int32
    private let lock = NSLock()

    public init(fd: Int32) {
        self.fd = fd
    }

    /// Send one control message. Values must be JSON-compatible Foundation
    /// types. Never put transcript text in an error message.
    public func send(_ object: [String: Any]) {
        guard let json = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys]),
              json.count <= Wire.maxControl
        else {
            fatal("a reply exceeded the control-message limit")
        }
        write(FrameCodec.encodeControl(json))
    }

    public func write(_ data: Data) {
        lock.lock()
        defer { lock.unlock() }
        data.withUnsafeBytes { raw in
            var offset = 0
            while offset < raw.count {
                let n = Darwin.write(fd, raw.baseAddress! + offset, raw.count - offset)
                if n < 0 {
                    if errno == EINTR { continue }
                    // The parent is gone; nobody is left to report to.
                    exit(0)
                }
                offset += n
            }
        }
    }

    /// A protocol error is fatal: report it, then exit 65 (EX_DATAERR).
    public func fatal(_ message: String) -> Never {
        let object: [String: Any] = ["type": "error", "code": "protocol", "message": message, "fatal": true]
        if let json = try? JSONSerialization.data(withJSONObject: object) {
            write(FrameCodec.encodeControl(json))
        }
        exit(65)
    }
}
