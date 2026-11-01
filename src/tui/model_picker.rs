//! The inline `/model` selector. It sits above the composer (like the
//! completion popup), leaves the chat visible, and changes the model of the
//! agent being viewed. This module holds its state, the pure row/effort
//! logic, and the renderer; `AppState` owns the instance and applies it.

use std::collections::HashMap;
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::adapter::DiscoveredModel;
use crate::domain::config::DetectedBackends;
use crate::domain::team::{BackendKind, Effort, MemberId};
use crate::tui::team_builder::{CatalogView, ModelCatalog};
use crate::tui::theme;

const BACKENDS: [BackendKind; 4] = [
    BackendKind::Codex,
    BackendKind::Claude,
    BackendKind::Grok,
    BackendKind::Agy,
];

/// Most model rows shown at once.
const VISIBLE_ROWS: usize = 8;

/// Widest model-name column.
const MAX_NAME_COLUMN: usize = 30;
/// Narrowest model-name column when the row is tight.
const MIN_NAME_COLUMN: usize = 12;
/// Columns between the effort cluster / name and the trailing text.
const TRAILING_GAP: usize = 2;
/// Columns between the name column and the effort cluster.
const CLUSTER_GAP: usize = 2;
/// Terminal width below which the hint shrinks.
const NARROW_HINT_WIDTH: usize = 70;

/// Identity of a selectable row: a backend and one of its model ids (for a
/// merged effort-variant row, the shared base id). Stable while catalogs
/// load and while the level changes, unlike a position.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ModelKey {
    pub(crate) backend: BackendKind,
    pub(crate) model: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelEntry {
    pub(crate) key: ModelKey,
    pub(crate) name: String,
    /// Effort levels the model reports, lowest first (empty when none).
    pub(crate) levels: Vec<Effort>,
    pub(crate) default_effort: Option<Effort>,
    /// For a merged row of effort-suffixed siblings (`m-low`, `m-high`): each
    /// level and the model id that carries it. Empty for ordinary models.
    pub(crate) variants: Vec<(Effort, String)>,
}

impl ModelEntry {
    /// The model id this row stands for at `level`: the matching variant of a
    /// merged row, else the row's own id.
    pub(crate) fn model_id(&self, level: Option<Effort>) -> String {
        level
            .and_then(|level| self.variants.iter().find(|(effort, _)| *effort == level))
            .or_else(|| self.variants.first())
            .map_or_else(|| self.key.model.clone(), |(_, id)| id.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PickerRow {
    /// Group heading for one backend's models. Never selectable.
    Heading(BackendKind),
    Entry(ModelEntry),
    /// Discovery is still running for this backend.
    Loading(BackendKind),
    /// Discovery failed for this backend; its default entry stays selectable.
    Failed(BackendKind, String),
}

/// State of the open selector.
#[derive(Debug)]
pub(crate) struct ModelPicker {
    /// The agent whose model is being changed.
    pub(crate) target: MemberId,
    pub(crate) filter: String,
    /// Highlighted model, by identity so a catalog that finishes loading
    /// cannot move the selection.
    pub(crate) selected: Option<ModelKey>,
    /// Position used while `selected` is not among the listed rows.
    pub(crate) fallback: usize,
    /// Levels the user changed with `←`/`→`. Models absent here show their
    /// seeded level and are applied without an effort override.
    pub(crate) efforts: HashMap<ModelKey, Effort>,
    /// Why Enter did nothing.
    pub(crate) notice: Option<String>,
}

impl ModelPicker {
    pub(crate) fn new(target: MemberId, filter: String) -> Self {
        Self {
            target,
            filter,
            selected: None,
            fallback: 0,
            efforts: HashMap::new(),
            notice: None,
        }
    }
}

/// One rendered row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PickerRowView {
    /// Dim one-line group heading. Never selectable.
    Heading(String),
    Entry {
        name: String,
        /// The model's effort levels and the one in force; empty when none.
        levels: Vec<Effort>,
        level: Option<Effort>,
        trailing: String,
        selected: bool,
    },
    Note(String),
}

/// Everything the renderer needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PickerView {
    pub(crate) target: String,
    pub(crate) filter: String,
    pub(crate) rows: Vec<PickerRowView>,
    pub(crate) selected_row: Option<usize>,
    /// 1-based position of the highlighted model among all listed models.
    pub(crate) selected_position: usize,
    pub(crate) total_models: usize,
    pub(crate) notice: Option<String>,
}

fn effort_rank(effort: Effort) -> usize {
    match effort {
        Effort::Low => 0,
        Effort::Medium => 1,
        Effort::High => 2,
        Effort::Xhigh => 3,
        Effort::Max => 4,
        Effort::Ultra => 5,
    }
}

/// A model's selectable levels, lowest first. One level is no choice, so a
/// model with fewer than two shows none.
fn sorted_levels(efforts: &[Effort]) -> Vec<Effort> {
    let mut levels = efforts.to_vec();
    levels.sort_by_key(|effort| effort_rank(*effort));
    levels.dedup();
    if levels.len() < 2 {
        levels.clear();
    }
    levels
}

/// `<base>-<effort>` for a model whose only reported level is that suffix
/// (how AGY lists one model per reasoning tier).
fn variant_of(model: &DiscoveredModel) -> Option<(String, Effort)> {
    let (base, suffix) = model.id.rsplit_once('-')?;
    let effort = Effort::parse(suffix)?;
    (!base.is_empty() && model.supported_efforts == [effort]).then(|| (base.to_string(), effort))
}

/// Bases shared by at least two effort-suffixed siblings.
fn variant_bases(models: &[DiscoveredModel]) -> Vec<String> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for (base, _) in models.iter().filter_map(variant_of) {
        match counts.iter_mut().find(|(known, _)| *known == base) {
            Some((_, count)) => *count += 1,
            None => counts.push((base, 1)),
        }
    }
    counts
        .into_iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(base, _)| base)
        .collect()
}

/// The display name without its trailing ` (Low)` / ` (Medium)` tier.
fn strip_tier(name: &str, base: &str) -> String {
    if let Some(open) = name.rfind(" (")
        && let Some(tier) = name[open + 2..].strip_suffix(')')
        && Effort::parse(tier.trim()).is_some()
    {
        return name[..open].trim_end().to_string();
    }
    base.to_string()
}

/// The row a member currently sits on: its pinned model, matched by id. A
/// member with no pinned model counts as being on its effective model (the
/// CLI's own configured one, else the catalog default; see
/// `ModelCatalog::effective_model`), but only when that matches a catalog
/// entry by id. `None` when nothing matches.
pub(crate) fn current_key(
    backend: BackendKind,
    model: Option<&str>,
    catalog: &ModelCatalog,
    cwd: &Path,
) -> Option<ModelKey> {
    let model = match model {
        Some(id) => id.to_string(),
        None => {
            // Unpinned: the model the member effectively runs, which only
            // counts when it is one of the catalog's entries.
            let effective = catalog.effective_model(backend, None, None, cwd);
            let id = effective.id?;
            match catalog.view(backend, cwd) {
                CatalogView::Ready(catalog) => {
                    catalog.models.iter().find(|m| m.matches_id(&id))?;
                    id
                }
                _ => return None,
            }
        }
    };
    // A model saved under an alternate spelling (`claude-opus-5-5` for the
    // `opus` row) sits on that row.
    let model = match catalog.view(backend, cwd) {
        CatalogView::Ready(catalog) => catalog
            .models
            .iter()
            .find(|candidate| candidate.id == model)
            .or_else(|| catalog.models.iter().find(|m| m.matches_id(&model)))
            .map_or(model, |row| row.id.clone()),
        _ => model,
    };
    // A variant belongs to its merged row.
    if let CatalogView::Ready(catalog) = catalog.view(backend, cwd) {
        let bases = variant_bases(&catalog.models);
        if let Some((base, _)) = catalog
            .models
            .iter()
            .find(|candidate| candidate.id == model)
            .and_then(variant_of)
            && bases.contains(&base)
        {
            return Some(ModelKey {
                backend,
                model: base,
            });
        }
    }
    Some(ModelKey { backend, model })
}

/// Rows for every detected backend (plus the target's own), narrowed by a
/// case-insensitive `filter` over backend, model id and name. Rows group by
/// backend in Codex, Claude, Grok, Agy order, each group led by a heading; the
/// current model keeps its place in its group. A backend whose catalog is
/// loading or failed contributes only a status row, nothing selectable.
pub(crate) fn build_rows(
    detected: DetectedBackends,
    target_backend: BackendKind,
    catalog: &ModelCatalog,
    cwd: &Path,
    filter: &str,
) -> Vec<PickerRow> {
    let needle = filter.trim().to_lowercase();
    let mut rows = Vec::new();
    for backend in BACKENDS {
        if !detected.contains(backend) && target_backend != backend {
            continue;
        }
        let view = catalog.view(backend, cwd);
        let mut entries = Vec::new();
        if let CatalogView::Ready(catalog) = &view {
            let bases = variant_bases(&catalog.models);
            let mut merged: Vec<&str> = Vec::new();
            for model in &catalog.models {
                let variant = variant_of(model).filter(|(base, _)| bases.contains(base));
                let Some((base, _)) = variant else {
                    entries.push(ModelEntry {
                        key: ModelKey {
                            backend,
                            model: model.id.clone(),
                        },
                        name: model.name.clone(),
                        levels: sorted_levels(&model.supported_efforts),
                        default_effort: model.default_effort,
                        variants: Vec::new(),
                    });
                    continue;
                };
                if merged.contains(&base.as_str()) {
                    continue;
                }
                merged.push(&model.id[..base.len()]);
                let mut variants: Vec<(Effort, String)> = catalog
                    .models
                    .iter()
                    .filter_map(|sibling| {
                        let (sibling_base, effort) = variant_of(sibling)?;
                        (sibling_base == base).then(|| (effort, sibling.id.clone()))
                    })
                    .collect();
                variants.sort_by_key(|(effort, _)| effort_rank(*effort));
                entries.push(ModelEntry {
                    key: ModelKey {
                        backend,
                        model: base.clone(),
                    },
                    name: strip_tier(&model.name, &base),
                    levels: variants.iter().map(|(effort, _)| *effort).collect(),
                    default_effort: None,
                    variants,
                });
            }
        }
        let backend_matches =
            needle.is_empty() || backend.as_str().to_lowercase().contains(&needle);
        if !needle.is_empty() {
            entries.retain(|entry| {
                backend_matches
                    || entry.name.to_lowercase().contains(&needle)
                    || entry.key.model.to_lowercase().contains(&needle)
                    || entry
                        .variants
                        .iter()
                        .any(|(_, id)| id.to_lowercase().contains(&needle))
            });
        }
        let status = backend_matches.then(|| match view {
            CatalogView::Loading | CatalogView::NotLoaded => Some(PickerRow::Loading(backend)),
            CatalogView::Failed(error) => {
                Some(PickerRow::Failed(backend, theme::truncate_width(error, 60)))
            }
            CatalogView::Ready(_) => None,
        });
        let status = status.flatten();
        if entries.is_empty() && status.is_none() {
            continue;
        }
        rows.push(PickerRow::Heading(backend));
        rows.extend(entries.into_iter().map(PickerRow::Entry));
        rows.extend(status);
    }
    rows
}

/// The level a model starts on: the member's own effort when this is its
/// current model and the model supports it, else the effort the CLI's own
/// configuration sets, else the model's default, else `medium`, else its
/// lowest level. `None` for models without levels.
pub(crate) fn seed_level(
    entry: &ModelEntry,
    members_effort: Option<Effort>,
    native_effort: Option<Effort>,
) -> Option<Effort> {
    let supported = |effort: &Effort| entry.levels.contains(effort);
    members_effort
        .filter(supported)
        .or_else(|| native_effort.filter(supported))
        .or_else(|| entry.default_effort.filter(supported))
        .or_else(|| supported(&Effort::Medium).then_some(Effort::Medium))
        .or_else(|| entry.levels.first().copied())
}

/// Step `current` one place through the model's levels, wrapping around.
/// Models without levels have nothing to cycle.
pub(crate) fn cycle_level(
    entry: &ModelEntry,
    current: Option<Effort>,
    forward: bool,
) -> Option<Effort> {
    if entry.levels.is_empty() {
        return None;
    }
    let len = entry.levels.len();
    let position = current
        .and_then(|effort| entry.levels.iter().position(|level| *level == effort))
        .unwrap_or(0);
    let next = if forward {
        (position + 1) % len
    } else {
        (position + len - 1) % len
    };
    Some(entry.levels[next])
}

/// Rows of the first window: the selection stays inside, with context above.
fn window(total: usize, selected: Option<usize>) -> (usize, usize) {
    if total <= VISIBLE_ROWS {
        return (0, total);
    }
    let selected = selected.unwrap_or(0);
    let start = selected
        .saturating_sub(VISIBLE_ROWS / 2)
        .min(total - VISIBLE_ROWS);
    (start, start + VISIBLE_ROWS)
}

/// Height of the selector for `view`: header, search line, rows, an optional
/// scroll row, optional notice, and hint.
pub(crate) fn picker_height(view: &PickerView) -> u16 {
    let rows = view.rows.len().clamp(1, VISIBLE_ROWS);
    let scroll = usize::from(view.rows.len() > VISIBLE_ROWS);
    (1 + 1 + rows + scroll + 1 + usize::from(view.notice.is_some())) as u16
}

fn hint(width: usize) -> &'static str {
    if width < NARROW_HINT_WIDTH {
        " Enter select · Esc close"
    } else {
        " ↑/↓ model · ←/→ effort · Enter select · Esc close"
    }
}

pub(crate) fn render_picker(frame: &mut Frame<'_>, area: Rect, view: &PickerView) {
    if area.height < 4 || area.width == 0 {
        return;
    }
    let width = area.width as usize;
    let header = Rect::new(area.x, area.y, area.width, 1);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  Model for ", theme::muted()),
            Span::styled(view.target.clone(), theme::accent_bold()),
        ])),
        header,
    );

    // Search: one line, `Search: <query>` with a dim placeholder.
    let label = "  Search: ";
    let search_area = Rect::new(area.x, area.y + 1, area.width, 1);
    let shown = if view.filter.is_empty() {
        Span::styled(
            "type to filter models",
            theme::muted().add_modifier(Modifier::DIM),
        )
    } else {
        Span::styled(view.filter.clone(), theme::text())
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(label, theme::muted()), shown])),
        search_area,
    );
    let cursor = theme::display_width(label) + theme::display_width(&view.filter);
    frame.set_cursor_position((
        area.x + (cursor as u16).min(area.width.saturating_sub(1)),
        search_area.y,
    ));

    let mut lines = Vec::new();
    if view.rows.is_empty() {
        lines.push(Line::styled("  No matching models", theme::muted()));
    }
    let (start, end) = window(view.rows.len(), view.selected_row);
    // Plan the columns from every listed row, not just the ones in the
    // window, so the squares stay put while the list scrolls.
    let layout = plan_layout(&view.rows, width);
    lines.extend(row_lines(&view.rows[start..end], width, &layout));
    if view.rows.len() > VISIBLE_ROWS {
        lines.push(Line::styled(
            format!("  ({}/{})", view.selected_position, view.total_models),
            theme::muted(),
        ));
    }
    if let Some(notice) = &view.notice {
        lines.push(Line::from(Span::styled(
            format!(" {notice}"),
            theme::warning(),
        )));
    }
    lines.push(Line::from(Span::styled(hint(width), theme::muted())));
    let body = Rect::new(area.x, area.y + 2, area.width, area.height - 2);
    frame.render_widget(Paragraph::new(lines), body);
}

/// How much of the effort cluster fits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Cluster {
    /// `← ■■□□ → medium`
    Full,
    /// `← ■■□□ →`
    NoLabel,
    /// Names only.
    Hidden,
}

/// Column plan shared by every row of the filtered list, so the cluster never
/// shifts as rows scroll or levels change.
struct EffortLayout {
    name_col: usize,
    gap: usize,
    slots: usize,
    cluster: Cluster,
}

fn cluster_width(slots: usize, label_width: usize, cluster: Cluster) -> usize {
    match cluster {
        Cluster::Full => slots + 5 + label_width,
        Cluster::NoLabel => slots + 4,
        Cluster::Hidden => 0,
    }
}

fn plan_layout(rows: &[PickerRowView], width: usize) -> EffortLayout {
    let entries: Vec<_> = rows
        .iter()
        .filter_map(|row| match row {
            PickerRowView::Entry {
                name,
                levels,
                level: _,
                trailing,
                ..
            } => Some((name, levels, trailing)),
            PickerRowView::Note(_) | PickerRowView::Heading(_) => None,
        })
        .collect();
    let trailing_width = entries
        .iter()
        .map(|(_, _, trailing)| theme::display_width(trailing))
        .max()
        .unwrap_or(0);
    let slots = entries
        .iter()
        .map(|(_, levels, _)| levels.len())
        .max()
        .unwrap_or(0);
    let label_width = entries
        .iter()
        .flat_map(|(_, levels, _)| levels.iter())
        .map(|level| level.as_str().len())
        .max()
        .unwrap_or(0);
    let widest_reasoning_name = entries
        .iter()
        .filter(|(_, levels, _)| !levels.is_empty())
        .map(|(name, _, _)| theme::display_width(name))
        .max();
    let widest_name = entries
        .iter()
        .map(|(name, _, _)| theme::display_width(name))
        .max()
        .unwrap_or(0);

    // Columns left for the name column, gap and cluster: the marker cell takes
    // two, and the trailing zone keeps a gap of its own.
    let available = width.saturating_sub(2 + TRAILING_GAP + trailing_width);
    let Some(name_width) = widest_reasoning_name else {
        return EffortLayout {
            name_col: widest_name.min(available),
            gap: 0,
            slots,
            cluster: Cluster::Hidden,
        };
    };
    let name_col = name_width.min(MAX_NAME_COLUMN);
    for cluster in [Cluster::Full, Cluster::NoLabel] {
        let cluster_w = cluster_width(slots, label_width, cluster);
        // Shrink the name column, but not below its floor, to fit the cluster.
        let room = available.saturating_sub(cluster_w + 1);
        let name_col = if room >= name_col {
            name_col
        } else if room >= MIN_NAME_COLUMN.min(name_col) {
            room
        } else {
            continue;
        };
        let slack = available.saturating_sub(name_col + cluster_w);
        // A fixed small gap after the name column, however wide the terminal.
        let gap = CLUSTER_GAP.clamp(1, slack.max(1));
        return EffortLayout {
            name_col,
            gap,
            slots,
            cluster,
        };
    }
    EffortLayout {
        name_col: widest_name.min(available),
        gap: 0,
        slots,
        cluster: Cluster::Hidden,
    }
}

fn row_lines(rows: &[PickerRowView], width: usize, layout: &EffortLayout) -> Vec<Line<'static>> {
    rows.iter()
        .map(|row| match row {
            // Headings and status rows sit in the name column, under the
            // marker cell.
            PickerRowView::Heading(text) => Line::from(vec![
                Span::raw("  "),
                Span::styled(text.clone(), theme::muted().add_modifier(Modifier::DIM)),
            ]),
            PickerRowView::Note(text) => Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    theme::truncate_width(text, width.saturating_sub(2).max(1)),
                    theme::muted(),
                ),
            ]),
            PickerRowView::Entry {
                name,
                levels,
                level,
                trailing,
                selected,
            } => entry_line(name, levels, *level, trailing, *selected, width, layout),
        })
        .collect()
}

fn entry_line(
    name: &str,
    levels: &[Effort],
    level: Option<Effort>,
    trailing: &str,
    selected: bool,
    width: usize,
    layout: &EffortLayout,
) -> Line<'static> {
    // A soft band across the whole selected row.
    let band = theme::chat_selection();
    let styled = |style: Style| {
        if selected {
            style.bg(band.bg.unwrap_or_default())
        } else {
            style
        }
    };
    let dim = styled(theme::muted().add_modifier(Modifier::DIM));
    let muted = styled(theme::muted());
    let mut spans = vec![Span::styled(
        if selected { "› " } else { "  " },
        styled(if selected {
            theme::accent_bold()
        } else {
            theme::muted()
        }),
    )];

    let name_style = if selected {
        band.add_modifier(Modifier::BOLD)
    } else {
        theme::text()
    };
    // Only a model with effort levels is squeezed into the name column. A
    // level-less name shows in full, cut only to the room before the trailing
    // text.
    let trailing_w = theme::display_width(trailing);
    let clustered = layout.cluster != Cluster::Hidden && !levels.is_empty() && level.is_some();
    let name_limit = if clustered {
        layout.name_col.max(1)
    } else {
        width.saturating_sub(2 + TRAILING_GAP + trailing_w).max(1)
    };
    let shown = theme::clip_width(name, name_limit);
    let pad = if clustered {
        name_limit.saturating_sub(theme::display_width(&shown))
    } else {
        0
    };
    let shown_width = theme::display_width(&shown);
    spans.push(Span::styled(shown, name_style));
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), styled(Style::default())));
    }

    if clustered {
        spans.push(Span::styled(
            " ".repeat(layout.gap),
            styled(Style::default()),
        ));
        if let Some(level) = level {
            let filled = levels
                .iter()
                .position(|candidate| *candidate == level)
                .map_or(0, |index| index + 1);
            spans.push(Span::styled(if selected { "←" } else { " " }, dim));
            spans.push(Span::styled(" ", styled(Style::default())));
            spans.push(Span::styled(
                "■".repeat(filled),
                styled(if selected {
                    theme::accent()
                } else {
                    theme::muted()
                }),
            ));
            spans.push(Span::styled("□".repeat(levels.len() - filled), dim));
            let spare = layout.slots - levels.len();
            spans.push(Span::styled(
                " ".repeat(spare + 1),
                styled(Style::default()),
            ));
            spans.push(Span::styled(if selected { "→" } else { " " }, dim));
            if layout.cluster == Cluster::Full {
                let label = level.as_str();
                spans.push(Span::styled(" ", styled(Style::default())));
                spans.push(Span::styled(label.to_string(), muted));
            }
        }
    }

    // The marker follows what the row has: two columns after the effort level,
    // or after the name column for a row without levels. The rest of the row
    // is filled so the selection band spans it.
    if !trailing.is_empty() {
        let behind_name = if clustered {
            0
        } else {
            layout.name_col.saturating_sub(shown_width)
        };
        let gap = if clustered {
            TRAILING_GAP
        } else {
            behind_name + TRAILING_GAP
        };
        spans.push(Span::styled(" ".repeat(gap), styled(Style::default())));
        spans.push(Span::styled(trailing.to_string(), muted));
    }
    let used: usize = spans
        .iter()
        .map(|span| theme::display_width(&span.content))
        .sum();
    spans.push(Span::styled(
        " ".repeat(width.saturating_sub(used)),
        styled(Style::default()),
    ));
    Line::from(spans)
}
