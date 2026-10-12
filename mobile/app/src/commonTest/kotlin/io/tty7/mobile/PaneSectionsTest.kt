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
}
