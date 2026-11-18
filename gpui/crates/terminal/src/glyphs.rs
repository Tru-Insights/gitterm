// Adapted from gpui_xterm 0.3.0; see README.md and licenses/gpui_xterm-MIT.
//! @file glyphs.rs
//! @brief Font-independent terminal block and box glyphs on the physical pixel grid.
//! @author modolet <y@xxyx.io>
//! @date 2026-09-12

use gpui_kit::{Bounds, Pixels, point, px, size};

pub(crate) struct GlyphRect {
    pub bounds: Bounds<Pixels>,
    pub opacity: f32,
}

/// Shared edges are rounded from absolute coordinates, never from cell widths.
/// This keeps adjacent cells connected even at fractional display scales.
pub(crate) fn rectangles(ch: char, bounds: Bounds<Pixels>, scale: f32) -> Option<Vec<GlyphRect>> {
    let x = f32::from(bounds.origin.x) * scale;
    let y = f32::from(bounds.origin.y) * scale;
    let w = f32::from(bounds.size.width) * scale;
    let h = f32::from(bounds.size.height) * scale;
    let mut rects = Vec::new();
    let mut rect = |left: f32, top: f32, right: f32, bottom: f32, opacity: f32| {
        let left = left.round().max(x.round());
        let top = top.round().max(y.round());
        let right = right.round().min((x + w).round());
        let bottom = bottom.round().min((y + h).round());
        if right > left && bottom > top {
            rects.push(GlyphRect {
                bounds: Bounds::new(
                    point(px(left / scale), px(top / scale)),
                    size(px((right - left) / scale), px((bottom - top) / scale)),
                ),
                opacity,
            });
        }
    };

    match ch {
        '▀' => rect(x, y, x + w, y + h / 2., 1.),
        '▁'..='█' => {
            let fraction = (ch as u32 - '▁' as u32 + 1) as f32 / 8.;
            rect(x, y + h * (1. - fraction), x + w, y + h, 1.);
        }
        '▉'..='▏' => {
            let fraction = ('▏' as u32 - ch as u32 + 1) as f32 / 8.;
            rect(x, y, x + w * fraction, y + h, 1.);
        }
        '▐' => rect(x + w / 2., y, x + w, y + h, 1.),
        '░' | '▒' | '▓' => {
            let opacity = (ch as u32 - '░' as u32 + 1) as f32 / 4.;
            rect(x, y, x + w, y + h, opacity);
        }
        '▔' => rect(x, y, x + w, y + h / 8., 1.),
        '▕' => rect(x + w * 7. / 8., y, x + w, y + h, 1.),
        '▖'..='▟' => {
            // Bits: top-left, top-right, bottom-left, bottom-right.
            let mask = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14][ch as usize - '▖' as usize];
            for (bit, col, row) in [(1, 0, 0), (2, 1, 0), (4, 0, 1), (8, 1, 1)] {
                if mask & bit != 0 {
                    rect(
                        x + w * col as f32 / 2.,
                        y + h * row as f32 / 2.,
                        x + w * (col + 1) as f32 / 2.,
                        y + h * (row + 1) as f32 / 2.,
                        1.,
                    );
                }
            }
        }
        _ => {
            // Left/right/up/down arms; unsupported glyphs retain font rendering.
            let (arms, double, heavy) = match ch {
                '─' | '━' | '═' => (3, ch == '═', ch == '━'),
                '│' | '┃' | '║' => (12, ch == '║', ch == '┃'),
                '┌' | '┏' | '╔' => (10, ch == '╔', ch == '┏'),
                '┐' | '┓' | '╗' => (9, ch == '╗', ch == '┓'),
                '└' | '┗' | '╚' => (6, ch == '╚', ch == '┗'),
                '┘' | '┛' | '╝' => (5, ch == '╝', ch == '┛'),
                '├' | '┣' | '╠' => (14, ch == '╠', ch == '┣'),
                '┤' | '┫' | '╣' => (13, ch == '╣', ch == '┫'),
                '┬' | '┳' | '╦' => (11, ch == '╦', ch == '┳'),
                '┴' | '┻' | '╩' => (7, ch == '╩', ch == '┻'),
                '┼' | '╋' | '╬' => (15, ch == '╬', ch == '╋'),
                _ => return None,
            };
            let left = arms & 1 != 0;
            let right = arms & 2 != 0;
            let up = arms & 4 != 0;
            let down = arms & 8 != 0;
            let thickness = (w / 10.).round().max(1.) * if heavy { 2. } else { 1. };
            // The two rails must have an identical phase at every cell boundary.
            let cx = (x + w / 2. - thickness / 2.).round();
            let cy = (y + h / 2. - thickness / 2.).round();
            if double {
                // Each arm terminates at a corner of the central square. Join
                // its edges only where there is no outgoing arm. This keeps
                // corners, tees and crosses open between their double rails.
                let lo_x = cx - thickness;
                let hi_x = cx + thickness;
                let lo_y = cy - thickness;
                let hi_y = cy + thickness;
                for rail_y in [lo_y, hi_y] {
                    if left {
                        rect(x, rail_y, lo_x + thickness, rail_y + thickness, 1.);
                    }
                    if right {
                        rect(hi_x, rail_y, x + w, rail_y + thickness, 1.);
                    }
                }
                for rail_x in [lo_x, hi_x] {
                    if up {
                        rect(rail_x, y, rail_x + thickness, lo_y + thickness, 1.);
                    }
                    if down {
                        rect(rail_x, hi_y, rail_x + thickness, y + h, 1.);
                    }
                }
                if !left {
                    rect(lo_x, lo_y, lo_x + thickness, hi_y + thickness, 1.);
                }
                if !right {
                    rect(hi_x, lo_y, hi_x + thickness, hi_y + thickness, 1.);
                }
                if !up {
                    rect(lo_x, lo_y, hi_x + thickness, lo_y + thickness, 1.);
                }
                if !down {
                    rect(lo_x, hi_y, hi_x + thickness, hi_y + thickness, 1.);
                }
            } else {
                if left {
                    rect(x, cy, cx + thickness, cy + thickness, 1.);
                }
                if right {
                    rect(cx, cy, x + w, cy + thickness, 1.);
                }
                if up {
                    rect(cx, y, cx + thickness, cy + thickness, 1.);
                }
                if down {
                    rect(cx, cy, cx + thickness, y + h, 1.);
                }
            }
        }
    }
    Some(rects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(col: usize, row: usize) -> Bounds<Pixels> {
        Bounds::new(
            point(px(0.3 + col as f32 * 9.6), px(0.7 + row as f32 * 23.4)),
            size(px(9.6), px(23.4)),
        )
    }

    #[test]
    fn full_blocks_share_edges_at_fractional_scales() {
        for scale in [1., 1.25, 1.5, 2.] {
            for col in 0..80 {
                let a = rectangles('█', cell(col, 0), scale).unwrap();
                let b = rectangles('█', cell(col + 1, 0), scale).unwrap();
                let c = rectangles('█', cell(col, 1), scale).unwrap();
                assert!((f32::from(a[0].bounds.right() - b[0].bounds.left())).abs() < 0.0001);
                assert!((f32::from(a[0].bounds.bottom() - c[0].bounds.top())).abs() < 0.0001);
            }
        }
    }

    #[test]
    fn upper_and_lower_blocks_fill_the_cell_without_a_gap() {
        for scale in [1., 1.25, 1.5, 2.] {
            let top = rectangles('▀', cell(0, 0), scale).unwrap();
            let bottom = rectangles('▄', cell(0, 0), scale).unwrap();
            assert!((f32::from(top[0].bounds.bottom() - bottom[0].bounds.top())).abs() < 0.0001);
        }
    }

    #[test]
    fn double_corner_rails_join_the_horizontal_and_vertical_edges() {
        let bounds = Bounds::new(point(px(0.), px(0.)), size(px(12.), px(24.)));
        let occupied = |ch, x, y| {
            rectangles(ch, bounds, 1.)
                .unwrap()
                .iter()
                .any(|r| r.bounds.contains(&point(px(x), px(y))))
        };
        for y in 0..24 {
            assert_eq!(
                occupied('╔', 11.5, y as f32 + 0.5),
                occupied('═', 0.5, y as f32 + 0.5)
            );
        }
        for x in 0..12 {
            assert_eq!(
                occupied('╔', x as f32 + 0.5, 23.5),
                occupied('║', x as f32 + 0.5, 0.5)
            );
        }
        assert!(!occupied('╔', 6.5, 12.5));
    }

    #[test]
    fn double_box_junctions_keep_their_rails_connected_and_separate() {
        use std::collections::HashSet;
        for (ch, expected) in "╔╗╚╝═║╠╣╦╩╬".chars().zip([2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 4])
        {
            let bounds = Bounds::new(point(px(0.), px(0.)), size(px(12.), px(24.)));
            let rects = rectangles(ch, bounds, 1.).unwrap();
            let mut pixels: HashSet<(i32, i32)> = (0..12)
                .flat_map(|x| (0..24).map(move |y| (x, y)))
                .filter(|&(x, y)| {
                    rects.iter().any(|r| {
                        r.bounds
                            .contains(&point(px(x as f32 + 0.5), px(y as f32 + 0.5)))
                    })
                })
                .collect();
            let mut components = 0;
            while let Some(&seed) = pixels.iter().next() {
                components += 1;
                pixels.remove(&seed);
                let mut queue = vec![seed];
                while let Some((x, y)) = queue.pop() {
                    for next in [(x - 1, y), (x + 1, y), (x, y - 1), (x, y + 1)] {
                        if pixels.remove(&next) {
                            queue.push(next);
                        }
                    }
                }
            }
            assert_eq!(
                components, expected,
                "disconnected or crossed rails for {ch}"
            );
        }
    }

    #[test]
    fn banner_glyphs_are_geometric_but_text_keeps_font_rendering() {
        for ch in "█╗╔═╚╝║".chars() {
            assert!(!rectangles(ch, cell(0, 0), 1.25).unwrap().is_empty());
        }
        for ch in "SIOIS?0a中".chars() {
            assert!(rectangles(ch, cell(0, 0), 1.).is_none());
        }
    }
}
