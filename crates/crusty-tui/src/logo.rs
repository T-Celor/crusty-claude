//! "Crusty" the mascot: a panko-breaded fried shrimp drawn as a pixel matrix.
//!
//! Each matrix character is one pixel. Two pixel rows are drawn per terminal cell with
//! the half-block characters, so the logo is `PIXELS.len() / 2` cells tall.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Pixel colours. `.` in `PIXELS` is transparent and never looked up here.
pub const PALETTE: &[(char, (u8, u8, u8))] = &[
    ('A', (0xE8, 0xA3, 0x3D)),
    ('B', (0xC9, 0x7B, 0x22)),
    ('C', (0xF4, 0xC7, 0x6A)),
    ('R', (0xE2, 0x51, 0x2B)),
    ('O', (0xF0, 0x7A, 0x3A)),
    ('E', (0x1E, 0x1E, 0x1E)),
];

/// The logo, 18 pixels wide and 12 tall. Head is at the right with the eye, tail fan at
/// the lower left.
pub const PIXELS: &[&str] = &[
    ".........CCCCC....",
    ".......CCAAAAAAC..",
    "....CAAAAAAAABBC..",
    "..CAAAAABBBBBAAC..",
    "..CAAAABBBBBBAEAC.",
    "RROOAAAABBBBBBAAC.",
    "ROOAAABBBBBAAAC...",
    "RROAAAABBBBAAAAC..",
    ".ROAABBBAAAC......",
    "..RRO.ABBAC.......",
    "...RO..CC.........",
    "....RR............",
];

/// Colour of a palette character, if it is in the palette.
fn colour(c: char) -> Option<Color> {
    PALETTE.iter().find(|(k, _)| *k == c).map(|(_, (r, g, b))| Color::Rgb(*r, *g, *b))
}

/// Renders `PIXELS` as terminal lines, two pixel rows per line with `▀` and `▄`.
/// Transparent pixels keep the terminal default colour.
pub fn logo_lines() -> Vec<Line<'static>> {
    let rows: Vec<Vec<char>> = PIXELS.iter().map(|r| r.chars().collect()).collect();
    let width = rows.first().map_or(0, Vec::len);
    let at = |row: usize, col: usize| -> Option<Color> { rows.get(row).and_then(|r| r.get(col)).and_then(|c| colour(*c)) };
    (0..rows.len().div_ceil(2))
        .map(|line| {
            let top = line * 2;
            let spans: Vec<Span<'static>> = (0..width)
                .map(|col| {
                    let (up, down) = (at(top, col), at(top + 1, col));
                    match (up, down) {
                        (None, None) => Span::raw(" "),
                        (Some(u), None) => Span::styled("▀", Style::new().fg(u)),
                        (None, Some(d)) => Span::styled("▄", Style::new().fg(d)),
                        (Some(u), Some(d)) => Span::styled("▀", Style::new().fg(u).bg(d)),
                    }
                })
                .collect();
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_have_equal_width() {
        let w = PIXELS[0].chars().count();
        assert!(PIXELS.iter().all(|r| r.chars().count() == w));
    }

    #[test]
    fn every_pixel_is_in_palette_or_transparent() {
        for row in PIXELS {
            for c in row.chars() {
                assert!(c == '.' || PALETTE.iter().any(|(k, _)| *k == c), "unknown pixel {c}");
            }
        }
    }

    #[test]
    fn renders_half_as_many_lines_as_pixel_rows() {
        assert_eq!(logo_lines().len(), PIXELS.len().div_ceil(2));
        assert_eq!(logo_lines().len(), 6);
    }
}
