package io.tty7.mobile

// Legacy storage was ordered by the last successful pairing/authentication.
// Prefer the last usable credential over an unfinished QR, then the latest QR.
internal fun retainedComputer(computers: List<Computer>): Computer? =
    computers.lastOrNull { !it.pending } ?: computers.lastOrNull()

internal const val UNPAIR_BEFORE_PAIRING = "Unpair the current computer before pairing another."

internal fun updatedComputer(current: Computer?, computer: Computer, newPair: Boolean = false): Computer {
    if (newPair) check(current == null) { UNPAIR_BEFORE_PAIRING }
    else {
        check(current != null && current.hostId == computer.hostId) { "Computer is no longer paired" }
        check(current.pending || current.credentialId == computer.credentialId) { UNPAIR_BEFORE_PAIRING }
        check(current.pending || !computer.pending) { UNPAIR_BEFORE_PAIRING }
    }
    return computer
}

internal class ForegroundLifetime {
    var active = false
        private set
    var generation = 0L
        private set
    fun setForeground(foreground: Boolean) {
        active = foreground
        if (!foreground) invalidate()
    }
    fun invalidate() { generation++ }
    fun accepts(generation: Long) = active && this.generation == generation
}

internal class RetiredRequestIds(private val capacity: Int = 128) {
    private val ids = linkedSetOf<Long>()
    fun add(id: Long) {
        ids.add(id)
        if (ids.size > capacity) ids.remove(ids.first())
    }
    fun consume(id: Long) = ids.remove(id)
    fun clear() = ids.clear()
}
