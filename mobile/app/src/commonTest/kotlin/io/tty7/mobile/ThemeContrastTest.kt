package io.tty7.mobile

import androidx.compose.ui.graphics.Color
import kotlin.math.max
import kotlin.math.min
import kotlin.math.pow
import kotlin.test.Test
import kotlin.test.assertTrue

class ThemeContrastTest {
    @Test fun bothThemesKeepTextAndInputReadable() {
        MobileTheme.entries.forEach { theme ->
            val colors = mobileColorScheme(theme.dark)
            val pairs = listOf(
                "body" to (colors.onSurface to colors.surface),
                "background" to (colors.onBackground to colors.background),
                "secondary text" to (colors.onSurfaceVariant to colors.surface),
                "search placeholder" to (colors.onSurfaceVariant to colors.surfaceContainerHighest),
                "search input" to (colors.onSurface to colors.surfaceContainerHighest),
                "input cursor" to (colors.primary to colors.surfaceContainerHighest),
                "button" to (colors.onPrimary to colors.primary),
                "selection" to (colors.onSecondaryContainer to colors.secondaryContainer),
                "error" to (colors.error to colors.surface),
            )
            pairs.forEach { (label, pair) ->
                val ratio = contrast(pair.first, pair.second)
                assertTrue(ratio >= 4.5, "${theme.title} $label contrast: $ratio")
            }
        }
    }

    private fun contrast(foreground: Color, background: Color): Double {
        fun luminance(color: Color): Double {
            fun channel(value: Float): Double = value.toDouble().let {
                if (it <= 0.04045) it / 12.92 else ((it + 0.055) / 1.055).pow(2.4)
            }
            return channel(color.red) * 0.2126 + channel(color.green) * 0.7152 + channel(color.blue) * 0.0722
        }
        val fg = luminance(foreground)
        val bg = luminance(background)
        return (max(fg, bg) + 0.05) / (min(fg, bg) + 0.05)
    }
}
