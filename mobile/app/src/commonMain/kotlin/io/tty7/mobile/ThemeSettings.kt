package io.tty7.mobile

import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.selection.selectable
import androidx.compose.foundation.selection.selectableGroup
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.*
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.unit.dp

@Composable
internal fun ThemeSettings(selected: MobileTheme, onSelect: (MobileTheme) -> Unit, onDismiss: () -> Unit) {
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text("设置") },
        text = {
            Column(Modifier.selectableGroup().testTag("theme-settings")) {
                Text("主题", style = MaterialTheme.typography.titleSmall)
                Text("切换后立即生效，并记住你的选择。", color = MaterialTheme.colorScheme.onSurfaceVariant)
                Spacer(Modifier.height(12.dp))
                MobileTheme.entries.forEach { theme ->
                    Row(
                        Modifier.fillMaxWidth().heightIn(min = 56.dp)
                            .selectable(selected = theme == selected, role = Role.RadioButton, onClick = { onSelect(theme) })
                            .testTag("theme-${theme.id}").padding(horizontal = 8.dp),
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(12.dp),
                    ) {
                        val preview = mobileColorScheme(theme.dark)
                        Box(
                            Modifier.size(36.dp).background(preview.surface, RoundedCornerShape(6.dp))
                                .border(1.dp, MaterialTheme.colorScheme.outline, RoundedCornerShape(6.dp)),
                            contentAlignment = Alignment.Center,
                        ) { Text("Aa", color = preview.onSurface, style = MaterialTheme.typography.labelMedium) }
                        Text(theme.title, Modifier.weight(1f))
                        RadioButton(selected = theme == selected, onClick = null)
                    }
                }
            }
        },
        confirmButton = { TextButton(onClick = onDismiss, modifier = Modifier.testTag("close-settings")) { Text("完成") } },
    )
}
