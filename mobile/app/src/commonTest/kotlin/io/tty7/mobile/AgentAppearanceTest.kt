package io.tty7.mobile

import kotlin.test.*

class AgentAppearanceTest {
    @Test fun daemonNamesResolveToDesktopMarks() {
        assertEquals("omp", agentAppearance("Oh My Pi")?.kind)
        assertEquals("codex", agentAppearance("Codex")?.kind)
        assertEquals("claude", agentAppearance("Claude Code")?.kind)
        assertEquals("qoderclicn", agentAppearance("Qoder CN CLI")?.kind)
        assertNotNull(agentMark("omp"))
        assertNotNull(agentMark("codex"))
    }

    @Test fun shellAndClearedAgentHaveNoBrandAvatar() {
        assertNull(agentAppearance(null))
        assertNull(agentAppearance("  "))
        assertEquals("omp", agentAppearance("OMP")?.kind)
        assertEquals("unknown", agentAppearance("Future Agent")?.kind)
    }

    @Test fun brandColorsKeepDesktopForegroundContrast() {
        assertEquals(0x000000, agentAppearance("Codex")?.field)
        assertEquals(0xFFFFFF, agentAppearance("Codex")?.ink)
        assertEquals(0x32F08C, agentAppearance("TraeCode")?.ink)
        assertEquals(0x000000, agentAppearance("Qoder CLI")?.ink)
    }
}
