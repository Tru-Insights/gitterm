// Adapted from gpui_xterm 0.3.0; see README.md and licenses/gpui_xterm-MIT.
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use gpui_kit::{Pixels, Point};

pub fn pixel_to_cell(
    position: Point<Pixels>,
    origin: Point<Pixels>,
    cell_width: Pixels,
    cell_height: Pixels,
) -> AlacPoint {
    let col = ((position.x - origin.x) / cell_width).floor();
    let col = col.max(0.0) as usize;

    let row = ((position.y - origin.y) / cell_height).floor();
    let row = row.max(0.0) as i32;

    AlacPoint::new(Line(row), Column(col))
}
