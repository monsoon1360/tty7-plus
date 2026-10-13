package io.tty7.mobile

import androidx.compose.material3.ColorScheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color

// Persist stable names so future themes do not depend on enum ordinals.
enum class MobileTheme(val id: String, val title: String, val dark: Boolean) {
    Dark("dark", "黑色", true),
    Light("light", "白色", false);

    companion object {
        fun fromId(id: String?): MobileTheme? = entries.firstOrNull { it.id == id }
    }
}

internal val LocalMobileDarkTheme = staticCompositionLocalOf { false }

internal fun mobileColorScheme(dark: Boolean): ColorScheme = if (dark) darkColorScheme(
    primary = Color(0xFFECECED), onPrimary = Color(0xFF18181A),
    primaryContainer = Color(0xFF29292C), onPrimaryContainer = Color(0xFFECECED),
    secondaryContainer = Color(0xFF29292C), onSecondaryContainer = Color(0xFFECECED),
    surfaceContainerHighest = Color(0xFF242427), surfaceContainerHigh = Color(0xFF242427),
    surfaceContainer = Color(0xFF1E1E20), surfaceContainerLow = Color(0xFF17171A),
    background = Color(0xFF17171A), surface = Color(0xFF1E1E20),
    onBackground = Color(0xFFECECED), onSurface = Color(0xFFECECED), onSurfaceVariant = Color(0xFF969698),
    outline = Color(0xFF757579), outlineVariant = Color(0xFF323235),
) else lightColorScheme(
    primary = Color(0xFF1C1C1E), onPrimary = Color(0xFFFCFCFB),
    primaryContainer = Color(0xFFECECE9), onPrimaryContainer = Color(0xFF1C1C1E),
    secondaryContainer = Color(0xFFECECE9), onSecondaryContainer = Color(0xFF1C1C1E),
    surfaceContainerHighest = Color(0xFFF3F3F1), surfaceContainerHigh = Color(0xFFF3F3F1),
    surfaceContainer = Color(0xFFFCFCFB), surfaceContainerLow = Color(0xFFFCFCFB),
    background = Color(0xFFFCFCFB), surface = Color(0xFFFCFCFB),
    onBackground = Color(0xFF1C1C1E), onSurface = Color(0xFF1C1C1E), onSurfaceVariant = Color(0xFF626264),
    outline = Color(0xFF858587), outlineVariant = Color(0xFFE4E4E1),
)


@Composable
internal fun MobileAppTheme(dark: Boolean, content: @Composable () -> Unit) {
    CompositionLocalProvider(LocalMobileDarkTheme provides dark) {
        MaterialTheme(colorScheme = mobileColorScheme(dark), content = content)
    }
}

@Composable
expect fun ApplySystemAppearance(dark: Boolean)
