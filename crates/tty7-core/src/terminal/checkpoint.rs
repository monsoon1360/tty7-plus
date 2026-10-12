use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

use super::size::TermSize;

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Listener;
impl alacritty_terminal::event::EventListener for Listener {}

fn terminal() -> Term<Listener> {
    Term::new(Config::default(), &TermSize::new(16, 5), Listener)
}

#[test]
fn exec_checkpoint_at_every_byte_preserves_terminal_continuation() {
    // Each byte can be the last one consumed before exec: UTF-8, OSC, CSI
    // subparameters, saved cursors/charsets, REP, and synchronized updates all
    // have state which a screen-only serialization cannot reconstruct.
    let stream = concat!(
        "normal\x1b[2;4H界e\u{301}\x1b7\x1b[31mR\x1b8Q",
        "\x1b]2;标题\x1b\\\x1b[38:2::7:80:190m色\x1b[0m",
        "\x1b[?1049h\x1b[2;4r\x1b[4;1Hbottom\r\nnext",
        "\x1b(0qq\x1b7\x1b(Bx\x1b8q\x1b(B",
        "\x1b[?2026h\x1b[3;2Hα\x1b[3b\x1b[?2026l",
        "\x1b[?1049l\x1b[4;2Hdone\x1b[?1h\x1b[?2004h",
    )
    .as_bytes();
    let mut expected = terminal();
    let mut expected_parser: Processor = Processor::new();
    expected_parser.advance(&mut expected, stream);

    for split in 0..=stream.len() {
        let mut term = terminal();
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, &stream[..split]);
        let checkpoint = serde_json::to_vec(&(&term, &parser)).unwrap();
        let (mut restored, mut parser): (Term<Listener>, Processor) =
            serde_json::from_slice(&checkpoint).unwrap();
        restored.reset_checkpoint_damage();
        parser.advance(&mut restored, &stream[split..]);

        for row in 0..5 {
            for col in 0..16 {
                let point = Point::new(Line(row), Column(col));
                assert_eq!(
                    restored.grid()[point],
                    expected.grid()[point],
                    "cell {row}:{col} diverged at byte {split}"
                );
            }
        }
        assert_eq!(
            restored.grid().cursor.point,
            expected.grid().cursor.point,
            "cursor position at byte {split}"
        );
        assert_eq!(
            restored.grid().cursor.input_needs_wrap,
            expected.grid().cursor.input_needs_wrap,
            "pending wrap at byte {split}"
        );
        assert_eq!(restored.mode(), expected.mode(), "modes at byte {split}");
        assert_eq!(
            restored.cursor_style(),
            expected.cursor_style(),
            "cursor style at byte {split}"
        );
    }
}
