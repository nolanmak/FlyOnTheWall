// A fixed set of engine instances, one per concurrent stream.
//
// Some FluidAudio streaming managers keep a single stream's state (audio
// buffer, encoder caches, decoder state) inside the manager itself, so two
// streams cannot share one. The pool loads `capacity` of them up front (each
// warmed, so no stream pays Core ML's first-run cost), lends one to each
// stream, and takes it back, reset, when the stream ends however it ends.
import Foundation

public actor SessionPool<Instance: Sendable> {
    private var idle: [Instance] = []
    private var lent = 0
    public let capacity: Int

    /// Build `capacity` instances with `make`, one after another, warming each.
    public init(capacity: Int, make: @Sendable () async throws -> Instance,
                warm: @Sendable (Instance) async throws -> Void) async throws {
        precondition(capacity > 0, "a pool needs at least one instance")
        self.capacity = capacity
        var built: [Instance] = []
        for _ in 0..<capacity {
            let instance = try await make()
            try await warm(instance)
            built.append(instance)
        }
        idle = built
    }

    /// Lend an instance, or refuse with `busy` when every one is in use.
    public func checkout() throws -> Instance {
        guard let instance = idle.popLast() else {
            throw SpeechdError("busy", "all \(capacity) sessions of this engine are in use")
        }
        lent += 1
        return instance
    }

    /// Lend an instance, waiting up to `timeout` for one to come back. A
    /// stream returns its instance from its own task as it ends, so a stream
    /// opened just after another was cancelled may arrive a moment early.
    public func checkout(within timeout: Duration) async throws -> Instance {
        let deadline = ContinuousClock.now + timeout
        while true {
            if let instance = idle.popLast() {
                lent += 1
                return instance
            }
            guard ContinuousClock.now < deadline else { return try checkout() }
            try await Task.sleep(for: .milliseconds(5))
        }
    }

    /// Take an instance back. The caller has reset it.
    public func checkin(_ instance: Instance) {
        lent -= 1
        idle.append(instance)
    }

    public var available: Int { idle.count }
    public var inUse: Int { lent }

    /// Every instance, idle ones only, for shutdown. Lent ones are released
    /// by their streams' close.
    public func drain() -> [Instance] {
        defer { idle.removeAll() }
        return idle
    }
}
