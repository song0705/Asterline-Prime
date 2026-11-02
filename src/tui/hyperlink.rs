//! Clickable links: terminal hyperlinks (OSC 8) plus in-app click handling.
//!
//! Markdown link labels carry their destination invisibly: the label's style
//! gets an `underline_color` of the form `Rgb(MARK, id_hi, id_lo)`, where `id`
//! indexes a registry of safe URLs. A terminal only paints the underline
//! colour of underlined text and links are never underlined, so the marker is
//! invisible, and it travels with the cell through wrapping, scrolling,
//! selection and partial redraws for free.
//!
//! Two consumers read the markers:
//! - [`LinkBackend`] wraps the crossterm backend. When ratatui flushes the
//!   changed cells of a frame it wraps link cells in OSC 8 open/close
//!   sequences on a transient copy of the cell, so the frame buffer (and every
//!   width calculation) never contains escape bytes, and each run of link
//!   cells is closed before the cursor can move elsewhere.
//! - [`record_frame`] remembers which screen cells are links so a left click
//!   can resolve to a URL ([`url_at`]) and open it ([`open_url`]).

use std::cell::RefCell;
use std::io::{self, Write};
use std::sync::Mutex;

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Style};
use unicode_width::UnicodeWidthStr;

use crate::tui::theme;

/// First component of the marker colour; keeps ordinary RGB underline colours
/// (there are none today) from being mistaken for links.
const MARK: u8 = 0xA5;
const MAX_URL_LEN: usize = 2048;
const CLOSE: &str = "\x1b]8;;\x1b\\";

static URLS: Mutex<Vec<String>> = Mutex::new(Vec::new());

thread_local! {
    /// `(x, y, link id)` of every link cell in the most recent frame.
    static HITS: RefCell<Vec<(u16, u16, u16)>> = const { RefCell::new(Vec::new()) };
    #[cfg(test)]
    static OPENED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Whether `url` may be linked: an allowed scheme and no control bytes (C0,
/// DEL, C1) that could end the OSC sequence or smuggle in another escape.
pub(crate) fn is_safe_url(url: &str) -> bool {
    if url.is_empty() || url.len() > MAX_URL_LEN {
        return false;
    }
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\u{9c}')
    {
        return false;
    }
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    let scheme = scheme.to_ascii_lowercase();
    match scheme.as_str() {
        "http" | "https" | "file" => rest.starts_with("//") && rest.len() > 2,
        "mailto" => !rest.is_empty(),
        _ => false,
    }
}

fn lock_urls() -> std::sync::MutexGuard<'static, Vec<String>> {
    URLS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The marker colour for `url`, interning it; `None` for an unsafe URL.
fn link_color(url: &str) -> Option<Color> {
    if !is_safe_url(url) {
        return None;
    }
    let mut urls = lock_urls();
    let index = match urls.iter().position(|known| known == url) {
        Some(index) => index,
        None => {
            if urls.len() >= usize::from(u16::MAX) {
                return None;
            }
            urls.push(url.to_string());
            urls.len() - 1
        }
    };
    // Id 0 is never used so the marker is never `Rgb(MARK, 0, 0)`.
    let id = u16::try_from(index + 1).ok()?;
    Some(Color::Rgb(MARK, (id >> 8) as u8, (id & 0xff) as u8))
}

/// The style of a link label: the theme's link colour, no underline, and the
/// invisible destination marker. A plain style for an unsafe URL, so the label
/// is shown as ordinary text.
pub(crate) fn link_style(url: &str) -> Style {
    match link_color(url) {
        Some(color) => Style::default()
            .fg(theme::accent_color())
            .underline_color(color),
        None => Style::default(),
    }
}

fn link_id(color: Color) -> Option<u16> {
    match color {
        Color::Rgb(MARK, hi, lo) => {
            let id = u16::from(hi) << 8 | u16::from(lo);
            (id != 0).then_some(id)
        }
        _ => None,
    }
}

/// The destination hidden in a style's marker, if it has one.
#[cfg(test)]
pub(crate) fn url_of_style(style: Style) -> Option<String> {
    link_id(style.underline_color?).and_then(url_of)
}

fn url_of(id: u16) -> Option<String> {
    lock_urls().get(usize::from(id) - 1).cloned()
}

/// Remember the link cells of the frame that was just drawn.
pub(crate) fn record_frame(buffer: &Buffer) {
    HITS.with(|hits| {
        let mut hits = hits.borrow_mut();
        hits.clear();
        let area = buffer.area;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let Some(cell) = buffer.cell((x, y)) else {
                    continue;
                };
                let Some(id) = link_id(cell.underline_color) else {
                    continue;
                };
                hits.push((x, y, id));
                // The right half of a wide character belongs to the link too.
                if cell.symbol().width() >= 2 {
                    hits.push((x + 1, y, id));
                }
            }
        }
    });
}

/// The URL of the link under screen cell `(x, y)` in the last recorded frame.
pub(crate) fn url_at(x: u16, y: u16) -> Option<String> {
    let id = HITS.with(|hits| {
        hits.borrow()
            .iter()
            .find(|(hx, hy, _)| *hx == x && *hy == y)
            .map(|(_, _, id)| *id)
    })?;
    url_of(id)
}

/// Open `url` with the platform's opener, detached and without a shell. The
/// URL is passed as a single argument and must pass [`is_safe_url`].
pub(crate) fn open_url(url: &str) -> bool {
    if !is_safe_url(url) {
        return false;
    }
    #[cfg(test)]
    {
        OPENED.with(|opened| opened.borrow_mut().push(url.to_string()));
        true
    }
    #[cfg(not(test))]
    {
        use std::process::{Command, Stdio};
        #[cfg(target_os = "macos")]
        let mut command = Command::new("open");
        #[cfg(windows)]
        let mut command = Command::new("explorer");
        #[cfg(not(any(target_os = "macos", windows)))]
        let mut command = Command::new("xdg-open");
        command
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        match command.spawn() {
            Ok(mut child) => {
                // Reap the opener so it never lingers as a zombie.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(test)]
pub(crate) fn take_opened() -> Vec<String> {
    OPENED.with(|opened| std::mem::take(&mut *opened.borrow_mut()))
}

/// Crossterm backend that emits OSC 8 hyperlinks around link cells.
pub struct LinkBackend<W: Write> {
    inner: CrosstermBackend<W>,
}

impl<W: Write> LinkBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
        }
    }
}

/// Whether `next` is drawn immediately after `prev` on the same row.
fn adjacent(prev: (u16, u16, &Cell), next: (u16, u16, &Cell)) -> bool {
    let width = prev.2.symbol().width().max(1);
    prev.1 == next.1 && usize::from(next.0) == usize::from(prev.0) + width
}

impl<W: Write> Backend for LinkBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let cells: Vec<(u16, u16, &Cell)> = content.collect();
        let ids: Vec<Option<u16>> = cells
            .iter()
            .map(|(_, _, cell)| link_id(cell.underline_color).filter(|id| url_of(*id).is_some()))
            .collect();
        if ids.iter().all(Option::is_none) {
            return self.inner.draw(cells.into_iter());
        }
        let mut replaced: Vec<Option<Cell>> = Vec::with_capacity(cells.len());
        for (index, &(x, y, cell)) in cells.iter().enumerate() {
            let Some(id) = ids[index] else {
                replaced.push(None);
                continue;
            };
            let continues_run =
                index > 0 && ids[index - 1] == Some(id) && adjacent(cells[index - 1], (x, y, cell));
            let run_goes_on = index + 1 < cells.len()
                && ids[index + 1] == Some(id)
                && adjacent((x, y, cell), cells[index + 1]);
            let mut symbol = String::new();
            if !continues_run && let Some(url) = url_of(id) {
                symbol.push_str(&format!("\x1b]8;id={id};{url}\x1b\\"));
            }
            symbol.push_str(cell.symbol());
            if !run_goes_on {
                symbol.push_str(CLOSE);
            }
            let mut copy = cell.clone();
            copy.set_symbol(&symbol);
            // The marker is not a real underline colour; keep it off the wire.
            copy.underline_color = Color::Reset;
            replaced.push(Some(copy));
        }
        self.inner.draw(
            cells
                .iter()
                .zip(replaced.iter())
                .map(|(&(x, y, cell), copy)| (x, y, copy.as_ref().unwrap_or(cell))),
        )
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[derive(Clone, Default)]
    struct Sink(std::rc::Rc<RefCell<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Sink {
        fn text(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).unwrap()
        }
    }

    fn draw_all(buffer: &Buffer) -> String {
        let sink = Sink::default();
        let mut backend = LinkBackend::new(sink.clone());
        // Draw what a real frame would: the diff against a blank screen.
        let blank = Buffer::empty(buffer.area);
        let cells = blank.diff(buffer);
        backend.draw(cells.into_iter()).unwrap();
        sink.text()
    }

    #[test]
    fn unsafe_urls_are_rejected() {
        for ok in [
            "https://example.com/a?b=1",
            "http://x.io",
            "mailto:a@b.co",
            "file:///tmp/a.txt",
            "HTTPS://EXAMPLE.COM",
        ] {
            assert!(is_safe_url(ok), "{ok}");
        }
        for bad in [
            "",
            "javascript:alert(1)",
            "ftp://x.io",
            "data:text/html,hi",
            "https://x.io/\x1b]52;c;AA\x07",
            "https://x.io/a\x07b",
            "https://x.io/a\u{9c}b",
            "https://x.io/a b",
            "https://x.io/\nfoo",
            "/relative/path",
            "https:",
            "mailto:",
        ] {
            assert!(!is_safe_url(bad), "{bad:?}");
        }
        assert!(!is_safe_url(&format!("https://x.io/{}", "a".repeat(3000))));
        assert_eq!(link_style("javascript:alert(1)"), Style::default());
        assert!(link_style("https://x.io").underline_color.is_some());
    }

    #[test]
    fn writer_wraps_exactly_the_link_cells_and_closes_them() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 12, 2));
        buffer.set_string(0, 0, "ab ", Style::default());
        buffer.set_string(3, 0, "link", link_style("https://one.io/x"));
        buffer.set_string(7, 0, " cd", Style::default());
        buffer.set_string(0, 1, "你好", link_style("https://one.io/x"));
        let out = draw_all(&buffer);
        let open = |url: &str| {
            format!(
                "\x1b]8;id={};{url}\x1b\\",
                link_id(link_color(url).unwrap()).unwrap()
            )
        };
        // One region for the ASCII run, one for the wide-character run.
        assert_eq!(out.matches("\x1b]8;id=").count(), 2, "{out:?}");
        assert_eq!(out.matches(CLOSE).count(), 2, "{out:?}");
        assert!(
            out.contains(&format!("{}l", open("https://one.io/x"))),
            "{out:?}"
        );
        assert!(out.contains(&format!("k{CLOSE}")), "{out:?}");
        // Plain cells are untouched and the marker colour never reaches the wire.
        assert!(out.contains("ab"));
        assert!(!out.contains("58;2;165"), "{out:?}");
        // Every region is closed before the next one opens.
        let mut depth = 0i32;
        for part in out.split("\x1b]8;") {
            if part.starts_with(';') {
                depth -= 1;
            } else if part.starts_with("id=") {
                depth += 1;
            }
            assert!((0..=1).contains(&depth), "{out:?}");
        }
        assert_eq!(depth, 0);
        // The buffer itself never contains escape bytes.
        assert!(
            buffer
                .content()
                .iter()
                .all(|c| !c.symbol().contains('\x1b'))
        );
    }

    #[test]
    fn a_changed_link_cell_alone_is_still_wrapped() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 1));
        buffer.set_string(2, 0, "x", link_style("https://lone.io"));
        let sink = Sink::default();
        let mut backend = LinkBackend::new(sink.clone());
        let cell = buffer.cell((2, 0)).unwrap();
        backend.draw(std::iter::once((2u16, 0u16, cell))).unwrap();
        let out = sink.text();
        assert!(
            out.contains("https://lone.io\x1b\\x\x1b]8;;\x1b\\"),
            "{out:?}"
        );
    }

    #[test]
    fn hits_resolve_cells_including_the_wide_right_half() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 3));
        buffer.set_string(4, 1, "官方", link_style("https://hit.io/a"));
        buffer.set_string(0, 2, "plain", Style::default());
        record_frame(&buffer);
        assert_eq!(url_at(4, 1).as_deref(), Some("https://hit.io/a"));
        assert_eq!(url_at(5, 1).as_deref(), Some("https://hit.io/a"));
        assert_eq!(url_at(7, 1).as_deref(), Some("https://hit.io/a"));
        assert_eq!(url_at(8, 1), None);
        assert_eq!(url_at(0, 2), None);
        assert_eq!(url_at(4, 0), None);
    }

    #[test]
    fn open_rejects_unsafe_urls_and_records_safe_ones() {
        let _ = take_opened();
        assert!(!open_url("javascript:alert(1)"));
        assert!(open_url("https://ok.io"));
        assert_eq!(take_opened(), ["https://ok.io"]);
    }
}
