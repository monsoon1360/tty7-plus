package io.tty7.mobile

internal fun mergeComputer(computers: List<Computer>, computer: Computer): List<Computer> =
    computers.filterNot { it.hostId == computer.hostId && (!computer.pending || it.pending) } + computer

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
