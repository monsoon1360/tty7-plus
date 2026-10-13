package io.tty7.mobile

import kotlin.test.*

class PaneSectionsTest {
    private fun pane(id: Long, group: String? = null, groupId: String? = null) =
        Pane(id, "zsh", "/home/user", 80, 24, group = group, groupId = groupId)

    @Test fun newUngroupedTerminalHasItsOwnHeaderAfterLastProject() {
        val sections = paneSections(listOf(
            pane(1, "project", "auto:project"), pane(2), pane(3),
        ))
        assertEquals(listOf("project", "未分组"), sections.map { it.title })
        assertEquals(listOf(2L, 3L), sections.last().panes.map { it.id })
    }

    @Test fun workspaceWithOnlyUngroupedTerminalsStillShowsHeader() {
        val section = paneSections(listOf(pane(1))).single()
        assertEquals("未分组", section.title)
        assertNull(section.name)
    }

    @Test fun adjacentEqualNamedGroupsKeepDesktopOrderAndMembership() {
        val sections = paneSections(listOf(
            pane(1, "project", "pinned:first"),
            pane(2, "project", "pinned:second"),
            pane(3, "project", "auto:project"), pane(4),
        ))
        assertEquals(listOf(listOf(1L), listOf(2L), listOf(3L), listOf(4L)),
            sections.map { section -> section.panes.map { it.id } })
        assertEquals(listOf("project", "project", "project", "未分组"), sections.map { it.title })
    }

    @Test fun terminalHeaderFollowsGroupAndLiveAgentLifecycle() {
        val shell = pane(1, "tty7-plus", "auto:tty7-plus")
        assertEquals("tty7-plus", terminalTitle(shell))
        val agent = shell.copy(agentName = "Codex", agentStatus = "working")
        assertNull(terminalAgentTitle(shell))
        assertEquals("复盘实战数据", terminalAgentTitle(agent.copy(title = "复盘实战数据")))
        assertEquals("Codex", terminalAgentTitle(agent.copy(title = "")))
        assertEquals("tty7-plus", terminalTitle(agent))
        assertEquals("tty7-plus", terminalTitle(agent.copy(agentStatus = "waiting")))
        assertEquals("tty7-plus", terminalTitle(agent.copy(agentStatus = "done")))
        assertEquals("tty7-plus", terminalTitle(agent.copy(agentName = null, agentStatus = null)))
        assertEquals("renamed", terminalTitle(shell.copy(group = "renamed")))
        assertEquals("renamed", terminalTitle(agent.copy(group = "renamed")))
    }

    @Test fun terminalHeaderNormalizesEmptyAndUngroupedNames() {
        for (group in listOf(null, "", " ", "Ungrouped")) {
            assertEquals("未分组", terminalTitle(pane(1, group).copy(agentName = " ")))
        }
    }
}
