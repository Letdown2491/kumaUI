//! Vector glyphs for the classes no mono font renders reliably: box
//! drawing, block elements, and braille. Fonts draw the heavy and double
//! line styles with side bearings, so joins gap and grids garble; solid
//! blocks refuse to stack seamlessly. Terminals that care (kitty) draw
//! these classes themselves, and so do we: exact cell geometry, font
//! independent, identical on every distro.
//!
//! Everything here is pure rect math in cell-local pixels; the view paints
//! each rect as a quad behind the row text. Shades and diagonals stay on
//! shaped text (their texture is the point), as do powerline arrows until
//! a path-fill pass lands.

/// One painted rect in cell-local pixels: (x, y, w, h).
pub type Rect = (f32, f32, f32, f32);

/// Is this cell drawn by us instead of the shaper?
pub fn is_vector_glyph(c: char) -> bool {
    let u = c as u32;
    (0x2500..=0x257F).contains(&u) && box_arms(c).is_some()
        || (0x2580..=0x2590).contains(&u)
        || (0x2594..=0x2595).contains(&u)
        || (0x2596..=0x259F).contains(&u)
        || (0x2800..=0x28FF).contains(&u)
}

#[derive(Clone, Copy, PartialEq)]
enum Arm {
    None,
    Light,
    Heavy,
    Double,
    LightHalf,
    HeavyHalf,
}

#[derive(Clone, Copy)]
struct Arms {
    up: Arm,
    down: Arm,
    left: Arm,
    right: Arm,
}

const N: Arm = Arm::None;
const LT: Arm = Arm::Light;
const HV: Arm = Arm::Heavy;
const DB: Arm = Arm::Double;
const LH: Arm = Arm::LightHalf;
const HH: Arm = Arm::HeavyHalf;

/// Arm decomposition for the box drawing block. Rounded corners
/// (U+256D..2570), diagonals (2571..2573), dashes and double-dashes are
/// None: they stay on shaped text.
fn box_arms(c: char) -> Option<Arms> {
    let a = match c as u32 {
        0x2500 => Arms { up: N, down: N, left: LT, right: LT },
        0x2501 => Arms { up: N, down: N, left: HV, right: HV },
        0x2502 => Arms { up: LT, down: LT, left: N, right: N },
        0x2503 => Arms { up: HV, down: HV, left: N, right: N },
        0x250C => Arms { up: N, down: LT, left: N, right: LT },
        0x250D => Arms { up: N, down: LT, left: N, right: HV },
        0x250E => Arms { up: N, down: HV, left: N, right: LT },
        0x250F => Arms { up: N, down: HV, left: N, right: HV },
        0x2510 => Arms { up: N, down: LT, left: LT, right: N },
        0x2511 => Arms { up: N, down: LT, left: HV, right: N },
        0x2512 => Arms { up: N, down: HV, left: LT, right: N },
        0x2513 => Arms { up: N, down: HV, left: HV, right: N },
        0x2514 => Arms { up: LT, down: N, left: N, right: LT },
        0x2515 => Arms { up: LT, down: N, left: N, right: HV },
        0x2516 => Arms { up: HV, down: N, left: N, right: LT },
        0x2517 => Arms { up: HV, down: N, left: N, right: HV },
        0x2518 => Arms { up: LT, down: N, left: LT, right: N },
        0x2519 => Arms { up: LT, down: N, left: HV, right: N },
        0x251A => Arms { up: HV, down: N, left: LT, right: N },
        0x251B => Arms { up: HV, down: N, left: HV, right: N },
        0x251C => Arms { up: LT, down: LT, left: N, right: LT },
        0x251D => Arms { up: LT, down: LT, left: N, right: HV },
        0x251E => Arms { up: HV, down: LT, left: N, right: LT },
        0x251F => Arms { up: LT, down: HV, left: N, right: LT },
        0x2520 => Arms { up: HV, down: HV, left: N, right: LT },
        0x2521 => Arms { up: HV, down: LT, left: N, right: HV },
        0x2522 => Arms { up: LT, down: HV, left: N, right: HV },
        0x2523 => Arms { up: HV, down: HV, left: N, right: HV },
        0x2524 => Arms { up: LT, down: LT, left: LT, right: N },
        0x2525 => Arms { up: LT, down: LT, left: HV, right: N },
        0x2526 => Arms { up: HV, down: LT, left: LT, right: N },
        0x2527 => Arms { up: LT, down: HV, left: LT, right: N },
        0x2528 => Arms { up: HV, down: HV, left: LT, right: N },
        0x2529 => Arms { up: HV, down: LT, left: HV, right: N },
        0x252A => Arms { up: LT, down: HV, left: HV, right: N },
        0x252B => Arms { up: HV, down: HV, left: HV, right: N },
        0x252C => Arms { up: N, down: LT, left: LT, right: LT },
        0x252D => Arms { up: N, down: LT, left: HV, right: LT },
        0x252E => Arms { up: N, down: LT, left: LT, right: HV },
        0x252F => Arms { up: N, down: LT, left: HV, right: HV },
        0x2530 => Arms { up: N, down: HV, left: LT, right: LT },
        0x2531 => Arms { up: N, down: HV, left: HV, right: LT },
        0x2532 => Arms { up: N, down: HV, left: LT, right: HV },
        0x2533 => Arms { up: N, down: HV, left: HV, right: HV },
        0x2534 => Arms { up: LT, down: N, left: LT, right: LT },
        0x2535 => Arms { up: LT, down: N, left: HV, right: LT },
        0x2536 => Arms { up: LT, down: N, left: LT, right: HV },
        0x2537 => Arms { up: LT, down: N, left: HV, right: HV },
        0x2538 => Arms { up: HV, down: N, left: LT, right: LT },
        0x2539 => Arms { up: HV, down: N, left: HV, right: LT },
        0x253A => Arms { up: HV, down: N, left: LT, right: HV },
        0x253B => Arms { up: HV, down: N, left: HV, right: HV },
        0x253C => Arms { up: LT, down: LT, left: LT, right: LT },
        0x253D => Arms { up: LT, down: LT, left: HV, right: LT },
        0x253E => Arms { up: LT, down: LT, left: LT, right: HV },
        0x253F => Arms { up: LT, down: LT, left: HV, right: HV },
        0x2540 => Arms { up: HV, down: LT, left: LT, right: LT },
        0x2541 => Arms { up: LT, down: HV, left: LT, right: LT },
        0x2542 => Arms { up: HV, down: HV, left: LT, right: LT },
        0x2543 => Arms { up: HV, down: LT, left: HV, right: LT },
        0x2544 => Arms { up: HV, down: LT, left: LT, right: HV },
        0x2545 => Arms { up: LT, down: HV, left: HV, right: LT },
        0x2546 => Arms { up: LT, down: HV, left: LT, right: HV },
        0x2547 => Arms { up: HV, down: LT, left: HV, right: HV },
        0x2548 => Arms { up: LT, down: HV, left: HV, right: HV },
        0x2549 => Arms { up: HV, down: HV, left: HV, right: LT },
        0x254A => Arms { up: HV, down: HV, left: LT, right: HV },
        0x254B => Arms { up: HV, down: HV, left: HV, right: HV },
        0x2550 => Arms { up: N, down: N, left: DB, right: DB },
        0x2551 => Arms { up: DB, down: DB, left: N, right: N },
        0x2552 => Arms { up: N, down: LT, left: N, right: DB },
        0x2553 => Arms { up: N, down: DB, left: N, right: LT },
        0x2554 => Arms { up: N, down: DB, left: N, right: DB },
        0x2555 => Arms { up: N, down: LT, left: DB, right: N },
        0x2556 => Arms { up: N, down: DB, left: LT, right: N },
        0x2557 => Arms { up: N, down: DB, left: DB, right: N },
        0x2558 => Arms { up: LT, down: N, left: N, right: DB },
        0x2559 => Arms { up: DB, down: N, left: N, right: LT },
        0x255A => Arms { up: DB, down: N, left: N, right: DB },
        0x255B => Arms { up: LT, down: N, left: DB, right: N },
        0x255C => Arms { up: DB, down: N, left: LT, right: N },
        0x255D => Arms { up: DB, down: N, left: DB, right: N },
        0x255E => Arms { up: LT, down: LT, left: N, right: DB },
        0x255F => Arms { up: DB, down: DB, left: N, right: LT },
        0x2560 => Arms { up: DB, down: DB, left: N, right: DB },
        0x2561 => Arms { up: LT, down: LT, left: DB, right: N },
        0x2562 => Arms { up: DB, down: DB, left: LT, right: N },
        0x2563 => Arms { up: DB, down: DB, left: DB, right: N },
        0x2564 => Arms { up: N, down: LT, left: DB, right: DB },
        0x2565 => Arms { up: N, down: DB, left: LT, right: LT },
        0x2566 => Arms { up: N, down: DB, left: DB, right: DB },
        0x2567 => Arms { up: LT, down: N, left: DB, right: DB },
        0x2568 => Arms { up: DB, down: N, left: LT, right: LT },
        0x2569 => Arms { up: DB, down: N, left: DB, right: DB },
        0x256A => Arms { up: LT, down: LT, left: DB, right: DB },
        0x256B => Arms { up: DB, down: DB, left: LT, right: LT },
        0x256C => Arms { up: DB, down: DB, left: DB, right: DB },
        0x2574 => Arms { up: N, down: N, left: LH, right: N },
        0x2575 => Arms { up: LH, down: N, left: N, right: N },
        0x2576 => Arms { up: N, down: N, left: N, right: LH },
        0x2577 => Arms { up: N, down: LH, left: N, right: N },
        0x2578 => Arms { up: N, down: N, left: HH, right: N },
        0x2579 => Arms { up: HH, down: N, left: N, right: N },
        0x257A => Arms { up: N, down: N, left: N, right: HH },
        0x257B => Arms { up: N, down: HH, left: N, right: N },
        0x257C => Arms { up: N, down: N, left: LH, right: HV },
        0x257D => Arms { up: LH, down: HV, left: N, right: N },
        0x257E => Arms { up: N, down: N, left: HH, right: LT },
        0x257F => Arms { up: HH, down: LT, left: N, right: N },
        _ => return None,
    };
    Some(a)
}

fn h_rect(x0: f32, x1: f32, y: f32, t: f32) -> Rect {
    (x0, y - t / 2.0, x1 - x0, t)
}

fn v_rect(y0: f32, y1: f32, x: f32, t: f32) -> Rect {
    (x - t / 2.0, y0, t, y1 - y0)
}

/// Same full-length style on both sides: the two stubs are one line.
fn combine(a: Arm, b: Arm) -> Option<Arm> {
    match (a, b) {
        (Arm::Light, Arm::Light) => Some(Arm::Light),
        (Arm::Heavy, Arm::Heavy) => Some(Arm::Heavy),
        (Arm::Double, Arm::Double) => Some(Arm::Double),
        _ => None,
    }
}

/// Stroke thickness in whole pixels: light 1, heavy 2 at typical sizes,
/// scaling up on large cells.
fn stroke_px(th: f32) -> f32 {
    (th - 0.25).round().max(1.0)
}

/// Draw one arm's line(s) along `span` (start, end on the line axis),
/// centered on `cross` (the perpendicular axis position). Double styles
/// draw two lines with a stroke-width gap between them. Positions and
/// thicknesses land on whole pixels.
fn push_lines(out: &mut Vec<Rect>, arm: Arm, horizontal: bool, span: (f32, f32), cross: f32, t: f32) {
    let (count, th) = match arm {
        Arm::Light | Arm::LightHalf => (1, stroke_px(t)),
        Arm::Heavy | Arm::HeavyHalf => (1, stroke_px(t * 2.0)),
        Arm::Double => (2, stroke_px(t)),
        Arm::None => return,
    };
    let (s0, s1) = (span.0.round(), span.1.round());
    for i in 0..count {
        let off = if count == 2 { (i as f32 - 0.5) * t * 3.0 } else { 0.0 };
        let c = (cross + off - th / 2.0).round();
        if horizontal {
            out.push((s0, c, (s1 - s0).max(1.0), th));
        } else {
            out.push((c, s0, th, (s1 - s0).max(1.0)));
        }
    }
}

/// All rects for one box drawing cell. Full-length arms collapse into a
/// single line through the cell; stubs run edge to center and overlap the
/// center by half a stroke so corners and tees are seamless.
fn box_rects(arms: Arms, w: f32, h: f32) -> Vec<Rect> {
    let t = (h / 16.0).max(1.0);
    let cx = w / 2.0;
    let cy = h / 2.0;
    let mut out = Vec::new();

    if let Some(arm) = combine(arms.left, arms.right) {
        push_lines(&mut out, arm, true, (0.0, w), cy, t);
    } else {
        for (arm, from_left) in [(arms.left, true), (arms.right, false)] {
            if arm == Arm::None {
                continue;
            }
            let half = matches!(arm, Arm::LightHalf | Arm::HeavyHalf);
            let span = match (from_left, half) {
                (true, true) => (0.0, cx),
                (true, false) => (0.0, cx + t / 2.0),
                (false, true) => (cx, w),
                (false, false) => (cx - t / 2.0, w),
            };
            push_lines(&mut out, arm, true, span, cy, t);
        }
    }
    if let Some(arm) = combine(arms.up, arms.down) {
        push_lines(&mut out, arm, false, (0.0, h), cx, t);
    } else {
        for (arm, from_top) in [(arms.up, true), (arms.down, false)] {
            if arm == Arm::None {
                continue;
            }
            let half = matches!(arm, Arm::LightHalf | Arm::HeavyHalf);
            let span = match (from_top, half) {
                (true, true) => (0.0, cy),
                (true, false) => (0.0, cy + t / 2.0),
                (false, true) => (cy, h),
                (false, false) => (cy - t / 2.0, h),
            };
            push_lines(&mut out, arm, false, span, cx, t);
        }
    }
    out
}

/// All rects for one block element cell.
fn block_rects(c: char, w: f32, h: f32) -> Vec<Rect> {
    let u = c as u32;
    let full = |y0: f32, y1: f32| -> Rect { (0.0, y0, w, y1 - y0) };
    match u {
        0x2580 => vec![full(0.0, h / 2.0)],
        0x2581..=0x2587 => {
            let k = (u - 0x2580) as f32;
            vec![full(h - h * k / 8.0, h)]
        }
        0x2588 => vec![(0.0, 0.0, w, h)],
        0x2589..=0x258F => {
            let k = (u - 0x2588) as f32;
            vec![(0.0, 0.0, w * k / 8.0, h)]
        }
        0x258C => vec![(0.0, 0.0, w / 2.0, h)],
        0x2590 => vec![(w / 2.0, 0.0, w / 2.0, h)],
        0x2594 => vec![full(0.0, h / 8.0)],
        0x2595 => vec![(w - w / 8.0, 0.0, w / 8.0, h)],
        0x2596..=0x259F => {
            let (ul, ur, ll, lr) = (
                (0.0, 0.0, w / 2.0, h / 2.0),
                (w / 2.0, 0.0, w / 2.0, h / 2.0),
                (0.0, h / 2.0, w / 2.0, h / 2.0),
                (w / 2.0, h / 2.0, w / 2.0, h / 2.0),
            );
            match u {
                0x2596 => vec![ll],
                0x2597 => vec![lr],
                0x2598 => vec![ul],
                0x2599 => vec![ul, ll, lr],
                0x259A => vec![ul, lr],
                0x259B => vec![ul, ur, ll],
                0x259C => vec![ul, ur, lr],
                0x259D => vec![ur],
                0x259E => vec![ur, ll],
                _ => vec![ur, ll, lr],
            }
        }
        _ => vec![],
    }
}

/// All rects for one braille cell: a 2x4 dot grid, standard dot numbering
/// (1,2,3 down the left, 4,5,6 down the right, 7 and 8 along the bottom).
fn braille_rects(c: char, w: f32, h: f32) -> Vec<Rect> {
    let bits = c as u32 - 0x2800;
    if bits == 0 {
        return vec![];
    }
    let side = (w / 3.2).min(h / 6.5).max(1.5);
    let xs = [w * 0.25 - side / 2.0, w * 0.75 - side / 2.0];
    // (bit index, column, row)
    let dots = [
        (0, 0usize, 0usize),
        (1, 0, 1),
        (2, 0, 2),
        (3, 1, 0),
        (4, 1, 1),
        (5, 1, 2),
        (6, 0, 3),
        (7, 1, 3),
    ];
    let mut out = Vec::new();
    for (bit, col, row) in dots {
        if bits & (1 << bit) != 0 {
            let y = h * (row as f32 + 0.5) / 4.0 - side / 2.0;
            out.push((xs[col], y, side, side));
        }
    }
    out
}

fn snap(r: Rect) -> Rect {
    let (x, y, w, h) = r;
    let (x0, y0, x1, y1) = (x.round(), y.round(), (x + w).round(), (y + h).round());
    (x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0))
}

/// Entry point: rects for one cell, or none when the shaper owns it.
/// Box drawing snaps inside box_rects; areas (blocks, braille dots) snap
/// at the edges so adjacent cells rounding the same edges stay seamless.
pub fn cell_rects(c: char, w: f32, h: f32) -> Vec<Rect> {
    let u = c as u32;
    if (0x2500..=0x257F).contains(&u) {
        box_arms(c).map(|a| box_rects(a, w, h)).unwrap_or_default()
    } else if (0x2580..=0x2590).contains(&u) || (0x2594..=0x259F).contains(&u) {
        block_rects(c, w, h).into_iter().map(snap).collect()
    } else if (0x2800..=0x28FF).contains(&u) {
        braille_rects(c, w, h).into_iter().map(snap).collect()
    } else {
        vec![]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rects(c: char, w: f32, h: f32) -> Vec<Rect> {
        cell_rects(c, w, h)
    }

    #[test]
    fn light_horizontal_is_one_centered_bar() {
        let r = rects('\u{2500}', 8.8, 20.0);
        assert_eq!(r.len(), 1);
        let (x, y, rw, rh) = r[0];
        assert_eq!(x, 0.0);
        assert!((y + rh / 2.0 - 10.0).abs() < 0.6, "bar near center on {y}");
        assert_eq!(rh, 1.0);
        assert_eq!(rw, 9.0);
    }

    #[test]
    fn heavy_is_thicker_than_light() {
        let light = rects('\u{2500}', 8.8, 20.0)[0].3;
        let heavy = rects('\u{2501}', 8.8, 20.0)[0].3;
        assert!((heavy - light * 2.0).abs() < 0.01, "light {light} heavy {heavy}");
    }

    #[test]
    fn corner_covers_the_elbow() {
        // U+250C: a down stub and a right stub meeting at the center; both
        // must contain the center point so the elbow has no hole
        let r = rects('\u{250C}', 8.8, 20.0);
        assert_eq!(r.len(), 2);
        for &(x, y, rw, rh) in &r {
            let covers_cx = x <= 4.4 && x + rw >= 4.4;
            let covers_cy = y <= 10.0 && y + rh >= 10.0;
            assert!(covers_cx && covers_cy, "elbow not covered: {r:?}");
        }
    }

    #[test]
    fn double_vertical_is_two_bars() {
        let r = rects('\u{2551}', 8.8, 20.0);
        assert_eq!(r.len(), 2);
        let (x1, _, _, _) = r[0];
        let (x2, _, _, _) = r[1];
        assert_ne!(x1, x2);
        // one bar left of center, one right
        assert!(x1 < 4.4 && x2 > 4.4);
    }

    #[test]
    fn full_block_is_the_whole_cell() {
        assert_eq!(rects('\u{2588}', 8.8, 20.0), vec![(0.0, 0.0, 9.0, 20.0)]);
    }

    #[test]
    fn upper_half_block_hugs_the_top() {
        assert_eq!(rects('\u{2580}', 8.8, 20.0), vec![(0.0, 0.0, 9.0, 10.0)]);
    }

    #[test]
    fn bottom_eighth_hugs_the_floor() {
        assert_eq!(rects('\u{2581}', 8.8, 20.0), vec![(0.0, 18.0, 9.0, 2.0)]);
    }

    #[test]
    fn left_three_eighths_widens_from_the_left() {
        let r = rects('\u{258B}', 8.8, 20.0);
        assert_eq!(r, vec![(0.0, 0.0, 3.0, 20.0)]);
    }

    #[test]
    fn quadrants_combo() {
        // U+2599 has every quadrant but the upper right
        let r = rects('\u{2599}', 8.0, 20.0);
        assert_eq!(r.len(), 3);
        // U+259A is the diagonal pair
        let r = rects('\u{259A}', 8.0, 20.0);
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn braille_dot_layout() {
        // U+2801 = dot 1: upper left
        let r = rects('\u{2801}', 8.8, 20.0);
        assert_eq!(r.len(), 1);
        assert!(r[0].0 < 4.4, "dot 1 is the left column");
        // U+2840 = dot 7: bottom left
        let r = rects('\u{2840}', 8.8, 20.0);
        assert_eq!(r.len(), 1);
        assert!(r[0].1 > 10.0, "dot 7 is the bottom row");
        // U+28FF lights all eight dots
        assert_eq!(rects('\u{28FF}', 8.8, 20.0).len(), 8);
        // U+2800 is the blank pattern: nothing
        assert!(rects('\u{2800}', 8.8, 20.0).is_empty());
    }

    #[test]
    fn diagonals_and_rounded_stay_on_text() {
        assert!(!is_vector_glyph('\u{256D}'));
        assert!(!is_vector_glyph('\u{2571}'));
        assert!(!is_vector_glyph('\u{2591}'));
        assert!(!is_vector_glyph('\u{2592}'));
        assert!(!is_vector_glyph('\u{2593}'));
        assert!(is_vector_glyph('\u{2574}'));
        assert!(is_vector_glyph('\u{2588}'));
        assert!(is_vector_glyph('\u{28FF}'));
    }
}
