use crate::kernel::kernel_message::SharedVt;
use crate::term::grid::{Pos, Rect};
use crate::term::Grid;

pub fn term_view(grid: &mut Grid, area: Rect, vt: &SharedVt) {
  let parser = vt.read().unwrap();
  let screen = parser.screen();

  for row in 0..area.height {
    for col in 0..area.width {
      let to_cell =
        if let Some(cell) = grid.drawing_cell_mut(Pos {
          col: area.x + col,
          row: area.y + row,
        }) {
          cell
        } else {
          continue;
        };
      if let Some(cell) = screen.cell(row, col) {
        *to_cell = cell.clone();
        if !cell.has_contents() {
          to_cell.set_str(" ");
        }
      }
    }
  }

  if !screen.hide_cursor() {
    let (cursor_row, cursor_col) = screen.cursor_position();
    grid.cursor_pos = Some(Pos {
      col: area.x + cursor_col,
      row: area.y + cursor_row,
    });
    grid.cursor_style = screen.cursor_style();
  }
}
