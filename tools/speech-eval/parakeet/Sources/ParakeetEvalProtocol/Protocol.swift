import CryptoKit
import Foundation

public enum EvaluationError: Error { case arguments, audio, model }

public struct Invocation {
    public static let modelID = "parakeet-unified-en-0.6b-int8"
    public let audio: URL
    public let models: URL
    public static func parse(_ arguments: [String]) throws -> Invocation {
        let allowed: Set<String> = ["--audio-wav", "--model-directory", "--model-id", "--terms-json"]
        guard arguments.count >= 6, arguments.count <= 8, arguments.count.isMultiple(of: 2) else {
            throw EvaluationError.arguments
        }
        var values: [String: String] = [:]
        for i in stride(from: 0, to: arguments.count, by: 2) {
            let key = arguments[i], value = arguments[i + 1]
            guard allowed.contains(key), values[key] == nil, !value.isEmpty,
                  value.utf8.count <= 4096, !value.contains("\0") else { throw EvaluationError.arguments }
            values[key] = value
        }
        guard let audio = values["--audio-wav"], let models = values["--model-directory"],
              (audio as NSString).isAbsolutePath, (models as NSString).isAbsolutePath,
              values["--model-id"] == modelID else { throw EvaluationError.arguments }
        do {
            let terms = try JSONSerialization.jsonObject(with: Data((values["--terms-json"] ?? "[]").utf8))
            guard let array = terms as? [Any], array.isEmpty else { throw EvaluationError.arguments }
        } catch { throw EvaluationError.arguments }
        return Invocation(audio: URL(fileURLWithPath: audio), models: URL(fileURLWithPath: models))
    }

    public func validateModelDirectory() throws {
        let manager = FileManager.default
        let bundles = ["parakeet_unified_encoder_int8.mlmodelc", "parakeet_unified_decoder.mlmodelc",
                       "parakeet_unified_joint_decision_single_step.mlmodelc"]
        let required = ["vocab.json", "metadata.json"] + bundles.flatMap { bundle in
            ["coremldata.bin", "model.mil", "weights/weight.bin"].map { bundle + "/" + $0 }
        }
        do {
            guard try manager.attributesOfItem(atPath: models.path)[.type] as? FileAttributeType == .typeDirectory,
                  let walker = manager.enumerator(at: models, includingPropertiesForKeys: nil) else {
                throw EvaluationError.model
            }
            var entries = 0, bytes: UInt64 = 0
            for case let url as URL in walker {
                entries += 1
                let info = try manager.attributesOfItem(atPath: url.path)
                let type = info[.type] as? FileAttributeType
                guard entries <= 10000, url.pathComponents.count - models.pathComponents.count <= 32,
                      type == .typeRegular || type == .typeDirectory else { throw EvaluationError.model }
                if type == .typeRegular { bytes += (info[.size] as? NSNumber)?.uint64Value ?? 0 }
                guard bytes <= 2_000_000_000 else { throw EvaluationError.model }
            }
            for item in required {
                let info = try manager.attributesOfItem(atPath: models.appendingPathComponent(item).path)
                guard info[.type] as? FileAttributeType == .typeRegular,
                      (info[.size] as? NSNumber)?.uint64Value ?? 0 > 0 else { throw EvaluationError.model }
            }
        } catch { throw EvaluationError.model }
    }
}

public struct AudioInput {
    public static let maximumBytes = 60_000_000
    public let samples: [Float]
    public let sha256: String
    public static func load(_ url: URL) throws -> AudioInput {
        do {
            guard try FileManager.default.attributesOfItem(atPath: url.path)[.type] as? FileAttributeType == .typeRegular else {
                throw EvaluationError.audio
            }
            let file = try FileHandle(forReadingFrom: url)
            defer { try? file.close() }
            return try AudioInput(data: file.read(upToCount: maximumBytes + 1) ?? Data())
        } catch { throw EvaluationError.audio }
    }
    public init(data: Data) throws {
        guard data.count >= 12, data.count <= Self.maximumBytes,
              data.prefix(4) == Data("RIFF".utf8), data[8..<12] == Data("WAVE".utf8) else {
            throw EvaluationError.audio
        }
        func u16(_ at: Int) -> UInt16 { UInt16(data[at]) | UInt16(data[at + 1]) << 8 }
        func u32(_ at: Int) -> UInt32 { UInt32(u16(at)) | UInt32(u16(at + 2)) << 16 }
        guard Int(u32(4)) + 8 == data.count else { throw EvaluationError.audio }
        var position = 12, format = false, pcm: Range<Int>?
        while position < data.count {
            guard data.count - position >= 8 else { throw EvaluationError.audio }
            let size = Int(u32(position + 4)), begin = position + 8, end = begin + size
            guard end <= data.count, end + size % 2 <= data.count else { throw EvaluationError.audio }
            let tag = data[position..<(position + 4)]
            if tag == Data("fmt ".utf8) {
                guard !format, size >= 16, u16(begin) == 1, u16(begin + 2) == 1,
                      u32(begin + 4) == 16000, u32(begin + 8) == 32000,
                      u16(begin + 12) == 2, u16(begin + 14) == 16 else { throw EvaluationError.audio }
                format = true
            } else if tag == Data("data".utf8) {
                guard format, pcm == nil, size > 0, size.isMultiple(of: 2), size / 2 <= 16000 * 1800 else {
                    throw EvaluationError.audio
                }
                pcm = begin..<end
            }
            position = end + size % 2
        }
        guard format, let pcm else { throw EvaluationError.audio }
        self.samples = stride(from: pcm.lowerBound, to: pcm.upperBound, by: 2).map {
            Float(Int16(bitPattern: u16($0))) / 32768
        }
        self.sha256 = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
    }
}

public struct RawResponse: Encodable {
    public let schema_version = 1
    public let audio_sha256: String
    public let model_id = Invocation.modelID
    public let text: String
    public init(audioSHA256: String, text: String) {
        self.audio_sha256 = audioSHA256
        self.text = text
    }
}
