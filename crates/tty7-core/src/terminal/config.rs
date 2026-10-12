use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle};

use crate::core::config::{Config as UserConfig, CursorStyle as UserCursorStyle};

pub fn terminal_config_from_user(user_config: &UserConfig) -> Config {
    let shape = match user_config.cursor_style {
        UserCursorStyle::Block => CursorShape::Block,
        UserCursorStyle::Bar => CursorShape::Beam,
        UserCursorStyle::Underline => CursorShape::Underline,
    };
    Config {
        scrolling_history: user_config.scrollback_limit,
        default_cursor_style: CursorStyle {
            shape,
            blinking: false,
        },
        semantic_escape_chars: user_config.word_separators.clone(),
        kitty_keyboard: true,
        // ConPTY repaints against conhost's pinned resize layout, not the
        // normal reflow layout. Callers with a raw remote PTY override this.
        conpty_resize: cfg!(windows),
        ..Config::default()
    }
}
