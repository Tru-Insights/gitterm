// Adapted from gpui_xterm 0.3.0; see README.md and licenses/gpui_xterm-MIT.
use crate::backend::RenderableContent;
use crate::colors::ColorPalette;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as AlacPoint};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::Color;
use gpui_kit::{
    App, Bounds, Edges, Font, FontFeatures, FontStyle, FontWeight, Hsla, Pixels, Point,
    SharedString, Size, TextAlign, TextRun, UnderlineStyle, Window, px, quad, transparent_black,
};
use std::ops::Range;

#[derive(Debug, Clone)]
pub struct BatchedTextRun {
    pub text: String,

    pub start_col: usize,

    pub row: usize,

    pub fg_color: Hsla,

    pub bg_color: Hsla,

    pub bold: bool,

    pub italic: bool,

    pub underline: bool,
}

#[derive(Debug, Clone)]
pub struct BackgroundRect {
    pub start_col: usize,

    pub end_col: usize,

    pub row: usize,

    pub color: Hsla,
}

impl BackgroundRect {
    fn can_merge_with(&self, other: &Self) -> bool {
        self.row == other.row && self.color == other.color && self.end_col == other.start_col
    }
}

#[derive(Clone)]
pub struct TerminalRenderer {
    pub font_family: String,

    pub font_size: Pixels,

    pub cell_width: Pixels,

    pub cell_height: Pixels,

    pub line_height_multiplier: f32,

    pub palette: ColorPalette,
}

impl TerminalRenderer {
    pub fn new(
        font_family: String,
        font_size: Pixels,
        line_height_multiplier: f32,
        palette: ColorPalette,
    ) -> Self {
        let cell_width = font_size * 0.6;
        let cell_height = font_size * 1.4;

        Self {
            font_family,
            font_size,
            cell_width,
            cell_height,
            line_height_multiplier,
            palette,
        }
    }

    pub fn measure_cell(&mut self, window: &mut Window) {
        // The grid follows the primary monospace font. Fallback block glyphs must
        // never enlarge it, and a font/size change must also be able to shrink it.
        let text_run = TextRun {
            len: 1,
            font: Font {
                family: self.font_family.clone().into(),
                features: FontFeatures::default(),
                fallbacks: None,
                weight: FontWeight::NORMAL,
                style: FontStyle::Normal,
            },
            color: gpui_kit::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window
            .text_system()
            .shape_line("M".into(), self.font_size, &[text_run], None);
        self.set_cell_metrics(shaped.width, shaped.ascent + shaped.descent);
    }

    fn set_cell_metrics(&mut self, advance: Pixels, line_height: Pixels) {
        self.cell_width = advance.max(px(1.0));
        self.cell_height = (line_height * self.line_height_multiplier).max(px(1.0));
    }

    pub fn layout_row(
        &self,
        row: usize,
        cells: impl Iterator<Item = (usize, Cell)>,
        colors: &Colors,
    ) -> (Vec<BackgroundRect>, Vec<BatchedTextRun>) {
        let mut backgrounds = Vec::new();
        let mut text_runs = Vec::new();

        let mut current_run: Option<BatchedTextRun> = None;
        let mut current_bg: Option<BackgroundRect> = None;

        for (col, cell) in cells {
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }

            let fg_color = self.palette.resolve(cell.fg, colors);
            let bg_color = self.palette.resolve(cell.bg, colors);
            let bold = cell.flags.contains(Flags::BOLD);
            let italic = cell.flags.contains(Flags::ITALIC);
            let underline = cell.flags.contains(Flags::UNDERLINE);

            let ch = if cell.c == ' ' || cell.c == '\0' {
                ' '
            } else {
                cell.c
            };

            if let Some(ref mut bg_rect) = current_bg {
                if bg_rect.color == bg_color && bg_rect.end_col == col {
                    bg_rect.end_col = col + 1;
                } else {
                    backgrounds.push(bg_rect.clone());
                    current_bg = Some(BackgroundRect {
                        start_col: col,
                        end_col: col + 1,
                        row,
                        color: bg_color,
                    });
                }
            } else {
                current_bg = Some(BackgroundRect {
                    start_col: col,
                    end_col: col + 1,
                    row,
                    color: bg_color,
                });
            }

            if let Some(ref mut run) = current_run {
                if run.fg_color == fg_color
                    && run.bg_color == bg_color
                    && run.bold == bold
                    && run.italic == italic
                    && run.underline == underline
                {
                    run.text.push(ch);
                } else {
                    text_runs.push(run.clone());
                    current_run = Some(BatchedTextRun {
                        text: ch.to_string(),
                        start_col: col,
                        row,
                        fg_color,
                        bg_color,
                        bold,
                        italic,
                        underline,
                    });
                }
            } else {
                current_run = Some(BatchedTextRun {
                    text: ch.to_string(),
                    start_col: col,
                    row,
                    fg_color,
                    bg_color,
                    bold,
                    italic,
                    underline,
                });
            }
        }

        if let Some(run) = current_run {
            text_runs.push(run);
        }
        if let Some(bg) = current_bg {
            backgrounds.push(bg);
        }

        let merged_backgrounds = self.merge_backgrounds(backgrounds);

        (merged_backgrounds, text_runs)
    }

    fn merge_backgrounds(&self, mut rects: Vec<BackgroundRect>) -> Vec<BackgroundRect> {
        if rects.is_empty() {
            return rects;
        }

        let mut merged = Vec::new();
        let mut current = rects.remove(0);

        for rect in rects {
            if current.can_merge_with(&rect) {
                current.end_col = rect.end_col;
            } else {
                merged.push(current);
                current = rect;
            }
        }

        merged.push(current);
        merged
    }

    pub fn paint(
        &self,
        bounds: Bounds<Pixels>,
        padding: Edges<Pixels>,
        snapshot: &RenderableContent,
        window: &mut Window,
        cx: &mut App,
        ime_preedit: Option<(&str, Option<&Range<usize>>)>,
    ) {
        let num_lines = snapshot.terminal_size.screen_lines();
        let num_cols = snapshot.terminal_size.columns();
        let colors = &snapshot.colors;

        let default_bg = self.palette.resolve(
            Color::Named(alacritty_terminal::vte::ansi::NamedColor::Background),
            colors,
        );

        window.paint_quad(quad(
            bounds,
            px(0.0),
            default_bg,
            Edges::<Pixels>::default(),
            transparent_black(),
            Default::default(),
        ));

        let origin = Point {
            x: bounds.origin.x + padding.left,
            y: bounds.origin.y + padding.top,
        };

        let display_offset = snapshot.display_offset as i32;
        let selection_range = snapshot.selectable_range;

        for line_idx in 0..num_lines {
            let line = Line(line_idx as i32 - display_offset);

            let cells: Vec<(usize, Cell)> = (0..num_cols)
                .map(|col_idx| {
                    let cell = snapshot.cells[line_idx * num_cols + col_idx].cell.clone();
                    (col_idx, cell)
                })
                .collect();

            let (backgrounds, _) = self.layout_row(line_idx, cells.iter().cloned(), colors);

            for bg_rect in backgrounds {
                if bg_rect.color == default_bg {
                    continue;
                }

                let x = origin.x + self.cell_width * (bg_rect.start_col as f32);
                let y = origin.y + self.cell_height * (bg_rect.row as f32);
                let width = self.cell_width * ((bg_rect.end_col - bg_rect.start_col) as f32);
                let height = self.cell_height;

                let rect_bounds = Bounds {
                    origin: Point { x, y },
                    size: Size { width, height },
                };

                window.paint_quad(quad(
                    rect_bounds,
                    px(0.0),
                    bg_rect.color,
                    Edges::<Pixels>::default(),
                    transparent_black(),
                    Default::default(),
                ));
            }

            for (start_col, end_col) in selection_segments_for_row(&selection_range, line, num_cols)
            {
                let x = origin.x + self.cell_width * (start_col as f32);
                let y = origin.y + self.cell_height * (line_idx as f32);
                let width = self.cell_width * ((end_col - start_col) as f32);
                let rect_bounds = Bounds {
                    origin: Point { x, y },
                    size: Size {
                        width,
                        height: self.cell_height,
                    },
                };
                window.paint_quad(quad(
                    rect_bounds,
                    px(0.0),
                    selection_highlight(self.palette.cursor()),
                    Edges::<Pixels>::default(),
                    transparent_black(),
                    Default::default(),
                ));
            }

            for (col_idx, cell) in cells {
                let ch = cell.c;

                if ch == ' ' || ch == '\0' || cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }

                let x = origin.x + self.cell_width * (col_idx as f32);
                let y = origin.y + self.cell_height * (line_idx as f32);

                let point = AlacPoint::new(line, Column(col_idx));
                let selected = selection_contains(&selection_range, point);
                let mut fg_color = self.palette.resolve(cell.fg, colors);
                if selected {
                    fg_color = self.palette.foreground();
                }

                let cell_bounds = Bounds {
                    origin: Point { x, y },
                    size: Size {
                        width: self.cell_width,
                        height: self.cell_height,
                    },
                };
                if let Some(rects) =
                    crate::glyphs::rectangles(ch, cell_bounds, window.scale_factor())
                {
                    for rect in rects {
                        let mut color = fg_color;
                        color.a *= rect.opacity;
                        window.paint_quad(quad(
                            rect.bounds,
                            px(0.0),
                            color,
                            Edges::<Pixels>::default(),
                            transparent_black(),
                            Default::default(),
                        ));
                    }
                    continue;
                }

                let flags = cell.flags;
                let bold = flags.contains(alacritty_terminal::term::cell::Flags::BOLD);
                let italic = flags.contains(alacritty_terminal::term::cell::Flags::ITALIC);
                let underline = flags.contains(alacritty_terminal::term::cell::Flags::UNDERLINE);

                let font = Font {
                    family: self.font_family.clone().into(),
                    features: FontFeatures::default(),
                    fallbacks: None,
                    weight: if bold {
                        FontWeight::BOLD
                    } else {
                        FontWeight::NORMAL
                    },
                    style: if italic {
                        FontStyle::Italic
                    } else {
                        FontStyle::Normal
                    },
                };

                let mut char_str = ch.to_string();
                if let Some(extra) = cell.zerowidth() {
                    char_str.extend(extra);
                }
                let text_run = TextRun {
                    len: char_str.len(),
                    font,
                    color: fg_color,
                    background_color: None,
                    underline: if underline {
                        Some(UnderlineStyle {
                            thickness: px(1.0),
                            color: Some(fg_color),
                            wavy: false,
                        })
                    } else {
                        None
                    },
                    strikethrough: None,
                };

                let text: SharedString = char_str.into();
                let shaped_line =
                    window
                        .text_system()
                        .shape_line(text, self.font_size, &[text_run], None);

                let _ = shaped_line.paint(
                    Point { x, y },
                    self.cell_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
        }

        if let Some((ime_text, marked_range)) = ime_preedit.filter(|(text, _)| !text.is_empty()) {
            self.paint_ime_preedit(
                origin,
                AlacPoint::new(
                    alacritty_terminal::index::Line(snapshot.cursor_point.line.0 + display_offset),
                    snapshot.cursor_point.column,
                ),
                ime_text,
                marked_range,
                window,
                cx,
            );
        }

        let cursor_point = snapshot.cursor_point;
        let visible_cursor_row = cursor_point.line.0 + display_offset;
        if ime_preedit.is_none()
            && snapshot
                .terminal_mode
                .contains(alacritty_terminal::term::TermMode::SHOW_CURSOR)
            && (0..num_lines as i32).contains(&visible_cursor_row)
        {
            let cursor_x = origin.x + self.cell_width * (cursor_point.column.0 as f32);
            let cursor_y = origin.y + self.cell_height * (visible_cursor_row as f32);

            let cursor_color = self.palette.resolve(
                Color::Named(alacritty_terminal::vte::ansi::NamedColor::Cursor),
                colors,
            );

            let cursor_bounds = Bounds {
                origin: Point {
                    x: cursor_x,
                    y: cursor_y,
                },
                size: Size {
                    width: self.cell_width,
                    height: self.cell_height,
                },
            };

            window.paint_quad(quad(
                cursor_bounds,
                px(0.0),
                cursor_color,
                Edges::<Pixels>::default(),
                transparent_black(),
                Default::default(),
            ));
        }
    }

    fn paint_ime_preedit(
        &self,
        origin: Point<Pixels>,
        cursor_point: AlacPoint,
        ime_text: &str,
        marked_range: Option<&Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let visible_row = cursor_point.line.0;
        if visible_row < 0 {
            return;
        }

        let cursor_x = origin.x + self.cell_width * (cursor_point.column.0 as f32);
        let cursor_y = origin.y + self.cell_height * (visible_row as f32);
        let highlight = selection_highlight(self.palette.cursor());

        for (index, ch) in ime_text.chars().enumerate() {
            let x = cursor_x + self.cell_width * (index as f32);
            let rect_bounds = Bounds {
                origin: Point { x, y: cursor_y },
                size: Size {
                    width: self.cell_width,
                    height: self.cell_height,
                },
            };

            window.paint_quad(quad(
                rect_bounds,
                px(0.0),
                highlight,
                Edges::<Pixels>::default(),
                transparent_black(),
                Default::default(),
            ));

            let underline = marked_range
                .and_then(|range| utf16_range_to_char_range(ime_text, range))
                .is_some_and(|range| range.contains(&index));

            let font = Font {
                family: self.font_family.clone().into(),
                features: FontFeatures::default(),
                fallbacks: None,
                weight: FontWeight::NORMAL,
                style: FontStyle::Normal,
            };

            let text_run = TextRun {
                len: ch.len_utf8(),
                font,
                color: self.palette.foreground(),
                background_color: None,
                underline: underline.then_some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(self.palette.foreground()),
                    wavy: false,
                }),
                strikethrough: None,
            };

            let shaped_line = window.text_system().shape_line(
                ch.to_string().into(),
                self.font_size,
                &[text_run],
                None,
            );
            let _ = shaped_line.paint(
                Point { x, y: cursor_y },
                self.cell_height,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }
}

fn selection_contains(selection: &Option<SelectionRange>, point: AlacPoint) -> bool {
    selection
        .as_ref()
        .is_some_and(|selection| selection.contains(point))
}

fn selection_segments_for_row(
    selection: &Option<SelectionRange>,
    row: Line,
    num_cols: usize,
) -> Vec<(usize, usize)> {
    let Some(selection) = selection else {
        return Vec::new();
    };
    if row < selection.start.line || row > selection.end.line {
        return Vec::new();
    }

    let last_col = num_cols.saturating_sub(1);
    let start = if selection.is_block || row == selection.start.line {
        selection.start.column.0.min(last_col)
    } else {
        0
    };
    let end_inclusive = if selection.is_block || row == selection.end.line {
        selection.end.column.0.min(last_col)
    } else {
        last_col
    };

    if start > end_inclusive {
        return Vec::new();
    }

    vec![(start, end_inclusive + 1)]
}

fn selection_highlight(cursor: Hsla) -> Hsla {
    Hsla { a: 0.35, ..cursor }
}

fn utf16_range_to_char_range(text: &str, range: &Range<usize>) -> Option<Range<usize>> {
    let mut utf16_offset = 0;
    let mut start = None;
    let mut end = None;

    for (char_idx, ch) in text.chars().enumerate() {
        if utf16_offset == range.start {
            start = Some(char_idx);
        }
        if utf16_offset == range.end {
            end = Some(char_idx);
            break;
        }
        utf16_offset += ch.len_utf16();
    }

    let char_count = text.chars().count();
    if utf16_offset == range.start {
        start = Some(char_count);
    }
    if utf16_offset == range.end {
        end = Some(char_count);
    }

    match (start, end) {
        (Some(start), Some(end)) if start <= end => Some(start..end),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_metrics_can_shrink_after_font_and_line_height_changes() {
        let mut renderer =
            TerminalRenderer::new("monospace".into(), px(20.), 1.2, ColorPalette::default());
        renderer.set_cell_metrics(px(12.), px(24.));
        renderer.line_height_multiplier = 1.;
        renderer.set_cell_metrics(px(8.), px(16.));
        assert_eq!(renderer.cell_width, px(8.));
        assert_eq!(renderer.cell_height, px(16.));
    }

    #[test]
    fn test_renderer_creation() {
        let renderer = TerminalRenderer::new(
            "Fira Code".to_string(),
            px(14.0),
            1.2,
            ColorPalette::default(),
        );
        assert_eq!(renderer.font_family, "Fira Code");
        assert_eq!(renderer.font_size, px(14.0));
        assert_eq!(renderer.line_height_multiplier, 1.2);
    }

    #[test]
    fn test_background_rect_merge() {
        let black = Hsla::black();

        let rect1 = BackgroundRect {
            start_col: 0,
            end_col: 5,
            row: 0,
            color: black,
        };

        let rect2 = BackgroundRect {
            start_col: 5,
            end_col: 10,
            row: 0,
            color: black,
        };

        assert!(rect1.can_merge_with(&rect2));

        let rect3 = BackgroundRect {
            start_col: 5,
            end_col: 10,
            row: 1,
            color: black,
        };

        assert!(!rect1.can_merge_with(&rect3));
    }

    #[test]
    fn selection_segments_cover_spaces_contiguously() {
        let selection = SelectionRange::new(
            AlacPoint::new(Line(1), Column(2)),
            AlacPoint::new(Line(1), Column(6)),
            false,
        );
        assert_eq!(
            selection_segments_for_row(&Some(selection), Line(1), 10),
            vec![(2, 7)]
        );
    }

    #[test]
    fn test_merge_backgrounds() {
        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.2,
            ColorPalette::default(),
        );
        let black = Hsla::black();

        let rects = vec![
            BackgroundRect {
                start_col: 0,
                end_col: 5,
                row: 0,
                color: black,
            },
            BackgroundRect {
                start_col: 5,
                end_col: 10,
                row: 0,
                color: black,
            },
        ];

        let merged = renderer.merge_backgrounds(rects);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].start_col, 0);
        assert_eq!(merged[0].end_col, 10);
    }

    #[test]
    fn layout_row_keeps_same_style_text_in_one_run() {
        use alacritty_terminal::term::cell::Cell;
        use alacritty_terminal::vte::ansi::NamedColor;

        let renderer = TerminalRenderer::new(
            "monospace".to_string(),
            px(14.0),
            1.0,
            ColorPalette::default(),
        );

        let first = Cell {
            c: 'S',
            fg: Color::Named(NamedColor::Foreground),
            ..Cell::default()
        };

        let second = Cell {
            c: 'I',
            fg: Color::Named(NamedColor::Foreground),
            ..Cell::default()
        };

        let third = Cell {
            c: 'O',
            fg: Color::Named(NamedColor::Foreground),
            ..Cell::default()
        };

        let fourth = Cell {
            c: 'S',
            fg: Color::Named(NamedColor::Foreground),
            ..Cell::default()
        };

        let (_backgrounds, runs) = renderer.layout_row(
            0,
            vec![(0, first), (1, second), (2, third), (3, fourth)].into_iter(),
            &Colors::default(),
        );

        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].start_col, 0);
        assert_eq!(runs[0].text, "SIOS");
    }
}
