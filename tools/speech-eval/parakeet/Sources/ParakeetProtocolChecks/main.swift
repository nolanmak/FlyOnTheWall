// Model-free behavior checks, runnable with Command Line Tools (no XCTest SDK).
import CryptoKit
import Foundation
import ParakeetEvalProtocol

enum CheckFailure: Error { case assertion, expectedRejection }
func equal<T: Equatable>(_ actual: T, _ expected: T) throws {
    if actual != expected { throw CheckFailure.assertion }
}
func rejects(_ action: () throws -> Void) throws {
    do { try action() } catch is EvaluationError { return }
    throw CheckFailure.expectedRejection
}
func wav(_ samples: [Int16] = [-32768, 0, 32767]) -> Data {
    func le<T: FixedWidthInteger>(_ value: T) -> Data {
        var number = value.littleEndian
        return withUnsafeBytes(of: &number) { Data($0) }
    }
    var data = Data("RIFF".utf8)
    data += le(UInt32(36 + samples.count * 2))
    data += Data("WAVEfmt ".utf8) + le(UInt32(16))
    data += le(UInt16(1)) + le(UInt16(1)) + le(UInt32(16000))
    data += le(UInt32(32000)) + le(UInt16(2)) + le(UInt16(16))
    data += Data("data".utf8) + le(UInt32(samples.count * 2))
    for sample in samples { data += le(sample) }
    return data
}

func exactPCMAndSourceReceipt() throws {
    let bytes = wav(), input = try AudioInput(data: bytes)
    try equal(input.samples, [-1, 0, Float(32767) / 32768])
    try equal(input.sha256, SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined())
    let response = try JSONSerialization.jsonObject(with: JSONEncoder().encode(
        RawResponse(audioSHA256: input.sha256, text: "日本語 C++ 20")
    )) as! [String: Any]
    try equal(response["schema_version"] as? Int, 1)
    try equal(response["model_id"] as? String, Invocation.modelID)
    try equal(response["text"] as? String, "日本語 C++ 20")
}
func malformedTruncatedAndWrongFormatAudio() throws {
    var wrongRate = wav(); wrongRate[24] = 1
    var stereo = wav(); stereo[22] = 2
    var nonPCM = wav(); nonPCM[20] = 3
    var partialFrame = wav(); partialFrame.removeLast()
    for bytes in [Data(), Data("not audio".utf8), wav([]), wrongRate, stereo, nonPCM, partialFrame] {
        try rejects { _ = try AudioInput(data: bytes) }
    }
}
func explicitArgumentsAndUnsupportedHints() throws {
    let args = ["--audio-wav", "/owned/input.wav", "--model-directory", "/owned/models",
                "--model-id", Invocation.modelID, "--terms-json", "[]"]
    let parsed = try Invocation.parse(args)
    try equal(parsed.audio.path, "/owned/input.wav")
    try equal(parsed.models.path, "/owned/models")
    for bad in [args + ["--audio-wav", "/other.wav"], Array(args.dropLast()),
                ["--audio-wav", "relative.wav", "--model-directory", "/owned", "--model-id", Invocation.modelID],
                ["--audio-wav", "/owned.wav", "--model-directory", "/owned", "--model-id", "other-model"],
                Array(args.dropLast()) + ["[\"C++\"]"]] {
        try rejects { _ = try Invocation.parse(bad) }
    }
}

func completeRIFFExtent() throws {
    var valid = wav()
    valid += Data([74, 85, 78, 75, 1, 0, 0, 0, 42, 0]) // padded JUNK
    func repairLength(_ data: inout Data) {
        let size = UInt32(data.count - 8)
        for index in 0..<4 { data[4 + index] = UInt8(truncatingIfNeeded: size >> (index * 8)) }
    }
    repairLength(&valid)
    try equal(try AudioInput(data: valid).samples.count, 3)
    var truncatedHeader = wav(); truncatedHeader += Data([74]); repairLength(&truncatedHeader)
    var truncatedBody = valid; truncatedBody.removeLast(2); repairLength(&truncatedBody)
    var missingPad = valid; missingPad.removeLast(); repairLength(&missingPad)
    var outsideRIFF = wav(); outsideRIFF += Data([1, 2])
    for bytes in [truncatedHeader, truncatedBody, missingPad, outsideRIFF] {
        try rejects { _ = try AudioInput(data: bytes) }
    }
}

func explicitLocalBundlePreflight() throws {
    let manager = FileManager.default
    let root = manager.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    try manager.createDirectory(at: root, withIntermediateDirectories: false)
    defer { try? manager.removeItem(at: root) }
    let request = try Invocation.parse(["--audio-wav", root.appendingPathComponent("input.wav").path,
                                       "--model-directory", root.path, "--model-id", Invocation.modelID])
    try rejects { try request.validateModelDirectory() }
    // Deliberately invalid model contents: only the filesystem preflight is tested.
    let bundles = ["parakeet_unified_encoder_int8.mlmodelc", "parakeet_unified_decoder.mlmodelc",
                   "parakeet_unified_joint_decision_single_step.mlmodelc"]
    let files = ["metadata.json", "vocab.json"] + bundles.flatMap { bundle in
        ["coremldata.bin", "model.mil", "weights/weight.bin"].map { bundle + "/" + $0 }
    }
    for path in files {
        let url = root.appendingPathComponent(path)
        try manager.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try Data([1]).write(to: url)
    }
    try request.validateModelDirectory()
    let weight = root.appendingPathComponent(files.last!)
    try Data().write(to: weight)
    try rejects { try request.validateModelDirectory() }
    try manager.removeItem(at: weight)
    try manager.createSymbolicLink(at: weight, withDestinationURL: root.appendingPathComponent("vocab.json"))
    try rejects { try request.validateModelDirectory() }
    let audio = root.appendingPathComponent("input.wav")
    try wav().write(to: audio)
    try equal(try AudioInput.load(audio).samples.count, 3)
    try manager.removeItem(at: audio)
    try manager.createSymbolicLink(at: audio, withDestinationURL: root.appendingPathComponent("vocab.json"))
    try rejects { _ = try AudioInput.load(audio) }
}

let cases: [(String, () throws -> Void)] = [
    ("exact PCM and source receipt", exactPCMAndSourceReceipt),
    ("malformed/truncated/unsupported WAV rejection", malformedTruncatedAndWrongFormatAudio),
    ("explicit arguments and unsupported hints", explicitArgumentsAndUnsupportedHints),
    ("complete RIFF extent including trailing metadata", completeRIFFExtent),
    ("explicit local bundle preflight and file handling", explicitLocalBundlePreflight),
]
var failures = 0
for (name, check) in cases {
    do { try check(); print("PASS: " + name) }
    catch { failures += 1; print("FAIL: " + name + " (" + String(describing: error) + ")") }
}
if failures != 0 { exit(1) }
