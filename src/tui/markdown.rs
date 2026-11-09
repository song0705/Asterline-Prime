//! Markdown rendering for agent messages, built on real libraries rather than a
//! hand-rolled approximation: [`pulldown_cmark`] parses the document, [`syntect`]
//! highlights fenced code blocks (multi-language, themed), and wrapping is
//! `unicode-width`-aware so CJK and other wide glyphs lay out correctly.
//!
//! The public surface is intentionally small and stable: [`render`] turns
//! markdown into styled `ratatui` lines wrapped to a width, and [`wrap`] is a
//! plain width-aware word wrapper shared by the non-markdown chat cells.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;

use std::borrow::Cow;
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::{hyperlink, theme};

/// Default syntaxes and a dark theme, loaded once. `syntect` parsing is backed
/// by `fancy-regex` (pure Rust), so there is no C toolchain dependency.
static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
static THEME: LazyLock<Theme> = LazyLock::new(|| {
    let mut set = ThemeSet::load_defaults();
    set.themes
        .remove("base16-ocean.dark")
        .or_else(|| set.themes.values().next().cloned())
        .expect("syntect ships default themes")
});

const MD_CACHE_CAP: usize = 128;

/// Rendered lines keyed by (text hash, whether typed line breaks are kept).
type RenderMap = HashMap<(u64, bool), Vec<Line<'static>>>;

thread_local! {
    static RENDER_CACHE: RefCell<(usize, RenderMap)> =
        RefCell::new((0, HashMap::new()));
}

/// Render Markdown `text` to styled lines wrapped to `width`.
pub(crate) fn render(text: &str, width: usize) -> Vec<Line<'static>> {
    render_cached(text, width, false)
}

/// Render a user's own prompt: Markdown as in [`render`], except that every
/// line break the user typed is kept instead of being joined into one line.
pub(crate) fn render_user(text: &str, width: usize) -> Vec<Line<'static>> {
    render_cached(text, width, true)
}

fn render_cached(text: &str, width: usize, keep_line_breaks: bool) -> Vec<Line<'static>> {
    let width = width.max(1);
    let key = (fnv1a64(text.as_bytes()), keep_line_breaks);
    RENDER_CACHE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if cache.0 != width {
            cache.0 = width;
            cache.1.clear();
        }
        if let Some(hit) = cache.1.get(&key) {
            return hit.clone();
        }
        let lines = render_uncached(text, width, keep_line_breaks);
        if cache.1.len() >= MD_CACHE_CAP {
            cache.1.clear();
        }
        cache.1.insert(key, lines.clone());
        lines
    })
}

fn parser_options() -> Options {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);
    options
}

fn render_uncached(text: &str, width: usize, keep_line_breaks: bool) -> Vec<Line<'static>> {
    let mut renderer = Renderer::new(width);
    renderer.keep_line_breaks = keep_line_breaks;
    let prepared = fix_cjk_emphasis(text);
    for event in Parser::new_ext(&prepared, parser_options()) {
        renderer.handle(event);
    }
    renderer.finish()
}

/// Find the first bare `http://` / `https://` URL in `text` as a byte range.
/// A URL ends at whitespace or the first non-ASCII character (CJK text often
/// follows a URL with no space) and drops trailing sentence punctuation.
fn find_bare_url(text: &str) -> Option<(usize, usize)> {
    let mut from = 0;
    while let Some(offset) = text[from..].find("://") {
        let sep = from + offset;
        from = sep + 3;
        let scheme_start = text[..sep]
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_ascii_alphabetic())
            .map_or(0, |(i, c)| i + c.len_utf8());
        let scheme = &text[scheme_start..sep];
        if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) {
            continue;
        }
        let mut end = text[sep + 3..]
            .find(|c: char| c.is_whitespace() || !c.is_ascii() || c == '<' || c == '>')
            .map_or(text.len(), |i| sep + 3 + i);
        loop {
            let url = &text[scheme_start..end];
            let Some(last) = url.chars().last() else {
                break;
            };
            let trim = match last {
                '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' => true,
                ')' => url.matches('(').count() < url.matches(')').count(),
                ']' => url.matches('[').count() < url.matches(']').count(),
                _ => false,
            };
            if !trim {
                break;
            }
            end -= 1;
        }
        if end > sep + 3 {
            return Some((scheme_start, end));
        }
    }
    None
}

/// Zero width space: neither whitespace nor punctuation for CommonMark's
/// flanking rules. It is inserted next to `*` runs to make emphasis around CJK
/// punctuation work, and stripped again from every text event.
const ZWSP: char = '\u{200b}';

fn strip_zwsp(text: &str) -> Cow<'_, str> {
    if text.contains(ZWSP) {
        Cow::Owned(text.replace(ZWSP, ""))
    } else {
        Cow::Borrowed(text)
    }
}

/// Non-ASCII punctuation that CommonMark treats as punctuation but that sits
/// directly against CJK text with no space (so `。**它` never closes bold).
fn is_cjk_punct(c: char) -> bool {
    matches!(
        c as u32,
        0x00B7
            | 0x2010..=0x2027
            | 0x2030..=0x205E
            | 0x3001..=0x3003
            | 0x3008..=0x3011
            | 0x3014..=0x301F
            | 0x3030
            | 0x303D
            | 0x30FB
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6B
            | 0xFF01..=0xFF0F
            | 0xFF1A..=0xFF20
            | 0xFF3B..=0xFF40
            | 0xFF5B..=0xFF65
    )
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || is_cjk_punct(c)
}

/// CommonMark left/right-flanking test for a `*` run, with `None` meaning a
/// line boundary (treated as whitespace).
fn flanking(prev: Option<char>, next: Option<char>) -> (bool, bool) {
    let ws = |c: Option<char>| c.is_none_or(char::is_whitespace);
    let punct = |c: Option<char>| c.is_some_and(is_punct);
    let can_open = !ws(next) && (!punct(next) || ws(prev) || punct(prev));
    let can_close = !ws(prev) && (!punct(prev) || ws(next) || punct(next));
    (can_open, can_close)
}

/// CommonMark refuses `**` that touches CJK punctuation on one side and a
/// letter on the other (`找人。**它`, `是**“词”**`), so bold leaks as literal
/// asterisks. Insert a zero width space on the punctuation side of such runs
/// so they become valid openers/closers. Code spans, code blocks and raw HTML
/// are left untouched, and only runs whose meaning is unambiguous are changed.
fn fix_cjk_emphasis(text: &str) -> Cow<'_, str> {
    if !text.contains('*') || !text.chars().any(is_cjk_punct) {
        return Cow::Borrowed(text);
    }
    let mut protected: Vec<Range<usize>> = Vec::new();
    for (event, range) in Parser::new_ext(text, parser_options()).into_offset_iter() {
        match event {
            Event::Code(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::Start(Tag::CodeBlock(_)) => protected.push(range),
            _ => {}
        }
    }
    protected.sort_by_key(|r| r.start);

    let mut out = String::with_capacity(text.len() + 16);
    let mut changed = false;
    // Lengths of `*` runs that opened and have not closed yet.
    let mut open: Vec<usize> = Vec::new();
    let mut next_protected = 0usize;
    let mut i = 0usize;
    while i < text.len() {
        while next_protected < protected.len() && protected[next_protected].end <= i {
            next_protected += 1;
        }
        if let Some(range) = protected.get(next_protected).filter(|r| r.start <= i) {
            out.push_str(&text[i..range.end]);
            i = range.end;
            continue;
        }
        let Some(c) = text[i..].chars().next() else {
            break;
        };
        if c == '\n' {
            let rest = &text[i + 1..];
            let next_line = rest.split('\n').next().unwrap_or("");
            let this_line = text[..i].rsplit('\n').next().unwrap_or("");
            if next_line.trim().is_empty() || this_line.trim_start().starts_with('|') {
                open.clear();
            }
        }
        if c != '*' {
            out.push(c);
            i += c.len_utf8();
            continue;
        }
        let mut j = i;
        while text[j..].starts_with('*') {
            j += 1;
        }
        let len = j - i;
        let run = &text[i..j];
        let escaped = text[..i].bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1;
        if escaped {
            out.push_str(run);
            i = j;
            continue;
        }
        let prev = text[..i].chars().next_back();
        let next = text[j..].chars().next();
        let (can_open, can_close) = flanking(prev, next);
        let has_open = open.contains(&len);
        let close = |open: &mut Vec<usize>| {
            if let Some(pos) = open.iter().rposition(|l| *l == len) {
                open.truncate(pos);
            }
        };
        let mut before = false;
        let mut after = false;
        if !can_open
            && can_close
            && next.is_some_and(is_cjk_punct)
            && prev.is_some_and(|p| !is_punct(p))
        {
            // `字**，`: a closer when something is open, else an opener that
            // needs help because CJK punctuation follows it.
            if has_open {
                close(&mut open);
            } else {
                after = true;
                open.push(len);
            }
        } else if can_open && !can_close && prev.is_some_and(is_cjk_punct) && has_open {
            // `。**字` closing an earlier opener.
            before = true;
            close(&mut open);
        } else if can_open && can_close {
            if has_open {
                close(&mut open);
            } else {
                open.push(len);
            }
        } else if can_open {
            open.push(len);
        } else if can_close && has_open {
            close(&mut open);
        }
        if before {
            out.push(ZWSP);
            changed = true;
        }
        out.push_str(run);
        if after {
            out.push(ZWSP);
            changed = true;
        }
        i = j;
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// A single styled character, the unit the span-aware wrapper works in.
type Unit = (char, Style);

/// Block-level rendering mode for routing `Text` events.
enum Mode {
    Normal,
    Code { lang: Option<String>, buf: String },
    Table(Table),
}

type TableCell = Vec<Unit>;

struct Table {
    alignments: Vec<pulldown_cmark::Alignment>,
    rows: Vec<Vec<TableCell>>,
    row: Vec<TableCell>,
    cell: TableCell,
    head_rows: usize,
    in_head: bool,
}

/// An open `[label](url)` link. Only the label is shown; the destination rides
/// invisibly in the label's style (see [`crate::tui::hyperlink`]).
struct LinkState {
    /// Label style (link colour plus the click target), or the plain style
    /// when the destination is not a safe URL.
    style: Style,
}

struct Renderer {
    width: usize,
    out: Vec<Line<'static>>,
    cur: Vec<Unit>,
    mode: Mode,
    bold: bool,
    italic: bool,
    strike: bool,
    link: Option<LinkState>,
    heading: Option<HeadingLevel>,
    /// One entry per open list; `Some(n)` is the next ordered number.
    lists: Vec<Option<u64>>,
    /// Per open list: the column where the current item's text starts, so
    /// nested lists and continuation lines align under it.
    item_cols: Vec<usize>,
    /// Inside a footnote definition (rendered as a dim list entry).
    in_footnote: bool,
    /// The previous top-level block was a footnote definition.
    after_footnote: bool,
    quote_depth: usize,
    /// Whether the outermost open block quote already holds a block.
    quote_has_content: bool,
    /// First-line prefix for the current list item (consumed on first flush).
    pending_prefix: Option<String>,
    /// Pulldown may split malformed emphasis markers into one-character text
    /// events; coalesce adjacent equal-style text before tolerant recovery.
    normal_text: Option<(String, Style)>,
    /// Treat a soft line break in prose as a hard one (user prompts).
    keep_line_breaks: bool,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            cur: Vec::new(),
            mode: Mode::Normal,
            bold: false,
            italic: false,
            strike: false,
            link: None,
            heading: None,
            lists: Vec::new(),
            item_cols: Vec::new(),
            in_footnote: false,
            after_footnote: false,
            quote_depth: 0,
            quote_has_content: false,
            pending_prefix: None,
            normal_text: None,
            keep_line_breaks: false,
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        if !self.cur.is_empty() {
            self.flush_block();
        }
        self.out
    }

    fn handle(&mut self, event: Event<'_>) {
        if !matches!(&event, Event::Text(_) | Event::SoftBreak) {
            self.flush_normal_text();
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&strip_zwsp(&text)),
            Event::Code(code) => {
                let style = self.inline_style();
                let style = if matches!(self.mode, Mode::Table(_)) {
                    style
                } else {
                    style.fg(theme::warning_color()).bg(theme::code_bg())
                };
                self.push_units(&code, style);
            }
            // In prose a soft break is a newline unit: the wrapper turns it
            // into a space, or into nothing between two CJK characters.
            Event::SoftBreak if self.keep_line_breaks && matches!(self.mode, Mode::Normal) => {
                self.flush_block();
            }
            Event::SoftBreak => {
                let soft = if matches!(self.mode, Mode::Normal) {
                    "\n"
                } else {
                    " "
                };
                self.text(soft);
            }
            Event::HardBreak => self.flush_block(),
            Event::Rule => {
                self.block_separator();
                self.out.push(Line::from(Span::styled(
                    "─".repeat(self.width.max(1)),
                    theme::dim(),
                )));
            }
            // Raw HTML / math: surface their text verbatim.
            Event::Html(s)
            | Event::InlineHtml(s)
            | Event::InlineMath(s)
            | Event::DisplayMath(s) => {
                self.push_units(&strip_zwsp(&s), self.inline_style());
            }
            Event::TaskListMarker(done) => {
                let (mark, color) = if done {
                    ("☑ ", theme::success_color())
                } else {
                    ("☐ ", theme::muted_color())
                };
                self.push_units(mark, Style::default().fg(color));
            }
            Event::FootnoteReference(name) => {
                self.push_units(&format!("[{name}]"), theme::dim());
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        self.flush_normal_text();
        match tag {
            Tag::Paragraph => self.block_separator(),
            Tag::Heading { level, .. } => {
                self.block_separator();
                self.heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.block_separator();
                if self.quote_depth == 0 {
                    self.quote_has_content = false;
                }
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.block_separator();
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let token = info.split_whitespace().next().unwrap_or("");
                        (!token.is_empty()).then(|| token.to_string())
                    }
                    CodeBlockKind::Indented => None,
                };
                self.mode = Mode::Code {
                    lang,
                    buf: String::new(),
                };
            }
            Tag::List(start) => {
                // Flush a tight parent item's text before descending, so its
                // line isn't merged with the nested item's content.
                if !self.cur.is_empty() {
                    self.flush_block();
                }
                if self.lists.is_empty() {
                    self.block_separator();
                }
                self.lists.push(start);
                self.item_cols.push(0);
            }
            Tag::Item => {
                let depth = self.lists.len();
                let parent_col = if depth >= 2 {
                    self.item_cols[depth - 2]
                } else {
                    0
                };
                let base = " ".repeat(parent_col);
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{base}{n}. ");
                        *n += 1;
                        m
                    }
                    _ => format!("{base}• "),
                };
                if let Some(col) = self.item_cols.last_mut() {
                    *col = display_width(&marker);
                }
                self.pending_prefix = Some(marker);
            }
            Tag::FootnoteDefinition(name) => {
                if !self.after_footnote {
                    self.block_separator();
                }
                self.in_footnote = true;
                self.pending_prefix = Some(format!("[{name}] "));
            }
            Tag::Strong => self.bold = true,
            Tag::Emphasis => self.italic = true,
            Tag::Strikethrough => self.strike = true,
            Tag::Link { dest_url, .. } => {
                self.link = Some(LinkState {
                    style: hyperlink::link_style(&strip_zwsp(&dest_url)),
                });
            }
            Tag::Table(alignments) => {
                self.block_separator();
                self.mode = Mode::Table(Table {
                    alignments,
                    rows: Vec::new(),
                    row: Vec::new(),
                    cell: Vec::new(),
                    head_rows: 0,
                    in_head: false,
                });
            }
            Tag::TableHead => {
                if let Mode::Table(t) = &mut self.mode {
                    t.in_head = true;
                }
            }
            Tag::TableRow => {}
            Tag::TableCell => {
                if let Mode::Table(t) = &mut self.mode {
                    t.cell.clear();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        self.flush_normal_text();
        match tag {
            TagEnd::Paragraph => self.flush_block(),
            TagEnd::Heading(_) => {
                self.flush_block();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => self.quote_depth = self.quote_depth.saturating_sub(1),
            TagEnd::CodeBlock => self.flush_code(),
            TagEnd::FootnoteDefinition => {
                if !self.cur.is_empty() {
                    self.flush_block();
                }
                self.in_footnote = false;
                self.after_footnote = true;
                self.pending_prefix = None;
            }
            TagEnd::List(_) => {
                self.lists.pop();
                self.item_cols.pop();
                self.pending_prefix = None;
            }
            TagEnd::Item => {
                if !self.cur.is_empty() {
                    self.flush_block();
                }
                self.pending_prefix = None;
            }
            TagEnd::Strong => self.bold = false,
            TagEnd::Emphasis => self.italic = false,
            TagEnd::Strikethrough => self.strike = false,
            TagEnd::Link => self.link = None,
            TagEnd::Table => self.flush_table(),
            TagEnd::TableCell => {
                if let Mode::Table(t) = &mut self.mode {
                    let cell = std::mem::take(&mut t.cell);
                    t.row.push(cell);
                }
            }
            TagEnd::TableHead => {
                if let Mode::Table(t) = &mut self.mode {
                    t.rows.push(std::mem::take(&mut t.row));
                    t.head_rows = t.rows.len();
                    t.in_head = false;
                }
            }
            TagEnd::TableRow => {
                if let Mode::Table(t) = &mut self.mode {
                    t.rows.push(std::mem::take(&mut t.row));
                }
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        let style = self.inline_style();
        match &mut self.mode {
            Mode::Code { buf, .. } => buf.push_str(text),
            Mode::Table(t) => t.cell.extend(text.chars().map(|ch| (ch, style))),
            Mode::Normal => {
                let same_style = self
                    .normal_text
                    .as_ref()
                    .is_some_and(|(_, current)| *current == style);
                if !same_style {
                    self.flush_normal_text();
                    self.normal_text = Some((String::new(), style));
                }
                if let Some((buffer, _)) = self.normal_text.as_mut() {
                    buffer.push_str(text);
                }
            }
        }
    }

    fn flush_normal_text(&mut self) {
        if let Some((text, style)) = self.normal_text.take() {
            self.push_loose_strong(&text, style);
        }
    }

    /// Some model output puts whitespace inside the closing strong delimiter
    /// (`**macOS: **`). CommonMark leaves that text literal; accept this one
    /// unambiguous near-miss without changing code blocks or valid Markdown.
    fn push_loose_strong(&mut self, text: &str, style: Style) {
        let mut rest = text;
        while let Some(open) = rest.find("**") {
            self.push_prose(&rest[..open], style);
            let after_open = &rest[open + 2..];
            let Some(close) = after_open.find("**") else {
                self.push_prose(&rest[open..], style);
                return;
            };
            let inner = &after_open[..close];
            self.emit_loose_strong(inner, style);
            rest = &after_open[close + 2..];
        }
        self.push_prose(rest, style);
    }

    fn emit_loose_strong(&mut self, inner: &str, style: Style) {
        let content = inner.trim_end();
        if content.is_empty() || content.len() == inner.len() {
            self.push_prose("**", style);
            self.push_prose(inner, style);
            self.push_prose("**", style);
            return;
        }
        self.push_prose(content, style.add_modifier(Modifier::BOLD));
        self.push_prose(&inner[content.len()..], style);
    }

    /// Push prose text, styling bare `http(s)://` URLs like link labels.
    fn push_prose(&mut self, text: &str, style: Style) {
        if self.link.is_some() || !text.contains("://") {
            self.push_units(text, style);
            return;
        }
        let mut rest = text;
        while let Some((start, end)) = find_bare_url(rest) {
            self.push_units(&rest[..start], style);
            self.push_units(
                &rest[start..end],
                style.patch(hyperlink::link_style(&rest[start..end])),
            );
            rest = &rest[end..];
        }
        self.push_units(rest, style);
    }

    fn push_units(&mut self, text: &str, style: Style) {
        let target = match &mut self.mode {
            Mode::Table(table) => &mut table.cell,
            _ => &mut self.cur,
        };
        target.extend(text.chars().map(|ch| (ch, style)));
    }

    /// The active inline style from the open emphasis/heading/link/quote state.
    fn inline_style(&self) -> Style {
        let mut style = Style::default();
        if let Some(level) = self.heading {
            let color = match level {
                HeadingLevel::H1 => theme::accent_color(),
                _ => theme::emphasis_color(),
            };
            style = style.fg(color).add_modifier(Modifier::BOLD);
        } else if self.in_footnote {
            style = style.fg(theme::dim_color());
        } else if self.quote_depth > 0 {
            style = style.fg(theme::muted_color());
        }
        if let Some(link) = &self.link {
            style = style.patch(link.style);
        }
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    }

    /// Separate the block that is about to start from the previous one: one
    /// blank line between top-level blocks (never doubled, never at the very
    /// start), a bare quote bar between blocks of the same quote, and nothing
    /// inside lists so tight items stay adjacent.
    fn block_separator(&mut self) {
        if !self.lists.is_empty() || self.in_footnote {
            return;
        }
        self.after_footnote = false;
        if self.quote_depth == 0 {
            if !self.cur.is_empty() {
                self.flush_block();
            }
            let last_is_blank = self
                .out
                .last()
                .is_none_or(|line| line.spans.iter().all(|s| s.content.trim().is_empty()));
            if !last_is_blank {
                self.out.push(Line::raw(""));
            }
        } else if self.quote_has_content {
            let bar = "▎ ".repeat(self.quote_depth);
            let last_is_bar = self.out.last().is_some_and(|line| {
                line.spans
                    .iter()
                    .all(|s| s.content.chars().all(|c| c == '▎' || c == ' '))
            });
            if !last_is_bar {
                self.out.push(Line::from(Span::styled(
                    bar.trim_end().to_string(),
                    theme::dim(),
                )));
            }
        }
    }

    /// Wrap and emit the accumulated inline content as one block, applying the
    /// current list-item / blockquote prefix.
    fn flush_block(&mut self) {
        self.flush_normal_text();
        let (first, cont) = self.prefixes();
        let prefix_style = if self.quote_depth > 0 || self.in_footnote {
            theme::dim()
        } else {
            Style::default().fg(theme::accent_color())
        };
        if self.quote_depth > 0 {
            self.quote_has_content = true;
        }
        let prefix_width = display_width(&first).max(display_width(&cont));
        let avail = self.width.saturating_sub(prefix_width).max(1);
        let wrapped = wrap_units(&self.cur, avail);
        self.cur.clear();

        if wrapped.is_empty() {
            return;
        }
        for (i, units) in wrapped.into_iter().enumerate() {
            let prefix = if i == 0 { &first } else { &cont };
            let mut spans = Vec::new();
            if !prefix.is_empty() {
                spans.push(Span::styled(prefix.clone(), prefix_style));
            }
            spans.extend(coalesce(units));
            self.out.push(Line::from(spans));
        }
    }

    /// The first-line and continuation prefixes for the current block, combining
    /// any blockquote bar with the pending list-item marker.
    fn prefixes(&mut self) -> (String, String) {
        let quote = "▎ ".repeat(self.quote_depth);
        let (first_marker, cont_marker) = match self.pending_prefix.take() {
            Some(marker) => {
                let indent = " ".repeat(display_width(&marker));
                // A later paragraph in the same item aligns under the text.
                self.pending_prefix = Some(indent.clone());
                (marker, indent)
            }
            None if !self.lists.is_empty() => {
                let indent = " ".repeat(self.item_cols.last().copied().unwrap_or(0));
                (indent.clone(), indent)
            }
            None => (String::new(), String::new()),
        };
        (
            format!("{quote}{first_marker}"),
            format!("{quote}{cont_marker}"),
        )
    }

    fn flush_code(&mut self) {
        let Mode::Code { lang, buf } = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        if let Some(lang) = &lang {
            self.out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(lang.clone(), theme::dim()),
            ]));
        }
        let room = self.width.saturating_sub(2).max(1);
        self.out.extend(highlight_code(&buf, lang.as_deref(), room));
    }

    fn flush_table(&mut self) {
        let Mode::Table(table) = std::mem::replace(&mut self.mode, Mode::Normal) else {
            return;
        };
        for line in render_table(&table, self.width) {
            self.out.push(line);
        }
    }
}

/// Highlight a fenced code block with syntect. Lines wider than `room` are
/// hard-broken (never cut); continuation rows start with a dim `↪` marker in
/// the two-column gutter that otherwise holds plain indentation.
fn highlight_code(code: &str, lang: Option<&str>, room: usize) -> Vec<Line<'static>> {
    let syntax = lang
        .and_then(|l| SYNTAXES.find_syntax_by_token(l))
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, &THEME);

    let mut out = Vec::new();
    for line in LinesWithEndings::from(code) {
        let expanded = line.replace('\t', "    ");
        let ranges = highlighter
            .highlight_line(&expanded, &SYNTAXES)
            .unwrap_or_default();
        let mut units: Vec<Unit> = Vec::new();
        for (syn, text) in ranges {
            let style = syntect_style(syn);
            for ch in text.trim_end_matches(['\n', '\r']).chars() {
                units.push((ch, style));
            }
        }
        let rows = hard_break(units, room);
        for (index, row) in rows.into_iter().enumerate() {
            let gutter = if index == 0 {
                Span::raw("  ")
            } else {
                Span::styled("↪ ", theme::dim())
            };
            let mut spans = vec![gutter];
            spans.extend(coalesce(row));
            out.push(Line::from(spans));
        }
    }
    out
}

fn syntect_color(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// Map a syntect style (color + font flags) to a ratatui style.
fn syntect_style(syn: syntect::highlighting::Style) -> Style {
    let mut style = Style::default().fg(syntect_color(syn.foreground));
    if syn.font_style.contains(FontStyle::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if syn.font_style.contains(FontStyle::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    style
}

/// Syntax-highlight a single line of code for a file `extension` (or language
/// token), returning styled spans. Stateless per line — handy for diff views.
pub(crate) fn highlight_code_line(line: &str, extension: &str) -> Vec<Span<'static>> {
    let syntax = SYNTAXES
        .find_syntax_by_extension(extension)
        .or_else(|| SYNTAXES.find_syntax_by_token(extension))
        .unwrap_or_else(|| SYNTAXES.find_syntax_plain_text());
    let mut highlighter = HighlightLines::new(syntax, &THEME);
    match highlighter.highlight_line(line, &SYNTAXES) {
        Ok(ranges) => ranges
            .into_iter()
            .map(|(syn, text)| Span::styled(text.to_string(), syntect_style(syn)))
            .collect(),
        Err(_) => vec![Span::raw(line.to_string())],
    }
}

/// Render a parsed table with box-drawing rules. Columns keep their natural
/// width when the table fits; otherwise the widest columns shrink and long
/// cells wrap onto extra lines inside the same row (nothing is truncated).
fn render_table(table: &Table, width: usize) -> Vec<Line<'static>> {
    if table.rows.is_empty() {
        return Vec::new();
    }
    let cols = table.rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }
    let col_w = table_column_widths(table, cols, width);

    let header_style = Style::default()
        .fg(theme::emphasis_color())
        .add_modifier(Modifier::BOLD);
    let body_style = Style::default();
    let rule_style = theme::dim();
    let mut out = vec![Line::from(Span::styled(
        table_rule(&col_w, '┌', '┬', '┐'),
        rule_style,
    ))];
    for (r, row) in table.rows.iter().enumerate() {
        let style = if r < table.head_rows {
            header_style
        } else {
            body_style
        };
        let cells: Vec<Vec<TableCell>> = col_w
            .iter()
            .enumerate()
            .map(|(c, w)| wrap_cell(row.get(c).map(Vec::as_slice).unwrap_or(&[]), *w))
            .collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for line_index in 0..height {
            let mut spans = vec![Span::styled("│ ", rule_style)];
            for (c, w) in col_w.iter().enumerate() {
                let piece = cells[c].get(line_index).map(Vec::as_slice).unwrap_or(&[]);
                spans.extend(coalesce(pad_cell(
                    piece,
                    *w,
                    table.alignments.get(c).copied(),
                    style,
                )));
                spans.push(Span::styled(
                    if c + 1 == cols { " │" } else { " │ " },
                    rule_style,
                ));
            }
            out.push(Line::from(spans));
        }
        // Header separator rule after the last head row.
        if r + 1 == table.head_rows {
            out.push(Line::from(Span::styled(
                table_rule(&col_w, '├', '┼', '┤'),
                rule_style,
            )));
        }
    }
    out.push(Line::from(Span::styled(
        table_rule(&col_w, '└', '┴', '┘'),
        rule_style,
    )));
    out
}

/// Pick content widths per column. Natural widths (widest cell) are used when
/// they fit; otherwise the widest columns are capped to a common limit so the
/// narrow ones keep their natural width. A column never shrinks below
/// `min(natural, 6)` (and never below its widest single character), so when even
/// those do not fit the table simply overflows.
fn table_column_widths(table: &Table, cols: usize, width: usize) -> Vec<usize> {
    const MIN_COL: usize = 6;
    let mut natural = vec![0usize; cols];
    let mut widest_char = vec![1usize; cols];
    for row in &table.rows {
        for (i, cell) in row.iter().enumerate() {
            let cell = trim_cell(cell);
            natural[i] = natural[i].max(cell_width(cell));
            for (ch, _) in cell {
                widest_char[i] = widest_char[i].max(char_width(*ch));
            }
        }
    }
    // Row layout: "│ " + cells joined by " │ " + " │" => 3 * cols + 1 border cells.
    let avail = width.saturating_sub(3 * cols + 1);
    if natural.iter().sum::<usize>() <= avail {
        return natural.into_iter().map(|w| w.max(1)).collect();
    }
    let floor: Vec<usize> = (0..cols)
        .map(|i| {
            natural[i]
                .min(MIN_COL)
                .max(widest_char[i])
                .min(natural[i].max(1))
        })
        .collect();
    let capped = |cap: usize| -> Vec<usize> {
        (0..cols)
            .map(|i| natural[i].min(cap).max(floor[i]))
            .collect()
    };
    let mut cap = natural.iter().copied().max().unwrap_or(1);
    while cap > 0 && capped(cap).iter().sum::<usize>() > avail {
        cap -= 1;
    }
    let mut widths = capped(cap);
    // Hand out leftover cells (cap steps are whole columns) to still-wrapped
    // columns, widest natural first.
    let mut spare = avail.saturating_sub(widths.iter().sum());
    let mut order: Vec<usize> = (0..cols).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(natural[i]));
    for i in order {
        if spare == 0 {
            break;
        }
        if widths[i] < natural[i] {
            widths[i] += 1;
            spare -= 1;
        }
    }
    widths
}

/// Wrap one table cell to `width` display cells with the same rules as prose
/// (see [`wrap_units`]). Always returns at least one (possibly empty) line.
fn wrap_cell(cell: &[Unit], width: usize) -> Vec<TableCell> {
    let lines = wrap_units(trim_cell(cell), width);
    if lines.is_empty() {
        vec![Vec::new()]
    } else {
        lines
    }
}

fn cell_width(cell: &[Unit]) -> usize {
    display_width(&cell.iter().map(|(ch, _)| *ch).collect::<String>())
}

fn trim_cell(cell: &[Unit]) -> &[Unit] {
    let start = cell
        .iter()
        .position(|(ch, _)| !ch.is_whitespace())
        .unwrap_or(cell.len());
    let end = cell
        .iter()
        .rposition(|(ch, _)| !ch.is_whitespace())
        .map_or(start, |index| index + 1);
    &cell[start..end]
}

fn table_rule(widths: &[usize], left: char, middle: char, right: char) -> String {
    let mut rule = String::new();
    rule.push(left);
    for (index, width) in widths.iter().enumerate() {
        rule.push_str(&"─".repeat(width.saturating_add(2)));
        rule.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    rule
}

/// Pad a wrapped cell while retaining inline styles and link targets.
fn pad_cell(
    cell: &[Unit],
    width: usize,
    align: Option<pulldown_cmark::Alignment>,
    base: Style,
) -> TableCell {
    use pulldown_cmark::Alignment;
    let used = cell_width(cell);
    let pad = width.saturating_sub(used);
    let left = match align {
        Some(Alignment::Right) => pad,
        Some(Alignment::Center) => pad / 2,
        _ => 0,
    };
    let mut out = vec![(' ', base); left];
    out.extend(cell.iter().map(|(ch, style)| (*ch, base.patch(*style))));
    out.extend(std::iter::repeat_n((' ', base), pad - left));
    out
}

/// Coalesce consecutive same-style units into spans.
fn coalesce(units: Vec<Unit>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let mut cur_style: Option<Style> = None;
    for (ch, style) in units {
        match cur_style {
            Some(s) if s == style => buf.push(ch),
            _ => {
                if let Some(s) = cur_style {
                    spans.push(Span::styled(std::mem::take(&mut buf), s));
                }
                cur_style = Some(style);
                buf.push(ch);
            }
        }
    }
    if let Some(s) = cur_style {
        spans.push(Span::styled(buf, s));
    }
    spans
}

/// Characters that must not start a line (closing punctuation, sentence
/// marks), CJK and ASCII.
fn no_line_start(c: char) -> bool {
    matches!(
        c,
        '。' | '，'
            | '、'
            | '；'
            | '：'
            | '！'
            | '？'
            | '）'
            | '」'
            | '』'
            | '”'
            | '’'
            | '》'
            | '〉'
            | '】'
            | '〕'
            | '〗'
            | '］'
            | '｝'
            | '…'
            | '‥'
            | '·'
            | '・'
            | '％'
            | ','
            | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | ')'
            | ']'
            | '}'
            | '%'
    )
}

/// Characters that must not end a line (opening punctuation).
fn no_line_end(c: char) -> bool {
    matches!(
        c,
        '（' | '「'
            | '『'
            | '“'
            | '‘'
            | '《'
            | '〈'
            | '【'
            | '〔'
            | '〖'
            | '［'
            | '｛'
            | '('
            | '['
            | '{'
    )
}

/// Whether a line may break between two adjacent non-space characters: only
/// next to a wide (CJK) character, never before a combining mark, and never
/// where line-start / line-end punctuation rules forbid it.
fn can_break_between(a: char, b: char) -> bool {
    let (wa, wb) = (char_width(a), char_width(b));
    wb > 0 && (wa >= 2 || wb >= 2) && !no_line_start(b) && !no_line_end(a)
}

/// An unbreakable run of characters plus the whitespace (if any) before it.
struct Atom {
    units: Vec<Unit>,
    width: usize,
    gap: Option<Style>,
}

/// Cut `units` into atoms: runs of non-space characters, split between CJK
/// characters wherever [`can_break_between`] allows. A soft line break
/// (newline) between two wide characters leaves no gap at all.
fn split_atoms(units: &[Unit]) -> Vec<Atom> {
    let mut atoms: Vec<Atom> = Vec::new();
    let mut cur: Vec<Unit> = Vec::new();
    let mut cur_gap: Option<Style> = None;
    let mut gap: Option<Style> = None;
    let mut gap_is_soft = false;
    let mut last: Option<char> = None;
    let flush = |cur: &mut Vec<Unit>, cur_gap: &mut Option<Style>, atoms: &mut Vec<Atom>| {
        if !cur.is_empty() {
            let units = std::mem::take(cur);
            let width = units.iter().map(|(c, _)| char_width(*c)).sum();
            atoms.push(Atom {
                units,
                width,
                gap: cur_gap.take(),
            });
        }
    };
    for &(ch, style) in units {
        if ch.is_whitespace() {
            flush(&mut cur, &mut cur_gap, &mut atoms);
            gap_is_soft = if gap.is_none() { ch == '\n' } else { false };
            gap.get_or_insert(style);
            continue;
        }
        if let Some(gap_style) = gap.take() {
            let both_wide = last.is_some_and(|l| char_width(l) >= 2) && char_width(ch) >= 2;
            cur_gap = (!(gap_is_soft && both_wide)).then_some(gap_style);
        } else if let Some(prev) = last
            && !cur.is_empty()
            && can_break_between(prev, ch)
        {
            flush(&mut cur, &mut cur_gap, &mut atoms);
        }
        cur.push((ch, style));
        last = Some(ch);
    }
    flush(&mut cur, &mut cur_gap, &mut atoms);
    atoms
}

/// Greedy, display-width-aware wrap over styled units. Lines break at spaces
/// and between CJK characters (with basic kinsoku rules); words, URLs and
/// inline-code tokens stay whole unless wider than `width`, in which case they
/// are hard-broken. Every unit keeps its own style across wraps.
fn wrap_units(units: &[Unit], width: usize) -> Vec<Vec<Unit>> {
    let width = width.max(1);
    let atoms = split_atoms(units);
    if atoms.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<Vec<Unit>> = Vec::new();
    let mut line: Vec<Unit> = Vec::new();
    let mut line_w = 0usize;

    for atom in atoms {
        let gap = usize::from(!line.is_empty() && atom.gap.is_some());
        if !line.is_empty() && line_w + gap + atom.width <= width {
            if let (1, Some(style)) = (gap, atom.gap) {
                line.push((' ', style));
            }
            line.extend(atom.units);
            line_w += gap + atom.width;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        if atom.width <= width {
            line = atom.units;
            line_w = atom.width;
            continue;
        }
        let mut chunks = hard_break(atom.units, width);
        line = chunks.pop().unwrap_or_default();
        line_w = line.iter().map(|(c, _)| char_width(*c)).sum();
        lines.extend(chunks);
    }
    lines.push(line);
    lines
}

/// Cut `units` into rows of at most `width` display cells, breaking anywhere.
/// Always returns at least one (possibly empty) row.
fn hard_break(units: Vec<Unit>, width: usize) -> Vec<Vec<Unit>> {
    let width = width.max(1);
    let mut rows: Vec<Vec<Unit>> = Vec::new();
    let mut row: Vec<Unit> = Vec::new();
    let mut row_w = 0usize;
    for unit in units {
        let cw = char_width(unit.0);
        if row_w + cw > width && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
            row_w = 0;
        }
        row.push(unit);
        row_w += cw;
    }
    rows.push(row);
    rows
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Width-aware wrap for plain (unstyled) text, preserving blank lines. Shared
/// with the non-markdown chat cells. It uses the same CJK-aware wrapper as
/// markdown prose: lines break at spaces and between CJK characters (with
/// line-start / line-end punctuation rules), and English words and URLs stay
/// whole unless longer than the line. A raw line that already fits is kept
/// verbatim (apart from trailing whitespace), so aligned output survives; the
/// leading indentation of a wrapped line stays on its first row.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let trimmed = raw.trim_end();
        if trimmed.is_empty() {
            out.push(String::new());
        } else if display_width(trimmed) <= width {
            out.push(trimmed.to_string());
        } else {
            wrap_plain_line(trimmed, width, &mut out);
        }
    }
    out
}

fn wrap_plain_line(line: &str, width: usize, out: &mut Vec<String>) {
    let content = line.trim_start();
    let indent = &line[..line.len() - content.len()];
    let units: Vec<Unit> = content.chars().map(|c| (c, Style::default())).collect();
    let mut rows = wrap_units(&units, width);
    let indent_width = display_width(indent);
    if indent_width > 0 && indent_width < width {
        let first_width: usize = rows
            .first()
            .map_or(0, |r| r.iter().map(|(c, _)| char_width(*c)).sum());
        if indent_width + first_width > width {
            rows = wrap_units(&units, width - indent_width);
        }
        if let Some(first) = rows.first_mut() {
            first.splice(0..0, indent.chars().map(|c| (c, Style::default())));
        }
    }
    out.extend(
        rows.into_iter()
            .map(|row| row.into_iter().map(|(c, _)| c).collect::<String>()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn wrap_breaks_words_and_long_tokens() {
        assert_eq!(
            wrap("hello world", 5),
            vec!["hello".to_string(), "world".to_string()]
        );
        assert_eq!(
            wrap("abcdefgh", 3),
            vec!["abc".to_string(), "def".to_string(), "gh".to_string()]
        );
    }

    #[test]
    fn wrap_is_unicode_width_aware() {
        // Wide (CJK) glyphs count as width 2, so two of them fill a width-4 line.
        let lines = wrap("你好世界", 4);
        assert_eq!(lines, vec!["你好".to_string(), "世界".to_string()]);
    }

    #[test]
    fn heading_is_detected_and_stripped() {
        let lines = render("## Title here", 40);
        assert_eq!(texts(&lines), vec!["Title here".to_string()]);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn bullets_get_a_marker() {
        let lines = render("- first\n- second", 40);
        let t = texts(&lines);
        assert_eq!(t, vec!["• first".to_string(), "• second".to_string()]);
    }

    #[test]
    fn ordered_list_numbers_items() {
        let lines = render("1. one\n2. two", 40);
        let t = texts(&lines);
        assert_eq!(t, vec!["1. one".to_string(), "2. two".to_string()]);
    }

    #[test]
    fn nested_bullets_indent() {
        let lines = render("- top\n  - child", 40);
        let t = texts(&lines);
        assert_eq!(t, vec!["• top".to_string(), "  • child".to_string()]);
    }

    #[test]
    fn fenced_code_block_strips_fences_and_styles_body() {
        let lines = render("text\n```rust\nlet x = 1;\n```\nmore", 40);
        let t = texts(&lines);
        assert!(t.iter().any(|l| l.contains("let x = 1;")));
        assert!(!t.iter().any(|l| l.contains("```")));
    }

    #[test]
    fn inline_bold_and_code_split_into_spans() {
        let lines = render("a **bold** and `code` end", 80);
        let spans = &lines[0].spans;
        assert!(
            spans
                .iter()
                .any(|s| s.content.as_ref() == "bold"
                    && s.style.add_modifier.contains(Modifier::BOLD))
        );
        assert!(spans.iter().any(|s| s.content.as_ref() == "code"));
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "a bold and code end");
    }

    #[test]
    fn loose_strong_with_trailing_space_is_rendered_without_markers() {
        let lines = render("- **macOS: **Download the installer", 80);
        let spans = &lines[0].spans;
        assert!(spans.iter().any(|span| span.content.as_ref() == "macOS:"
            && span.style.add_modifier.contains(Modifier::BOLD)));
        let joined: String = spans.iter().map(|span| span.content.as_ref()).collect();
        assert_eq!(joined, "• macOS: Download the installer");
    }

    #[test]
    fn plain_paragraph_wraps() {
        let lines = render("one two three four", 8);
        assert!(lines.len() >= 2);
    }

    #[test]
    fn consecutive_paragraphs_are_separated_by_one_blank_line() {
        let lines = render("first paragraph\n\nsecond paragraph", 80);
        let text: Vec<_> = lines.iter().map(Line::to_string).collect();
        assert_eq!(text, ["first paragraph", "", "second paragraph"]);
    }

    #[test]
    fn table_cells_keep_inline_styles_without_adding_body_row_rules() {
        let lines = render(
            "| 方法 | 数值 |\n|---|---:|\n| baseline | 7.5 |\n| candidate | **9.5** |",
            80,
        );
        assert!(
            find_span(&lines, "9.5")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        for plain in ["candidate", "baseline", "7.5"] {
            assert!(
                !find_span(&lines, plain)
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD)
            );
        }
        let text = texts(&lines);
        assert_eq!(
            text.iter().filter(|line| line.starts_with('├')).count(),
            1,
            "only the existing header rule"
        );
        assert_eq!(text.len(), 6);
    }

    #[test]
    fn wrapped_table_cells_preserve_bold_cjk_and_number_styles() {
        let lines = render(
            "| 方法 | 数值 |\n|---|---:|\n| **加粗内容跨行仍然保留** | **12345±0.67** |\n| plain | 8.0 |",
            24,
        );
        let body: Vec<_> = lines.iter().skip(3).flat_map(|line| &line.spans).collect();
        let bold: String = body
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::BOLD))
            .map(|span| span.content.as_ref())
            .collect();
        // The two cells interleave physically; inspect their character sets separately.
        let cjk: String = bold
            .chars()
            .filter(|ch| !ch.is_ascii() && *ch != '±')
            .collect();
        let number: String = bold
            .chars()
            .filter(|ch| ch.is_ascii_digit() || matches!(ch, '±' | '.'))
            .collect();
        assert_eq!(cjk, "加粗内容跨行仍然保留");
        assert_eq!(number, "12345±0.67");
        assert!(
            !find_span(&lines, "plain")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(texts(&lines).iter().all(|line| display_width(line) <= 24));
    }

    #[test]
    fn table_inline_code_emphasis_and_links_retain_their_styles() {
        let lines = render(
            "| a | b |\n|---|---|\n| **best** then plain | *italic* ~~removed~~ **`code`** [link](https://example.com) |",
            100,
        );
        assert!(
            find_span(&lines, "best")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !find_span(&lines, "then plain")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            find_span(&lines, "italic")
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert!(
            find_span(&lines, "removed")
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert!(
            find_span(&lines, "code")
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(find_span(&lines, "code").style.bg, None);
        assert_eq!(
            hyperlink::url_of_style(find_span(&lines, "link").style).as_deref(),
            Some("https://example.com")
        );
    }

    #[test]
    fn table_renders_with_separators() {
        let lines = render("| a | b |\n|---|---|\n| 1 | 2 |", 40);
        let t = texts(&lines);
        assert!(t.first().is_some_and(|line| line.starts_with('┌')));
        assert!(t.iter().any(|l| l.contains('a') && l.contains('b')));
        assert!(t.iter().any(|l| l.contains('│')));
        assert!(t.iter().any(|l| l.contains('┼')));
        assert!(t.last().is_some_and(|line| line.ends_with('┘')));
    }

    const MODEL_TABLE: &str = "| 项目 | 内容 |\n|---|---|\n| 内置子代理工具 | gpt-6.1-sol、gpt-6-astra、gpt-6.1-terra、gpt-6-sol、gpt-6-terra、gpt-6.1-astra |\n| 默认模型配置项目 | 继承主代理 |";

    fn line_widths(t: &[String]) -> Vec<usize> {
        t.iter().map(|l| display_width(l)).collect()
    }

    #[test]
    fn wide_table_keeps_natural_widths_without_truncation() {
        let t = texts(&render(MODEL_TABLE, 110));
        assert!(t.iter().all(|l| !l.contains('…')), "{t:#?}");
        assert!(
            t.iter().any(|l| l.contains(
                "gpt-6.1-sol、gpt-6-astra、gpt-6.1-terra、gpt-6-sol、gpt-6-terra、gpt-6.1-astra"
            )),
            "{t:#?}"
        );
        // Column 1 keeps its natural width (widest cell, 16 cells) and the
        // table is one physical line per row: rule + head + rule + 2 rows + rule.
        assert_eq!(t.len(), 6, "{t:#?}");
        let w = line_widths(&t);
        assert!(w.iter().all(|x| *x == w[0]), "{w:?}");
        assert!(t[1].starts_with("│ 项目"), "{t:#?}");
        assert!(
            t[0].starts_with(&format!("┌{}┬", "─".repeat(16 + 2))),
            "{t:#?}"
        );
    }

    #[test]
    fn narrow_table_wraps_cells_with_aligned_borders() {
        let t = texts(&render(MODEL_TABLE, 60));
        assert!(t.iter().all(|l| !l.contains('…')), "{t:#?}");
        assert!(t.len() > 6, "{t:#?}");
        let w = line_widths(&t);
        assert!(w.iter().all(|x| *x <= 60 && *x == w[0]), "{w:?}\n{t:#?}");
        for line in &t[1..t.len() - 1] {
            if !line.starts_with('├') {
                assert!(line.starts_with('│') && line.ends_with('│'), "{line}");
            }
        }
        // All of the long cell's text survives across the wrapped lines.
        let joined: String = t
            .iter()
            .filter(|l| l.starts_with('│'))
            .map(|l| l.split('│').nth(2).unwrap_or("").trim().to_string())
            .collect();
        assert!(
            joined.contains(
                "gpt-6.1-sol、gpt-6-astra、gpt-6.1-terra、gpt-6-sol、gpt-6-terra、gpt-6.1-astra"
            ),
            "{joined}"
        );
    }

    #[test]
    fn fitting_table_uses_natural_widths() {
        let t = texts(&render("| a | bbb |\n|:-:|--:|\n| 1 | 2 |", 40));
        assert_eq!(
            t,
            [
                "┌───┬─────┐",
                "│ a │ bbb │",
                "├───┼─────┤",
                "│ 1 │   2 │",
                "└───┴─────┘",
            ]
        );
    }

    #[test]
    fn inline_code_stays_inside_table_cells() {
        let lines = render(
            "| access | claude | codex |\n|---|---|---|\n| `read-only` | `plan` | `-s read-only` |\n| `edit` | `acceptEdits` | `-s workspace-write` |",
            80,
        );
        let t = texts(&lines);

        assert_eq!(
            t.len(),
            6,
            "table code must not leak into a trailing paragraph"
        );
        assert!(t.iter().any(|line| {
            line.contains("read-only") && line.contains("plan") && line.contains("-s read-only")
        }));
        assert!(t.iter().any(|line| {
            line.contains("edit")
                && line.contains("acceptEdits")
                && line.contains("-s workspace-write")
        }));
        assert!(lines.iter().all(|line| {
            line.spans
                .iter()
                .all(|span| span.style.fg != Some(theme::warning_color()))
        }));
    }

    #[test]
    fn horizontal_rule_spans_the_width_in_the_dim_style() {
        let lines = render("before\n\n---\n\nafter", 80);
        let t = texts(&lines);
        assert_eq!(
            t,
            vec![
                "before".to_string(),
                "".to_string(),
                "─".repeat(80),
                "".to_string(),
                "after".to_string()
            ]
        );
    }

    fn plain(lines: &[Line<'static>]) -> String {
        texts(lines).join("\n")
    }

    #[test]
    fn blocks_are_separated_by_exactly_one_blank_line() {
        let md = "# Title\n\npara one\n\n- a\n- b\n\n```\ncode\n```\n\n> quote\n\n---\n\n| x | y |\n|---|---|\n| 1 | 2 |\n\nend";
        let t = texts(&render(md, 40));
        assert_eq!(t.first().map(String::as_str), Some("Title"));
        assert_eq!(t.last().map(String::as_str), Some("end"));
        assert!(
            t.windows(2).all(|w| !(w[0].is_empty() && w[1].is_empty())),
            "{t:#?}"
        );
        let expect = [
            "Title",
            "",
            "para one",
            "",
            "• a",
            "• b",
            "",
            "  code",
            "",
            "▎ quote",
            "",
            "────────────────────────────────────────",
            "",
            "┌───┬───┐",
        ];
        assert_eq!(t[..expect.len()], expect, "{t:#?}");
        assert_eq!(t[t.len() - 2], "", "{t:#?}");
    }

    #[test]
    fn tight_list_items_stay_adjacent_and_nested_lists_do_not_get_gaps() {
        let t = texts(&render("intro\n\n- a\n  - b\n- c\n\ntail", 40));
        assert_eq!(t, ["intro", "", "• a", "  • b", "• c", "", "tail"]);
    }

    #[test]
    fn quote_paragraphs_keep_a_bare_bar_between_them() {
        let t = texts(&render("> one\n>\n> two", 40));
        assert_eq!(t, ["▎ one", "▎", "▎ two"]);
    }

    #[test]
    fn cjk_bold_next_to_fullwidth_punctuation_is_recognised() {
        let md = "我的判断是：**最值得关注的，是找人。**它瞄准注意力。资源。**一个任务可以跨越。**官方模型";
        let lines = render(md, 200);
        let joined = plain(&lines);
        assert!(!joined.contains('*'), "{joined}");
        assert!(!joined.contains('\u{200b}'), "{joined}");
        let bold: Vec<&str> = lines[0]
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(bold, ["最值得关注的，是找人。", "一个任务可以跨越。"]);
        // Bold that opens against an opening quote after a CJK letter.
        let lines = render("这是**“重点”**，别忘了", 80);
        let joined = plain(&lines);
        assert_eq!(joined, "这是“重点”，别忘了");
        assert!(lines[0].spans.iter().any(
            |s| s.content.as_ref() == "“重点”" && s.style.add_modifier.contains(Modifier::BOLD)
        ));
    }

    #[test]
    fn cjk_emphasis_fix_leaves_english_code_and_plain_asterisks_alone() {
        let lines = render("a **bold** b and *it* and 2*3*4 and snake_case_name", 80);
        // `2*3*4` is ordinary CommonMark emphasis, exactly as before.
        assert_eq!(plain(&lines), "a bold b and it and 234 and snake_case_name");
        assert!(matches!(
            fix_cjk_emphasis("2*3*4，**x** 和 **y**。后"),
            Cow::Borrowed(_)
        ));
        // Inside inline code and fenced code nothing is inserted.
        let md = "`找人。**它` and\n\n```\n找人。**它\n```";
        assert!(matches!(fix_cjk_emphasis(md), Cow::Borrowed(_)));
        let t = plain(&render(md, 80));
        assert!(t.contains("找人。**它"), "{t}");
        assert!(!t.contains('\u{200b}'));
        // Escaped asterisks stay literal.
        let t = plain(&render("好\\*\\*。**它", 80));
        assert!(!t.contains('\u{200b}'));
    }

    #[test]
    fn prose_wraps_between_cjk_characters_and_at_spaces() {
        let t = texts(&render("甲乙丙丁戊己庚辛 abc def", 8));
        assert_eq!(t, ["甲乙丙丁", "戊己庚辛", "abc def"]);
        // A mixed line fills to the edge instead of stopping at the last space.
        let t = texts(&render("word 中文中文中文中文", 11));
        assert_eq!(t, ["word 中文中", "文中文中文"]);
    }

    #[test]
    fn wrap_applies_kinsoku_and_keeps_tokens_whole() {
        // Closing punctuation never starts a line; opening never ends one.
        let t = texts(&render("甲乙丙丁，戊（己庚）", 8));
        assert_eq!(t, ["甲乙丙", "丁，戊", "（己庚）"]);
        for line in &t {
            let first = line.chars().next().unwrap();
            let last = line.chars().last().unwrap();
            assert!(!no_line_start(first), "{t:?}");
            assert!(!no_line_end(last), "{t:?}");
        }
        // English words and inline code are not split while they fit.
        let t = texts(&render("中文 `cargo-build` internationalization 结束", 22));
        assert!(t.iter().any(|l| l.contains("cargo-build")), "{t:?}");
        assert!(
            t.iter().any(|l| l.contains("internationalization")),
            "{t:?}"
        );
        // Overlong tokens hard-break.
        let t = texts(&render("abcdefghij", 4));
        assert_eq!(t, ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn soft_breaks_between_cjk_lines_add_no_space() {
        let t = texts(&render("你好\n世界\nab\ncd", 40));
        assert_eq!(t, ["你好世界 ab cd"]);
    }

    #[test]
    fn wrapped_lines_keep_styles_on_the_right_characters() {
        let lines = render("前缀中文**加粗的一段文字**后缀 `code span` 尾", 10);
        let mut styled = String::new();
        for line in &lines {
            for span in &line.spans {
                if span.style.add_modifier.contains(Modifier::BOLD) {
                    styled.push_str(&span.content);
                }
            }
        }
        assert_eq!(styled, "加粗的一段文字");
        let code: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.fg == Some(theme::warning_color()))
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(code, "code span");
        let all = plain(&lines).replace('\n', "");
        assert!(all.starts_with("前缀中文加粗的一段文字后缀"), "{all}");
        assert!(lines.iter().all(|l| display_width(&l.to_string()) <= 10));
    }

    /// The link destination carried by a span's invisible marker.
    fn span_url(span: &Span<'static>) -> Option<String> {
        hyperlink::url_of_style(span.style)
    }

    #[test]
    fn link_shows_only_the_label_in_accent_with_a_hidden_target() {
        let lines = render("see [官方仓库](https://example.com/a) now", 80);
        assert_eq!(plain(&lines), "see 官方仓库 now");
        let label = find_span(&lines, "官方仓库");
        assert_eq!(label.style.fg, Some(theme::accent_color()));
        assert_eq!(span_url(label).as_deref(), Some("https://example.com/a"));
        for span in &lines[0].spans {
            assert!(
                !span.style.add_modifier.contains(Modifier::UNDERLINED),
                "{span:?}"
            );
            assert!(!span.content.contains("example.com"));
        }
        // Surrounding text is not part of the link.
        assert_eq!(span_url(find_span(&lines, "see")), None);
    }

    #[test]
    fn unsafe_link_destinations_render_as_plain_labels() {
        let lines = render(
            "[click](javascript:alert(1)) and [x](https://a.io/\u{1b}[31m)",
            80,
        );
        assert_eq!(plain(&lines).split(" and ").next(), Some("click"));
        let label = find_span(&lines, "click");
        assert_eq!(label.style.fg, None);
        assert_eq!(span_url(label), None);
    }

    #[test]
    fn autolink_and_equal_label_show_the_url_once() {
        let lines = render(
            "<https://example.com/x> and [https://a.io](https://a.io)",
            80,
        );
        assert_eq!(plain(&lines), "https://example.com/x and https://a.io");
        assert_eq!(
            span_url(find_span(&lines, "https://example.com/x")).as_deref(),
            Some("https://example.com/x")
        );
    }

    #[test]
    fn long_link_labels_wrap_with_each_segment_keeping_the_target() {
        let url = "https://example.com/target";
        let lines = render(&format!("[{}]({url})", "label".repeat(8)), 12);
        let t = texts(&lines);
        assert!(t.len() >= 4, "{t:?}");
        assert!(t.iter().all(|l| display_width(l) <= 12), "{t:?}");
        assert_eq!(t.concat(), "label".repeat(8));
        for line in &lines {
            assert_eq!(span_url(&line.spans[0]).as_deref(), Some(url));
        }
    }

    #[test]
    fn long_bare_urls_wrap_without_truncation() {
        let url = format!("https://example.com/{}", "segment/".repeat(10));
        let t = texts(&render(&url, 30));
        assert!(t.len() >= 3, "{t:?}");
        assert!(t.iter().all(|l| display_width(l) <= 30), "{t:?}");
        assert_eq!(t.concat(), url);
    }

    #[test]
    fn markdown_colours_come_from_the_theme_palette() {
        let md = "# H1\n\n### H3\n\n> quote\n\n---\n\n| a |\n|---|\n| b |\n\n[l](http://u) `c`";
        for line in render(md, 40) {
            for span in &line.spans {
                for color in [span.style.fg, span.style.bg].into_iter().flatten() {
                    assert!(
                        matches!(color, Color::Rgb(..)),
                        "non-theme colour {color:?} in {span:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn link_in_table_cell_shows_only_the_label() {
        let t = texts(&render("| a |\n|---|\n| [x](http://u.io) |", 60));
        assert!(t.iter().any(|l| l.contains("│ x ")), "{t:?}");
        assert!(t.iter().all(|l| !l.contains("u.io")), "{t:?}");
    }

    fn find_span<'a>(lines: &'a [Line<'static>], text: &str) -> &'a Span<'static> {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains(text))
            .unwrap_or_else(|| panic!("no span containing {text:?}"))
    }

    #[test]
    fn rule_is_dim() {
        let lines = render("a\n\n---\n\nb", 12);
        assert_eq!(lines[2].spans[0].content.as_ref(), "─".repeat(12));
        assert_eq!(lines[2].spans[0].style.fg, Some(theme::dim_color()));
    }

    #[test]
    fn section_headings_use_emphasis_instead_of_muted_text() {
        let lines = render(
            "# One\n\n## Two\n\n### Three\n\n#### Four\n\n##### Five\n\n###### Six",
            40,
        );
        let fg = |text: &str| find_span(&lines, text).style.fg;
        assert_eq!(fg("One"), Some(theme::accent_color()));
        assert_eq!(fg("Two"), Some(theme::emphasis_color()));
        for title in ["Three", "Four", "Five", "Six"] {
            assert_eq!(fg(title), Some(theme::emphasis_color()));
        }
        assert_ne!(fg("One"), fg("Two"));
        for text in ["One", "Two", "Three", "Four", "Five", "Six"] {
            let m = find_span(&lines, text).style.add_modifier;
            assert!(m.contains(Modifier::BOLD) && !m.contains(Modifier::UNDERLINED));
        }
    }

    #[test]
    fn table_body_uses_default_fg_and_header_is_emphasised() {
        let lines = render("| hdr |\n|---|\n| body |", 40);
        assert_eq!(find_span(&lines, "body").style.fg, None);
        let header = find_span(&lines, "hdr").style;
        assert_eq!(header.fg, Some(theme::emphasis_color()));
        assert!(header.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn long_code_lines_wrap_instead_of_being_cut() {
        let long = "x".repeat(25);
        let md = format!("```rust\nlet a = \"{long}\";\nshort\n```");
        let lines = render(&md, 12);
        let t = texts(&lines);
        assert_eq!(t[0], "  rust");
        assert_eq!(lines[0].spans[1].style.fg, Some(theme::dim_color()));
        assert!(t.iter().all(|l| display_width(l) <= 12), "{t:?}");
        let body: String = t[1..t.len() - 1]
            .iter()
            .map(|l| l.chars().skip(2).collect::<String>())
            .collect();
        assert_eq!(body, format!("let a = \"{long}\";"));
        assert!(t[2].starts_with("↪ "), "{t:?}");
        assert_eq!(t.last().unwrap(), "  short");
        // Highlighting survives: more than one distinct colour on the code.
        let colours: std::collections::HashSet<_> = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter_map(|s| s.style.fg)
            .collect();
        assert!(colours.len() > 1);
    }

    #[test]
    fn task_list_markers_render_as_checkboxes() {
        let t = texts(&render("- [ ] todo\n- [x] done", 40));
        assert_eq!(t, ["• ☐ todo", "• ☑ done"]);
    }

    #[test]
    fn footnotes_render_dim_reference_and_definition() {
        let lines = render("Claim[^1] here.\n\n[^1]: The source.\n[^2]: Another.", 40);
        let t = texts(&lines);
        assert_eq!(t[0], "Claim[1] here.");
        assert_eq!(find_span(&lines, "[1]").style.fg, Some(theme::dim_color()));
        assert_eq!(t[1], "");
        assert!(t[2].starts_with("[1] The source."), "{t:?}");
        assert!(t[3].starts_with("[2] Another."), "{t:?}");
        assert_eq!(
            find_span(&lines, "The source").style.fg,
            Some(theme::dim_color())
        );
    }

    #[test]
    fn soft_break_joins_cjk_without_space_but_english_with_space() {
        assert_eq!(texts(&render("**中文**\n接着", 40)), ["中文接着"]);
        assert_eq!(texts(&render("one\ntwo", 40)), ["one two"]);
        assert_eq!(texts(&render("中文\nabc", 40)), ["中文 abc"]);
    }

    #[test]
    fn nested_lists_align_under_the_parent_text() {
        let t = texts(&render(
            "1. one\n2. two\n   - child\n     - grand\n3. three",
            40,
        ));
        assert_eq!(
            t,
            ["1. one", "2. two", "   • child", "     • grand", "3. three"]
        );
        // Wrapped continuation lines align under the item text at every depth.
        let t = texts(&render(
            "- a\n  1. bb cc dd ee ff\n     - gg hh ii jj kk",
            12,
        ));
        assert_eq!(
            t,
            [
                "• a",
                "  1. bb cc",
                "     dd ee",
                "     ff",
                "     • gg hh",
                "       ii jj",
                "       kk"
            ]
        );
    }

    #[test]
    fn inline_code_uses_theme_foreground_and_background() {
        let lines = render("run `cargo test` now", 40);
        let span = find_span(&lines, "cargo test");
        assert_eq!(span.style.fg, Some(theme::warning_color()));
        assert_eq!(span.style.bg, Some(theme::code_bg()));
    }

    #[test]
    fn user_style_text_never_starts_a_line_with_closing_punctuation() {
        let text = "这是一段比较长的中文提示词用来检查换行规则，它不应该让逗号出现在行首。结尾";
        for width in 10..40 {
            for line in texts(&render(text, width)) {
                let first = line.chars().next().unwrap();
                assert!(!no_line_start(first), "width {width}: {line}");
            }
        }
    }

    #[test]
    fn bare_urls_and_autolinks_use_the_link_colour_once() {
        let lines = render(
            "see https://example.com/a?b=1, or <https://other.io/x>。",
            80,
        );
        assert_eq!(
            plain(&lines),
            "see https://example.com/a?b=1, or https://other.io/x。"
        );
        let bare = find_span(&lines, "https://example.com/a?b=1");
        assert_eq!(bare.content.as_ref(), "https://example.com/a?b=1");
        assert_eq!(bare.style.fg, Some(theme::accent_color()));
        assert_eq!(span_url(bare).as_deref(), Some("https://example.com/a?b=1"));
        let auto = find_span(&lines, "https://other.io/x");
        assert_eq!(auto.style.fg, Some(theme::accent_color()));
        assert!(!auto.style.add_modifier.contains(Modifier::UNDERLINED));
        // A URL followed directly by CJK text ends at the CJK character.
        let lines = render("见https://a.io/p中文", 80);
        assert_eq!(
            find_span(&lines, "https://a.io/p").content.as_ref(),
            "https://a.io/p"
        );
    }

    #[test]
    fn user_render_keeps_typed_line_breaks_but_agent_render_joins_them() {
        let text = "first line\nsecond line\n\nnew paragraph\n- one\n- two\n\n```\na\nb\n```";
        let user = texts(&render_user(text, 40));
        assert_eq!(user[0], "first line");
        assert_eq!(user[1], "second line");
        assert!(user.iter().any(|l| l == "new paragraph"), "{user:?}");
        assert!(user.iter().any(|l| l.contains("• one")), "{user:?}");
        assert!(user.iter().any(|l| l.contains("• two")), "{user:?}");
        let code: Vec<_> = user
            .iter()
            .filter(|l| l.trim() == "a" || l.trim() == "b")
            .collect();
        assert_eq!(code.len(), 2, "{user:?}");
        // CJK lines stay separate too, and the plain render is unaffected
        // (the cache must not mix the two).
        assert_eq!(texts(&render_user("你好\n世界", 40)), vec!["你好", "世界"]);
        assert_eq!(
            texts(&render("first line\nsecond line", 40)),
            vec!["first line second line"]
        );
        assert_eq!(texts(&render("你好\n世界", 40)), vec!["你好世界"]);
        assert_eq!(texts(&render_user("a\nb", 40)), vec!["a", "b"]);
    }

    #[test]
    fn user_render_keeps_line_breaks_inside_list_items_aligned() {
        let t = texts(&render_user("- one\n  two\n- three", 40));
        assert_eq!(t, vec!["• one", "  two", "• three"]);
    }

    #[test]
    fn plain_wrap_breaks_between_cjk_characters_and_follows_kinsoku() {
        let text = "这是一个很长的中文句子，包含标点。还有更多内容！";
        for width in 6..=14 {
            let lines = wrap(text, width);
            assert!(lines.iter().all(|l| display_width(l) <= width), "{lines:?}");
            assert_eq!(lines.concat(), text);
            for line in &lines {
                assert!(
                    !no_line_start(line.chars().next().unwrap()),
                    "w{width}: {lines:?}"
                );
                assert!(
                    !no_line_end(line.chars().last().unwrap()),
                    "w{width}: {lines:?}"
                );
            }
        }
    }

    #[test]
    fn plain_wrap_keeps_words_and_urls_whole() {
        assert_eq!(
            wrap("see https://example.com/a/b ok", 27),
            vec!["see https://example.com/a/b", "ok"]
        );
        assert_eq!(
            wrap("go https://example.com/a/b", 20),
            vec!["go", "https://example.com/", "a/b"]
        );
    }

    #[test]
    fn plain_wrap_preserves_blank_lines_indentation_and_fitting_lines() {
        assert_eq!(wrap("a\n\n  b", 10), vec!["a", "", "  b"]);
        assert_eq!(wrap("col1   col2", 20), vec!["col1   col2"]);
        let lines = wrap("    indented words that wrap", 14);
        assert_eq!(lines[0], "    indented");
        assert!(lines.iter().all(|l| display_width(l) <= 14), "{lines:?}");
        assert_eq!(wrap("xxxxxxxxxx", 4), vec!["xxxx", "xxxx", "xx"]);
    }
}
