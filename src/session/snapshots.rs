//! Terminal output audit, activity tracking, screen baselines and grid rendering.
use super::*;

// Keep the evidence for an append beside the parser, under the same lock. A
// string suffix overlap cannot distinguish new repeated lines from old output.
#[derive(Default)]
pub(super) struct AppendAudit {
    pub(super) unsafe_output: bool,
    pub(super) escape: u8,
    pub(super) carriage_return: bool,
    parameters: Vec<u8>,
}

impl AppendAudit {
    pub(super) fn process(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.carriage_return {
                self.carriage_return = false;
                // ConPTY can redraw the current prompt after CR. The cell
                // comparison below proves that its existing prefix survived.
            }
            match self.escape {
                1 => {
                    self.parameters.clear();
                    match byte {
                        b'[' => self.escape = 2,
                        b']' => self.escape = 3,
                        _ => {
                            self.escape = 0;
                            self.unsafe_output = true;
                        }
                    }
                }
                2 => {
                    if (0x40..=0x7e).contains(&byte) {
                        // Colors, cursor visibility and bracketed paste do not
                        // change cells or cursor position. Bash toggles paste
                        // mode around every command, including through ConPTY.
                        let cosmetic = byte == b'm'
                            && self
                                .parameters
                                .iter()
                                .all(|b| b.is_ascii_digit() || matches!(b, b';' | b':'));
                        let input_or_cursor_mode = matches!(byte, b'h' | b'l')
                            && matches!(self.parameters.as_slice(), b"25" | b"?25" | b"?2004");
                        // ConPTY clears blank padding with ECH/EL and can move
                        // right over spaces with CUF. The cell comparison
                        // rejects changed prefixes and text beyond the cursor.
                        let padding = (matches!(byte, b'X' | b'C')
                            && self.parameters.iter().all(u8::is_ascii_digit))
                            || (byte == b'K' && matches!(self.parameters.as_slice(), b"" | b"0"));
                        // ConPTY also pads a repainted prompt with spaces,
                        // then positions the cursor back at its actual end.
                        let position = matches!(byte, b'H' | b'f')
                            && self
                                .parameters
                                .iter()
                                .all(|b| b.is_ascii_digit() || *b == b';');
                        self.unsafe_output |=
                            !cosmetic && !input_or_cursor_mode && !padding && !position;
                        self.escape = 0;
                    } else {
                        self.parameter(byte);
                    }
                }
                3 => {
                    // Only window/icon titles are allowed. Other OSC commands
                    // remain a full-screen fallback, as do incomplete strings.
                    if byte == b';' {
                        self.unsafe_output |=
                            !matches!(self.parameters.as_slice(), b"0" | b"1" | b"2");
                        self.escape = 4;
                    } else if byte.is_ascii_digit() {
                        self.parameter(byte);
                    } else {
                        self.unsafe_output = true;
                        self.escape = 0;
                    }
                }
                4 => match byte {
                    0x07 => self.escape = 0,
                    0x1b => self.escape = 5,
                    _ => {}
                },
                5 => {
                    self.unsafe_output |= byte != b'\\';
                    self.escape = 0;
                }
                _ => match byte {
                    0x1b => {
                        self.escape = 1;
                    }
                    b'\r' => self.carriage_return = true,
                    b'\n' | b'\t' | 0x20..=0x7e | 0x80..=0xff => {}
                    _ => self.unsafe_output = true,
                },
            }
        }
    }

    fn parameter(&mut self, byte: u8) {
        if self.parameters.len() < 32 {
            self.parameters.push(byte);
        } else {
            self.unsafe_output = true;
        }
    }

    pub(super) fn safe(&self) -> bool {
        !self.unsafe_output && self.escape == 0 && !self.carriage_return
    }
}

pub(super) struct TerminalScreen {
    pub(super) parser: vt100::Parser<TerminalReplies>,
    pub(super) audit: AppendAudit,
    pub(super) activity: OutputActivity,
}

impl TerminalScreen {
    pub(super) fn process(&mut self, bytes: &[u8]) {
        self.audit.process(bytes);
        self.parser.process(bytes);
        if !bytes.is_empty() {
            self.activity.record(Instant::now());
        }
    }

    pub(super) fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub(super) fn screen_mut(&mut self) -> &mut vt100::Screen {
        self.audit.unsafe_output = true;
        self.parser.screen_mut()
    }
}

#[derive(Default)]
pub(super) struct OutputActivity {
    pub(super) revision: u64,
    pub(super) last_output: Option<Instant>,
}

impl OutputActivity {
    pub(super) fn record(&mut self, now: Instant) {
        self.revision = self.revision.wrapping_add(1);
        self.last_output = Some(now);
    }

    pub(super) fn outcome(
        &self,
        revision: u64,
        deadline: Instant,
        now: Instant,
    ) -> Option<&'static str> {
        // Quiet must have been reached by the deadline, even if the owner wakes
        // slightly late. Receiving bytes (including an echo) is only a heuristic.
        if self.revision != revision
            && self
                .last_output
                .is_some_and(|last| last + SCREEN_QUIET <= now && last + SCREEN_QUIET <= deadline)
        {
            Some("quiet")
        } else if now >= deadline {
            Some("timeout")
        } else {
            None
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct PhysicalRow {
    pub(super) cells: Vec<String>,
    pub(super) wrapped: bool,
}

pub(super) struct ReadBaseline {
    pub(super) rows: Vec<PhysicalRow>,
    pub(super) history: usize,
    pub(super) cursor: (usize, usize),
    pub(super) size: (u16, u16),
    pub(super) alternate: bool,
}

impl ReadBaseline {
    pub(super) fn capture(screen: &vt100::Screen) -> Self {
        let mut past = screen.clone();
        past.set_scrollback(HISTORY);
        let history = past.scrollback();
        let (height, width) = screen.size();
        let mut rows = Vec::with_capacity(history + height as usize);
        let mut offset = history;
        while offset > 0 {
            past.set_scrollback(offset);
            let count = offset.min(height as usize);
            rows.extend((0..count).map(|row| physical_row(&past, row as u16)));
            offset -= count;
        }
        rows.extend((0..height).map(|row| physical_row(screen, row)));
        let (row, col) = screen.cursor_position();
        Self {
            rows,
            history,
            cursor: (history + row as usize, col as usize),
            size: (height, width),
            alternate: screen.alternate_screen(),
        }
    }

    pub(super) fn delta(&self, next: &Self, safe: bool) -> Option<(String, Vec<bool>)> {
        if !safe
            || self.size != next.size
            || self.alternate
            || next.alternate
            || next.history < self.history
            || next.history == HISTORY
            || self.history == HISTORY
            || next.cursor < self.cursor
            || next.cursor.0.saturating_sub(self.cursor.0) >= self.size.0 as usize
        {
            return None;
        }
        let (start_row, start_col) = self.cursor;
        let (end_row, end_col) = next.cursor;
        let old = self.rows.get(start_row)?;
        let new = next.rows.get(start_row)?;
        // A cursor at the last column can mean a pending automatic wrap. Fall
        // back instead of guessing which side of that cell is the append point.
        if start_col >= self.size.1 as usize - 1
            || end_col >= next.size.1 as usize - 1
            || self.rows[..start_row] != next.rows[..start_row]
            || old.cells[..start_col] != new.cells[..start_col]
            || old.cells[start_col..].iter().any(|c| c != " ")
            || self.rows[start_row + 1..]
                .iter()
                .any(|r| r.cells.iter().any(|c| c != " "))
            || next.rows.get(end_row)?.cells[end_col..]
                .iter()
                .any(|c| c != " ")
            || next.rows[end_row + 1..]
                .iter()
                .any(|r| r.cells.iter().any(|c| c != " "))
        {
            return None;
        }
        if self.cursor == next.cursor {
            return (self.rows == next.rows).then(|| (String::new(), Vec::new()));
        }
        let mut output = Vec::new();
        let mut wrapped = Vec::new();
        for row in start_row..=end_row {
            let physical = next.rows.get(row)?;
            let begin = if row == start_row { start_col } else { 0 };
            let end = if row == end_row {
                end_col
            } else {
                physical.cells.len()
            };
            // An empty final row still records a newly emitted line break.
            output.push(physical.cells[begin..end].concat());
            wrapped.push(physical.wrapped);
        }
        Some((output.join("\n"), wrapped))
    }
}

pub(super) fn physical_row(screen: &vt100::Screen, row: u16) -> PhysicalRow {
    let (_, cols) = screen.size();
    PhysicalRow {
        cells: (0..cols)
            .map(|col| match screen.cell(row, col) {
                Some(cell) if cell.is_wide_continuation() => String::new(),
                Some(cell) if !cell.contents().is_empty() => cell.contents().to_owned(),
                _ => " ".into(),
            })
            .collect(),
        wrapped: screen.row_wrapped(row),
    }
}

pub(super) fn grid(screen: &vt100::Screen) -> String {
    let (rows, cols) = screen.size();
    let mut output = String::new();
    for row in 0..rows {
        if row != 0 {
            output.push('\n');
        }
        for col in 0..cols {
            if let Some(cell) = screen.cell(row, col) {
                if cell.is_wide_continuation() {
                    continue;
                }
                if cell.contents().is_empty() {
                    output.push(' ');
                } else {
                    output.push_str(cell.contents());
                }
            }
        }
    }
    output
}
