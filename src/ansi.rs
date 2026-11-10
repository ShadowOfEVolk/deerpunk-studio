//! Byte-stream → styled text model for the console. Handles the subset of
//! ANSI that the engine logger and cargo emit: SGR colors/attrs, CSI (ignored
//! beyond clear-to-EOL), OSC titles, `\r` line rewrite. Stream mode — no
//! cursor addressing (see the console design decision).

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct TextStyle {
    /// Foreground RGB, `None` = default text color.
    pub color: Option<(u8, u8, u8)>,
    pub bold: bool,
    pub dim: bool,
}

impl TextStyle {
    const fn plain() -> Self {
        Self { color: None, bold: false, dim: false }
    }

    fn same(&self, other: &TextStyle) -> bool {
        self.color == other.color && self.bold == other.bold && self.dim == other.dim
    }
}

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: TextStyle,
}

/// Console stream: a fixed-capacity ring of finished lines plus the current
/// line being written.
pub struct Stream {
    pub lines: Vec<Vec<Span>>,
    cur: Vec<Span>,
    style: TextStyle,
    capacity: usize,
    seq: String,
    in_csi: bool,
    in_osc: bool,
    /// The byte after an OSC terminated by ESC (the `\` of ST) is swallowed.
    swallow_next: bool,
    /// Printable bytes awaiting a complete UTF-8 character.
    pending: Vec<u8>,
}

const BASE_COLORS: [(u8, u8, u8); 8] = [
    (0, 0, 0),
    (178, 34, 34),
    (67, 161, 67),
    (176, 130, 39),
    (68, 114, 196),
    (162, 78, 162),
    (51, 153, 153),
    (170, 170, 170),
];

const BRIGHT_COLORS: [(u8, u8, u8); 8] = [
    (110, 110, 110),
    (231, 106, 106),
    (108, 213, 108),
    (230, 190, 90),
    (110, 155, 245),
    (200, 120, 200),
    (90, 200, 200),
    (235, 235, 235),
];

impl Stream {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: Vec::new(),
            cur: Vec::new(),
            style: TextStyle::plain(),
            capacity,
            seq: String::new(),
            in_csi: false,
            in_osc: false,
            swallow_next: false,
            pending: Vec::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed_byte(b);
        }
        self.flush_bytes();
    }

    /// The current (not yet newline-terminated) line.
    pub fn pending(&self) -> &[Span] {
        &self.cur
    }

    fn flush_bytes(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let bytes = std::mem::take(&mut self.pending);
        match std::str::from_utf8(&bytes) {
            Ok(s) => self.push_text(s),
            Err(e) => {
                let valid = e.valid_up_to();
                if valid > 0 {
                    if let Ok(s) = std::str::from_utf8(&bytes[..valid]) {
                        self.push_text(s);
                    }
                }
                match e.error_len() {
                    // Invalid byte: dropped.
                    Some(_) => {}
                    // Incomplete character at the chunk end: keep for later.
                    None => self.pending = bytes[valid..].to_vec(),
                }
            }
        }
    }

    fn feed_byte(&mut self, b: u8) {
        if self.swallow_next {
            self.swallow_next = false;
            return;
        }
        if self.in_osc {
            // OSC: title etc, terminated by BEL or ST (ESC \).
            if b == 0x07 {
                self.in_osc = false;
            } else if b == 0x1b {
                self.in_osc = false;
                self.swallow_next = true;
            }
            return;
        }
        if self.in_csi {
            self.seq.push(b as char);
            // CSI final byte: 0x40..=0x7E.
            if (0x40..=0x7E).contains(&b) {
                self.in_csi = false;
                let params: String = self.seq[..self.seq.len() - 1].to_string();
                match b as char {
                    'm' => self.apply_sgr(&params),
                    'K' => { /* clear-to-EOL: text already merged, nothing to unwrite */ }
                    _ => {}
                }
                self.seq.clear();
            }
            return;
        }
        match b {
            0x1b => {
                self.flush_bytes();
                // Peek next byte to classify: handled by deferring — we treat
                // ESC as starting an escape; the next byte decides.
                self.seq.clear();
                self.seq.push('\x1b');
            }
            _ if self.seq.len() == 1 && self.seq.as_bytes()[0] == 0x1b => {
                self.flush_bytes();
                match b {
                    b'[' => {
                        self.in_csi = true;
                        self.seq.clear();
                    }
                    b']' => {
                        self.in_osc = true;
                        self.seq.clear();
                    }
                    _ => {
                        // Two-byte escape, ignore.
                        self.seq.clear();
                    }
                }
            }
            b'\n' => {
                self.flush_bytes();
                self.commit_line();
            }
            b'\r' => {
                self.flush_bytes();
                self.cur.clear();
            }
            b'\t' => {
                self.flush_bytes();
                self.push_text("    ");
            }
            0x00..=0x08 | 0x0b..=0x0c | 0x0e..=0x1a | 0x1c..=0x1f => {}
            _ => {
                // Printable (ASCII or UTF-8 continuation) — buffered so that
                // multi-byte characters assemble before rendering.
                self.pending.push(b);
            }
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.cur.last_mut() {
            if last.style.same(&self.style) {
                last.text.push_str(text);
                return;
            }
        }
        self.cur.push(Span { text: text.to_string(), style: self.style });
    }

    fn commit_line(&mut self) {
        let line = std::mem::take(&mut self.cur);
        if !line.is_empty() {
            self.lines.push(line);
            if self.lines.len() > self.capacity {
                let excess = self.lines.len() - self.capacity;
                self.lines.drain(..excess);
            }
        }
    }

    fn apply_sgr(&mut self, params: &str) {
        let parts: Vec<u32> = params
            .split(';')
            .map(|p| p.parse::<u32>().unwrap_or(0))
            .collect();
        let mut iter = parts.into_iter().peekable();
        while let Some(p) = iter.next() {
            match p {
                0 => self.style = TextStyle::plain(),
                1 => self.style.bold = true,
                2 => self.style.dim = true,
                22 => {
                    self.style.bold = false;
                    self.style.dim = false;
                }
                30..=37 => {
                    self.style.color = Some(BASE_COLORS[(p - 30) as usize]);
                }
                39 => self.style.color = None,
                90..=97 => {
                    self.style.color = Some(BRIGHT_COLORS[(p - 90) as usize]);
                }
                38 => {
                    // Extended color: 38;5;n or 38;2;r;g;b.
                    match iter.peek() {
                        Some(5) => {
                            iter.next();
                            if let Some(n) = iter.next() {
                                self.style.color = Some(xterm256(n));
                            }
                        }
                        Some(2) => {
                            iter.next();
                            let r = iter.next().unwrap_or(0);
                            let g = iter.next().unwrap_or(0);
                            let b = iter.next().unwrap_or(0);
                            self.style.color = Some((r as u8, g as u8, b as u8));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

fn xterm256(n: u32) -> (u8, u8, u8) {
    match n {
        0..=15 => {
            let i = n as usize;
            if i < 8 {
                BASE_COLORS[i]
            } else {
                BRIGHT_COLORS[i - 8]
            }
        }
        16..=231 => {
            let i = n - 16;
            let r = (i / 36) * 51;
            let g = ((i / 6) % 6) * 51;
            let b = (i % 6) * 51;
            (r.min(255) as u8, g.min(255) as u8, b.min(255) as u8)
        }
        _ => {
            let g = ((n - 232).min(23) * 10 + 8) as u8;
            (g, g, g)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_and_reset() {
        let mut s = Stream::new(100);
        s.feed(b"\x1b[36m[engine]\x1b[0m hello \x1b[90mgray\x1b[0m\n");
        assert_eq!(s.lines.len(), 1);
        let line = &s.lines[0];
        assert_eq!(line.len(), 3);
        assert_eq!(line[0].text, "[engine]");
        assert_eq!(line[0].style.color, Some((51, 153, 153)));
        assert_eq!(line[1].text, " hello ");
        assert!(line[1].style.color.is_none());
        assert_eq!(line[2].text, "gray");
        assert_eq!(line[2].style.color, Some((110, 110, 110)));
    }

    #[test]
    fn carriage_return_rewrites_line() {
        let mut s = Stream::new(100);
        s.feed(b"Building 1/9\rBuilding 9/9\nnext\n");
        assert_eq!(s.lines.len(), 2);
        assert_eq!(spans_text(&s.lines[0]), "Building 9/9");
        assert_eq!(spans_text(&s.lines[1]), "next");
    }

    #[test]
    fn osc_and_csi_ignored() {
        let mut s = Stream::new(100);
        s.feed(b"\x1b]0;window title\x07plain\x1b[2K\n");
        assert_eq!(s.lines.len(), 1);
        assert_eq!(spans_text(&s.lines[0]), "plain");
    }

    #[test]
    fn ring_capacity() {
        let mut s = Stream::new(3);
        for i in 0..5 {
            s.feed(format!("line{i}\n").as_bytes());
        }
        assert_eq!(s.lines.len(), 3);
        assert_eq!(spans_text(&s.lines[0]), "line2");
        assert_eq!(spans_text(&s.lines[2]), "line4");
    }

    fn spans_text(line: &[Span]) -> String {
        line.iter().map(|sp| sp.text.as_str()).collect()
    }
}
