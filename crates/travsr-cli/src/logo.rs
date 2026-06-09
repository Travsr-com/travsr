//! Real-logo banner for `travsr --help`.
//!
//! Renders the actual brand favicon as truecolor half-block art, embedded from
//! `assets/logo.ansi` (generated offline from `design/logo/orange fav icon.png`
//! — the `design/` originals are gitignored, so a committed copy is required).
//!
//! This uses only ANSI SGR color + Unicode half-blocks (`▀`), so the real logo
//! renders on any truecolor terminal — including VTE/GNOME Terminal, which does
//! not support the kitty/iTerm2/sixel image protocols — and it survives clap's
//! `before_help` (which keeps SGR but strips image-protocol escapes).
//!
//! Falls back to the geometric motif in [`crate::progress::banner`] when color
//! is unavailable (piped output or `NO_COLOR`), so it never breaks anywhere.

use std::io::IsTerminal;

/// Half-block ANSI art of the brand favicon (16 cols x 8 rows).
const LOGO: &str = include_str!("../assets/logo.ansi");

/// The `--help` header: the real logo on a color terminal, else the motif.
pub fn banner() -> String {
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    if !color {
        // Plain terminals / pipes / NO_COLOR: motif already carries the wordmark.
        return crate::progress::banner();
    }
    // Indent the art for a left margin, with the bold wordmark beneath it.
    let art = LOGO
        .lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("\n{art}\n  \x1b[1mtravsr\x1b[0m\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logo_art_is_embedded() {
        assert!(LOGO.contains('▀'), "art should use half-block glyphs");
        assert!(
            LOGO.contains("\x1b[38;2;"),
            "art should carry truecolor SGR"
        );
        let rows = LOGO.lines().count();
        assert!((8..=40).contains(&rows), "art has a sane row count: {rows}");
    }
}
