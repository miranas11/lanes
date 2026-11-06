//! Pure geometry for screens (several windows sharing one stop in the row).
//! No AppKit or Accessibility here, so it is cheap and fully unit-tested.

use std::collections::HashSet;

/// A rectangle as shares of the screen: x, y, width, height (0…1).
pub type Rect = [f64; 4];

/// Arrow bits. One arrow picks a half, two at right angles a corner.
pub const LEFT: u8 = 1;
pub const RIGHT: u8 = 2;
pub const UP: u8 = 4;
pub const DOWN: u8 = 8;

/// How each area is shown on the map, in the same order as `AREAS`.
/// The middle (full screen) is not offered: its entry is empty and unused.
pub const AREA_ARROWS: [&str; 9] = ["↑ ←", "↑", "↑ →", "←", "", "→", "↓ ←", "↓", "↓ →"];

/// The full-screen area, which the map leaves out.
pub const MIDDLE: usize = 4;

/// Which area a set of arrows means.
pub fn area_for_arrows(mask: u8) -> Option<usize> {
    let h = mask & (LEFT | RIGHT);
    let v = mask & (UP | DOWN);
    // Opposite arrows have no meaning (the middle is not offered).
    if h == LEFT | RIGHT || v == UP | DOWN {
        return None;
    }
    Some(match (v, h) {
        (UP, LEFT) => 0,
        (UP, 0) => 1,
        (UP, RIGHT) => 2,
        (0, LEFT) => 3,
        (0, RIGHT) => 5,
        (DOWN, LEFT) => 6,
        (DOWN, 0) => 7,
        (DOWN, RIGHT) => 8,
        _ => return None,
    })
}

/// True when two single arrows are at right angles (they make a corner).
pub fn at_right_angles(a: u8, b: u8) -> bool {
    ((a & (LEFT | RIGHT)) != 0) != ((b & (LEFT | RIGHT)) != 0)
}

/// Corners are quarters, edges are halves, the middle is full.
pub const AREAS: [Rect; 9] = [
    [0.0, 0.0, 0.5, 0.5],
    [0.0, 0.0, 1.0, 0.5],
    [0.5, 0.0, 0.5, 0.5],
    [0.0, 0.0, 0.5, 1.0],
    [0.0, 0.0, 1.0, 1.0],
    [0.5, 0.0, 0.5, 1.0],
    [0.0, 0.5, 0.5, 0.5],
    [0.0, 0.5, 1.0, 0.5],
    [0.5, 0.5, 0.5, 0.5],
];

pub const AREA_NAMES: [&str; 9] = [
    "Top-left quarter",
    "Top half",
    "Top-right quarter",
    "Left half",
    "Full screen",
    "Right half",
    "Bottom-left quarter",
    "Bottom half",
    "Bottom-right quarter",
];

const EPS: f64 = 1e-6;

pub fn overlaps(a: Rect, b: Rect) -> bool {
    a[0] < b[0] + b[2] - EPS
        && b[0] < a[0] + a[2] - EPS
        && a[1] < b[1] + b[3] - EPS
        && b[1] < a[1] + a[3] - EPS
}

/// What is left of `r` once `n` takes part of it, when that is one rectangle:
/// a left half that loses its bottom quarter keeps the top quarter. None when
/// `n` covers `r` completely (or would cut it into an L shape).
pub fn subtract(r: Rect, n: Rect) -> Option<Rect> {
    if !overlaps(r, n) {
        return Some(r);
    }
    let (rx0, ry0, rx1, ry1) = (r[0], r[1], r[0] + r[2], r[1] + r[3]);
    let (nx0, ny0, nx1, ny1) = (n[0], n[1], n[0] + n[2], n[1] + n[3]);
    let spans_x = nx0 <= rx0 + EPS && nx1 >= rx1 - EPS;
    let spans_y = ny0 <= ry0 + EPS && ny1 >= ry1 - EPS;
    match (spans_x, spans_y) {
        (true, true) => None,
        (true, false) if ny0 <= ry0 + EPS => Some([rx0, ny1, r[2], ry1 - ny1]),
        (true, false) if ny1 >= ry1 - EPS => Some([rx0, ry0, r[2], ny0 - ry0]),
        (false, true) if nx0 <= rx0 + EPS => Some([nx1, ry0, rx1 - nx1, r[3]]),
        (false, true) if nx1 >= rx1 - EPS => Some([rx0, ry0, nx0 - rx0, r[3]]),
        _ => None,
    }
}

/// Does the union of `rects` cover the whole screen? Sampled on a 10 × 10 grid,
/// which is exact for the halves and quarters Lanes uses.
pub fn covers_all(rects: &[Rect]) -> bool {
    (0..10).all(|i| {
        (0..10).all(|j| {
            let (x, y) = (0.05 + i as f64 * 0.1, 0.05 + j as f64 * 0.1);
            rects
                .iter()
                .any(|r| x >= r[0] && x <= r[0] + r[2] && y >= r[1] && y <= r[1] + r[3])
        })
    })
}

/// Reading order inside a screen: top to bottom, then left to right.
pub fn reading_key(r: Rect) -> (i64, i64) {
    (
        (r[1] * 1000.0).round() as i64,
        (r[0] * 1000.0).round() as i64,
    )
}

/// The frame of one window inside a screen that occupies x, y, w, h in points,
/// keeping the same gap between windows as the row uses.
pub fn member_frame(x: f64, y: f64, w: f64, h: f64, r: Rect, gap: f64) -> (f64, f64, f64, f64) {
    let fx = x + r[0] * (w + gap);
    let fy = y + r[1] * (h + gap);
    let fw = r[2] * (w + gap) - gap;
    let fh = r[3] * (h + gap) - gap;
    (fx.round(), fy.round(), fw.round(), fh.round())
}

/// One letter per app: its first letter, else the next free letter of its
/// name, else the first free letter of the alphabet. The same names in the
/// same order always get the same letters.
pub fn letters(names: &[&str]) -> Vec<char> {
    let mut used = HashSet::new();
    names
        .iter()
        .map(|name| {
            let c = name
                .chars()
                .filter(char::is_ascii_alphabetic)
                .map(|c| c.to_ascii_uppercase())
                .chain('A'..='Z')
                .find(|c| !used.contains(c))
                .unwrap_or('?');
            used.insert(c);
            c
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halves_make_a_full_screen() {
        assert!(covers_all(&[AREAS[3], AREAS[5]]));
        assert!(covers_all(&[AREAS[3], AREAS[2], AREAS[8]]));
        assert!(!covers_all(&[AREAS[3]]));
        assert!(!covers_all(&[AREAS[0], AREAS[2], AREAS[6]]));
    }

    #[test]
    fn quarters_overlap_their_halves_only() {
        assert!(overlaps(AREAS[0], AREAS[3]));
        assert!(!overlaps(AREAS[0], AREAS[5]));
        assert!(!overlaps(AREAS[3], AREAS[5]));
    }

    #[test]
    fn arrows_pick_halves_corners_and_full() {
        assert_eq!(area_for_arrows(LEFT), Some(3));
        assert_eq!(area_for_arrows(DOWN), Some(7));
        assert_eq!(area_for_arrows(UP | LEFT), Some(0));
        assert_eq!(area_for_arrows(DOWN | RIGHT), Some(8));
        assert_eq!(area_for_arrows(LEFT | RIGHT), None);
        assert_eq!(area_for_arrows(UP | DOWN | LEFT), None);
        assert_eq!(area_for_arrows(0), None);
        assert!(at_right_angles(LEFT, UP));
        assert!(!at_right_angles(LEFT, RIGHT));
        // Every area on the map is reachable, and the middle never is.
        let all: std::collections::HashSet<usize> = (1u8..16).filter_map(area_for_arrows).collect();
        assert_eq!(all.len(), 8);
        assert!(!all.contains(&MIDDLE));
    }

    #[test]
    fn a_new_app_shrinks_the_window_it_overlaps() {
        // Left half, then an app in the bottom-left: the window keeps the top-left.
        assert_eq!(subtract(AREAS[3], AREAS[6]), Some(AREAS[0]));
        // Top half, then the top-right corner: it keeps the top-left.
        assert_eq!(subtract(AREAS[1], AREAS[2]), Some(AREAS[0]));
        // Right half, then the top half: it keeps the bottom-right.
        assert_eq!(subtract(AREAS[5], AREAS[1]), Some(AREAS[8]));
        // Fully covered: it has to leave the screen.
        assert_eq!(subtract(AREAS[0], AREAS[3]), None);
        // Not touching: unchanged.
        assert_eq!(subtract(AREAS[3], AREAS[5]), Some(AREAS[3]));
    }

    #[test]
    fn letters_follow_names_and_never_clash() {
        let l = letters(&["Slack", "Spotify", "Chrome", "Chrome", "\u{200e}WhatsApp"]);
        assert_eq!(l, vec!['S', 'P', 'C', 'H', 'W']);
    }

    #[test]
    fn windows_in_a_screen_keep_the_row_gap() {
        // Two halves of a 1000 × 800 screen with an 8 pt gap.
        let a = member_frame(0.0, 0.0, 1000.0, 800.0, AREAS[3], 8.0);
        let b = member_frame(0.0, 0.0, 1000.0, 800.0, AREAS[5], 8.0);
        assert_eq!(a, (0.0, 0.0, 496.0, 800.0));
        assert_eq!(b, (504.0, 0.0, 496.0, 800.0));
    }
}
