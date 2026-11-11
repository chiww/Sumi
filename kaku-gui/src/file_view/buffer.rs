//! The text model of the editor: lines, a cursor with an optional
//! selection anchor, and snapshot-based undo.

use std::time::{Duration, Instant};
use termwiz::cell::unicode_column_width;

pub(crate) const TAB_WIDTH: usize = 4;
const UNDO_LIMIT: usize = 500;
/// Typing within this pause joins the previous undo step.
const UNDO_GROUP: Duration = Duration::from_millis(800);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Pos {
    pub row: usize,
    /// Character index within the line.
    pub col: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditKind {
    Insert,
    Delete,
    Other,
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Pos,
}

pub(crate) struct Buffer {
    pub lines: Vec<String>,
    pub cursor: Pos,
    /// The other end of the selection, when one is active.
    pub anchor: Option<Pos>,
    /// Display column vertical moves aim for.
    want_x: Option<usize>,
    pub crlf: bool,
    pub trailing_newline: bool,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last_edit: Option<(EditKind, Instant)>,
    /// Bumped on every change; the dirty flag keys on it.
    pub version: u64,
    /// First row whose text may have changed since the highlighter last
    /// looked; `usize::MAX` when nothing changed.
    pub dirty_from: usize,
    saved_version: u64,
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn byte_at(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map_or(s.len(), |(i, _)| i)
}

/// Display width of the first `col` characters of `line`, with tabs
/// expanded to the next stop.
pub(crate) fn display_x(line: &str, col: usize) -> usize {
    let mut x = 0;
    for ch in line.chars().take(col) {
        x += char_width(ch, x);
    }
    x
}

pub(crate) fn char_width(ch: char, x: usize) -> usize {
    if ch == '\t' {
        TAB_WIDTH - x % TAB_WIDTH
    } else {
        unicode_column_width(ch.encode_utf8(&mut [0; 4]), None).max(1)
    }
}

/// The character index whose cell covers display column `target`.
pub(crate) fn col_for_x(line: &str, target: usize) -> usize {
    let mut x = 0;
    for (idx, ch) in line.chars().enumerate() {
        let w = char_width(ch, x);
        if x + w > target {
            // Land on the nearer edge of a wide cell.
            return if target - x >= w.div_ceil(2) {
                idx + 1
            } else {
                idx
            };
        }
        x += w;
    }
    char_count(line)
}

impl Buffer {
    pub fn from_text(text: &str) -> Self {
        let crlf = text.contains("\r\n");
        let trailing_newline = text.ends_with('\n');
        let body = text.strip_suffix('\n').unwrap_or(text);
        let body = if crlf {
            body.strip_suffix('\r').unwrap_or(body)
        } else {
            body
        };
        let lines: Vec<String> = if crlf {
            body.split("\r\n").map(str::to_string).collect()
        } else {
            body.split('\n').map(str::to_string).collect()
        };
        Self {
            lines: if lines.is_empty() {
                vec![String::new()]
            } else {
                lines
            },
            cursor: Pos::default(),
            anchor: None,
            want_x: None,
            crlf,
            trailing_newline: trailing_newline || text.is_empty(),
            undo: vec![],
            redo: vec![],
            last_edit: None,
            version: 0,
            saved_version: 0,
            dirty_from: 0,
        }
    }

    pub fn text(&self) -> String {
        let sep = if self.crlf { "\r\n" } else { "\n" };
        let mut out = self.lines.join(sep);
        if self.trailing_newline {
            out.push_str(sep);
        }
        out
    }

    pub fn is_dirty(&self) -> bool {
        self.version != self.saved_version
    }

    pub fn mark_saved(&mut self) {
        self.saved_version = self.version;
    }

    /// Replaces the content after an outside change, keeping the cursor
    /// where it still fits.
    pub fn reload(&mut self, text: &str) {
        let cursor = self.cursor;
        let undo = std::mem::take(&mut self.undo);
        let version = self.version;
        *self = Self::from_text(text);
        self.undo = undo;
        self.version = version + 1;
        self.saved_version = self.version;
        self.cursor = self.clamp(cursor);
    }

    fn clamp(&self, pos: Pos) -> Pos {
        let row = pos.row.min(self.lines.len() - 1);
        Pos {
            row,
            col: pos.col.min(char_count(&self.lines[row])),
        }
    }

    pub fn line_len(&self, row: usize) -> usize {
        char_count(&self.lines[row])
    }

    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(if anchor < self.cursor {
            (anchor, self.cursor)
        } else {
            (self.cursor, anchor)
        })
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        Some(self.text_between(start, end))
    }

    fn text_between(&self, start: Pos, end: Pos) -> String {
        if start.row == end.row {
            let line = &self.lines[start.row];
            return line[byte_at(line, start.col)..byte_at(line, end.col)].to_string();
        }
        let mut out = String::new();
        let first = &self.lines[start.row];
        out.push_str(&first[byte_at(first, start.col)..]);
        for row in start.row + 1..end.row {
            out.push('\n');
            out.push_str(&self.lines[row]);
        }
        out.push('\n');
        let last = &self.lines[end.row];
        out.push_str(&last[..byte_at(last, end.col)]);
        out
    }

    fn checkpoint(&mut self, kind: EditKind) {
        let now = Instant::now();
        let joins = matches!(
            self.last_edit,
            Some((last, at)) if last == kind && kind != EditKind::Other && now.duration_since(at) < UNDO_GROUP
        );
        if !joins {
            self.undo.push(Snapshot {
                lines: self.lines.clone(),
                cursor: self.cursor,
            });
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_edit = Some((kind, now));
        self.version += 1;
        let first = self
            .anchor
            .map_or(self.cursor.row, |a| a.row.min(self.cursor.row));
        self.dirty_from = self.dirty_from.min(first.saturating_sub(1));
    }

    fn delete_range(&mut self, start: Pos, end: Pos) {
        let tail = {
            let last = &self.lines[end.row];
            last[byte_at(last, end.col)..].to_string()
        };
        let first = &mut self.lines[start.row];
        first.truncate(byte_at(first, start.col));
        first.push_str(&tail);
        self.lines.drain(start.row + 1..=end.row);
        self.cursor = start;
        self.anchor = None;
    }

    /// Removes the selection; returns whether there was one.
    fn take_selection(&mut self) -> bool {
        match self.selection() {
            Some((start, end)) => {
                self.delete_range(start, end);
                true
            }
            None => {
                self.anchor = None;
                false
            }
        }
    }

    pub fn insert(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.checkpoint(if text.contains('\n') || text.chars().count() > 1 {
            EditKind::Other
        } else {
            EditKind::Insert
        });
        self.take_selection();
        let row = self.cursor.row;
        let line = &self.lines[row];
        let split = byte_at(line, self.cursor.col);
        let (head, tail) = (line[..split].to_string(), line[split..].to_string());
        let mut parts: Vec<&str> = text.split('\n').collect();
        let last = parts.pop().unwrap_or_default();
        if parts.is_empty() {
            self.lines[row] = format!("{head}{last}{tail}");
            self.cursor.col += char_count(last);
        } else {
            self.lines[row] = format!("{head}{}", parts[0]);
            let mut insert_at = row + 1;
            for part in &parts[1..] {
                self.lines.insert(insert_at, part.to_string());
                insert_at += 1;
            }
            self.lines.insert(insert_at, format!("{last}{tail}"));
            self.cursor = Pos {
                row: insert_at,
                col: char_count(last),
            };
        }
        self.want_x = None;
    }

    /// Enter: a new line that keeps the current line's indentation.
    pub fn newline(&mut self) {
        let line = &self.lines[self.cursor.row];
        let indent: String = line
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .take(self.cursor.col)
            .collect();
        self.insert(&format!("\n{indent}"));
    }

    pub fn backspace(&mut self) {
        self.checkpoint(EditKind::Delete);
        if self.take_selection() {
            return;
        }
        let Pos { row, col } = self.cursor;
        if col > 0 {
            self.delete_range(Pos { row, col: col - 1 }, self.cursor);
        } else if row > 0 {
            let col = self.line_len(row - 1);
            self.delete_range(Pos { row: row - 1, col }, self.cursor);
        }
        self.want_x = None;
    }

    pub fn delete(&mut self) {
        self.checkpoint(EditKind::Delete);
        if self.take_selection() {
            return;
        }
        let Pos { row, col } = self.cursor;
        if col < self.line_len(row) {
            self.delete_range(self.cursor, Pos { row, col: col + 1 });
        } else if row + 1 < self.lines.len() {
            self.delete_range(
                self.cursor,
                Pos {
                    row: row + 1,
                    col: 0,
                },
            );
        }
        self.want_x = None;
    }

    pub fn cut(&mut self) -> Option<String> {
        let text = self.selected_text()?;
        self.checkpoint(EditKind::Other);
        self.take_selection();
        Some(text)
    }

    pub fn undo(&mut self) -> bool {
        self.restore(true)
    }

    pub fn redo(&mut self) -> bool {
        self.restore(false)
    }

    fn restore(&mut self, undo: bool) -> bool {
        let (from, to) = if undo {
            (&mut self.undo, &mut self.redo)
        } else {
            (&mut self.redo, &mut self.undo)
        };
        let Some(snapshot) = from.pop() else {
            return false;
        };
        to.push(Snapshot {
            lines: std::mem::replace(&mut self.lines, snapshot.lines),
            cursor: self.cursor,
        });
        self.cursor = snapshot.cursor;
        self.anchor = None;
        self.last_edit = None;
        self.version += 1;
        self.dirty_from = 0;
        true
    }

    /// Moves the cursor, extending the selection when `select`.
    pub fn move_to(&mut self, pos: Pos, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = self.clamp(pos);
    }

    pub fn left(&mut self, select: bool) {
        if !select {
            if let Some((start, _)) = self.selection() {
                self.move_to(start, false);
                return;
            }
        }
        let Pos { row, col } = self.cursor;
        let pos = if col > 0 {
            Pos { row, col: col - 1 }
        } else if row > 0 {
            Pos {
                row: row - 1,
                col: self.line_len(row - 1),
            }
        } else {
            self.cursor
        };
        self.want_x = None;
        self.move_to(pos, select);
    }

    pub fn right(&mut self, select: bool) {
        if !select {
            if let Some((_, end)) = self.selection() {
                self.move_to(end, false);
                return;
            }
        }
        let Pos { row, col } = self.cursor;
        let pos = if col < self.line_len(row) {
            Pos { row, col: col + 1 }
        } else if row + 1 < self.lines.len() {
            Pos {
                row: row + 1,
                col: 0,
            }
        } else {
            self.cursor
        };
        self.want_x = None;
        self.move_to(pos, select);
    }

    /// Moves `delta` rows, keeping the display column.
    pub fn vertical(&mut self, delta: isize, select: bool) {
        let x = match self.want_x {
            Some(x) => x,
            None => display_x(&self.lines[self.cursor.row], self.cursor.col),
        };
        self.want_x = Some(x);
        let row =
            (self.cursor.row as isize + delta).clamp(0, self.lines.len() as isize - 1) as usize;
        let col = col_for_x(&self.lines[row], x);
        let want = self.want_x;
        self.move_to(Pos { row, col }, select);
        self.want_x = want;
    }

    /// Home: to the first non-blank, or to column 0 when already there.
    pub fn home(&mut self, select: bool) {
        let line = &self.lines[self.cursor.row];
        let indent = line.chars().take_while(|c| c.is_whitespace()).count();
        let col = if self.cursor.col == indent { 0 } else { indent };
        self.want_x = None;
        self.move_to(
            Pos {
                row: self.cursor.row,
                col,
            },
            select,
        );
    }

    pub fn end(&mut self, select: bool) {
        let col = self.line_len(self.cursor.row);
        self.want_x = None;
        self.move_to(
            Pos {
                row: self.cursor.row,
                col,
            },
            select,
        );
    }

    pub fn select_all(&mut self) {
        let last = self.lines.len() - 1;
        self.anchor = Some(Pos::default());
        self.cursor = Pos {
            row: last,
            col: self.line_len(last),
        };
    }

    /// The next match of `needle` after the cursor, wrapping around.
    pub fn find_next(&self, needle: &str, from: Pos) -> Option<(Pos, Pos)> {
        if needle.is_empty() {
            return None;
        }
        let lower = needle.to_lowercase();
        let n = self.lines.len();
        for step in 0..=n {
            let row = (from.row + step) % n;
            let line = self.lines[row].to_lowercase();
            let start_col = if step == 0 { from.col } else { 0 };
            let start_byte = byte_at(&line, start_col);
            if let Some(found) = line[start_byte..].find(&lower) {
                let byte = start_byte + found;
                let col = line[..byte].chars().count();
                return Some((
                    Pos { row, col },
                    Pos {
                        row,
                        col: col + char_count(&lower),
                    },
                ));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(row: usize, col: usize) -> Pos {
        Pos { row, col }
    }

    #[test]
    fn text_round_trips_line_endings_and_final_newline() {
        for text in ["a\nb\n", "a\nb", "a\r\nb\r\n", "", "\n"] {
            assert_eq!(
                Buffer::from_text(text).text(),
                if text.is_empty() { "\n" } else { text }
            );
        }
    }

    #[test]
    fn typing_newlines_and_deleting_edit_the_lines() {
        let mut b = Buffer::from_text("    fn main() {}\n");
        b.move_to(at(0, 15), false);
        b.newline();
        assert_eq!(b.lines, ["    fn main() {", "    }"]);
        assert_eq!(b.cursor, at(1, 4));
        b.insert("中文");
        assert_eq!(b.lines[1], "    中文}");
        b.backspace();
        b.backspace();
        assert_eq!(b.lines[1], "    }");
        b.move_to(at(1, 0), false);
        b.backspace();
        assert_eq!(b.lines, ["    fn main() {    }"]);
        assert!(b.is_dirty());
    }

    #[test]
    fn selection_replaces_and_cuts_across_lines() {
        let mut b = Buffer::from_text("one\ntwo\nthree");
        b.move_to(at(0, 1), false);
        b.move_to(at(2, 2), true);
        assert_eq!(b.selected_text().as_deref(), Some("ne\ntwo\nth"));
        b.insert("X");
        assert_eq!(b.lines, ["oXree"]);
        b.select_all();
        assert_eq!(b.cut().as_deref(), Some("oXree"));
        assert_eq!(b.lines, [""]);
    }

    #[test]
    fn undo_groups_typing_and_redo_restores() {
        let mut b = Buffer::from_text("");
        for c in ["a", "b", "c"] {
            b.insert(c);
        }
        b.newline();
        assert_eq!(b.text(), "abc\n\n");
        assert!(b.undo());
        assert_eq!(b.text(), "abc\n");
        assert!(b.undo());
        assert_eq!(b.text(), "\n");
        assert!(!b.undo());
        assert!(b.redo());
        assert_eq!(b.text(), "abc\n");
    }

    #[test]
    fn vertical_moves_keep_the_display_column_across_tabs_and_wide_text() {
        let mut b = Buffer::from_text("\tabc\n中文字\nx");
        b.move_to(at(0, 2), false); // after "\ta": x = 5
        b.vertical(1, false);
        // x = 5 falls inside 字 (cells 4..6); its far edge is as near.
        assert_eq!(b.cursor, at(1, 3));
        b.vertical(1, false);
        assert_eq!(b.cursor, at(2, 1));
        b.vertical(-2, false);
        assert_eq!(b.cursor.row, 0);
        assert_eq!(display_x("\tab", 2), 5);
        assert_eq!(col_for_x("中文", 1), 1);
        assert_eq!(col_for_x("中文", 0), 0);
    }

    #[test]
    fn find_wraps_and_ignores_case() {
        let b = Buffer::from_text("Alpha\nbeta\nALPHA");
        assert_eq!(b.find_next("alpha", at(0, 1)), Some((at(2, 0), at(2, 5))));
        assert_eq!(b.find_next("alpha", at(2, 1)), Some((at(0, 0), at(0, 5))));
        assert_eq!(b.find_next("zzz", at(0, 0)), None);
    }

    #[test]
    fn reload_keeps_a_fitting_cursor_and_is_clean() {
        let mut b = Buffer::from_text("long line\nsecond\n");
        b.move_to(at(1, 4), false);
        b.insert("!");
        b.reload("short\n");
        assert_eq!(b.cursor, at(0, 5));
        assert!(!b.is_dirty());
    }
}
