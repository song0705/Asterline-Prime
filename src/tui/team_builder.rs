//! Building blocks of the `/team` editor and the mode panel: the cached model
//! catalog, the backend/model pickers, the single-line edit box, and the
//! member field helpers. (Asterline no longer has a startup builder; it begins
//! with one default `main` member.)

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::adapter::{DiscoveredCatalog, DiscoveredModel};
use crate::domain::config::{DetectedBackends, default_member};
use crate::domain::team::{
    BackendKind, Effort, PermissionMode, SandboxPolicy, SessionPolicy, TeamMember,
    normalize_member_id as normalize_domain_member_id,
};
use crate::tui::theme;

#[derive(Debug)]
enum ModelLoad {
    Loading(Receiver<Result<DiscoveredCatalog, String>>),
    Ready(Result<DiscoveredCatalog, String>),
}

#[derive(Debug, Default)]
pub(crate) struct ModelCatalog {
    loads: HashMap<(BackendKind, PathBuf), ModelLoad>,
    frozen: bool,
}

/// The model and effort a member effectively runs; see
/// [`ModelCatalog::effective_model`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EffectiveModel {
    pub(crate) id: Option<String>,
    /// The catalog's display name for `id`, when it lists that id.
    pub(crate) name: Option<String>,
    pub(crate) effort: Option<Effort>,
}

/// Borrowed view of one backend's discovery state.
pub(crate) enum CatalogView<'a> {
    NotLoaded,
    Loading,
    Ready(&'a DiscoveredCatalog),
    Failed(&'a str),
}

pub(crate) enum ModelChoices {
    Loading,
    Ready(Vec<DiscoveredModel>),
    Failed(String),
}

impl ModelCatalog {
    /// Begin an asynchronous lookup ahead of opening a model picker. Repeated
    /// calls for the same backend and working directory are coalesced.
    pub(crate) fn preload(&mut self, backend: BackendKind, cwd: &Path) {
        if !self.frozen {
            self.request(backend, cwd);
        }
    }

    /// Stop automatic background warm-up after the initial roster. An explicit
    /// Model selection may still load a newly added backend on demand; that
    /// result is cached and shared for the rest of this `ast` process.
    pub(crate) fn freeze(&mut self) {
        self.frozen = true;
    }

    /// Re-fetch exactly one catalog on explicit user request. A load that is
    /// already in flight remains in place; every other cached result is
    /// replaced so matching backend/working-directory members share the fresh
    /// result.
    pub(crate) fn refresh(&mut self, backend: BackendKind, cwd: &Path) -> Result<(), String> {
        let key = (backend, cwd.to_path_buf());
        match self.loads.get(&key) {
            Some(ModelLoad::Loading(_)) => Err(format!(
                "{} model catalog is still loading",
                backend.as_str()
            )),
            Some(ModelLoad::Ready(_)) => {
                self.loads.remove(&key);
                self.request(backend, cwd);
                Ok(())
            }
            None => {
                self.request(backend, cwd);
                Ok(())
            }
        }
    }

    /// Start discovery without cloning a ready model list. This is used by
    /// background warm-up paths that can be called on every TUI frame.
    /// Start discovery now even after the startup warm-up has frozen
    /// automatic preloading. Used when the `/model` selector opens.
    pub(crate) fn ensure(&mut self, backend: BackendKind, cwd: &Path) {
        self.request(backend, cwd);
    }

    /// What is known about one backend's models, without cloning the list.
    pub(crate) fn view(&self, backend: BackendKind, cwd: &Path) -> CatalogView<'_> {
        match self.loads.get(&(backend, cwd.to_path_buf())) {
            None => CatalogView::NotLoaded,
            Some(ModelLoad::Loading(_)) => CatalogView::Loading,
            Some(ModelLoad::Ready(Ok(catalog))) => CatalogView::Ready(catalog),
            Some(ModelLoad::Ready(Err(err))) => CatalogView::Failed(err),
        }
    }

    fn request(&mut self, backend: BackendKind, cwd: &Path) {
        let key = (backend, cwd.to_path_buf());
        if self.loads.contains_key(&key) {
            return;
        }
        let worker_cwd = cwd.to_path_buf();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(crate::adapter::models::discover_catalog(
                backend,
                &worker_cwd,
            ));
        });
        self.loads.insert(key, ModelLoad::Loading(rx));
    }

    /// Move finished lookups into the cache. Returns whether any landed, so
    /// the caller knows a redraw is due.
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        let keys = self.loads.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            let result = match self.loads.get(&key) {
                Some(ModelLoad::Loading(rx)) => match rx.try_recv() {
                    Ok(result) => Some(result),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Err(
                        "model discovery worker stopped unexpectedly".to_string(),
                    )),
                },
                _ => None,
            };
            if let Some(result) = result {
                self.loads.insert(key, ModelLoad::Ready(result));
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn models(&mut self, backend: BackendKind, cwd: &Path) -> ModelChoices {
        let key = (backend, cwd.to_path_buf());
        // `models` is reached only from an explicit Model-field activation.
        // Unlike background preload, that user action may add one new catalog
        // after startup (for example after adding a Codex member). The key
        // makes it one request shared by every member with this backend/cwd.
        self.request(backend, cwd);
        let Some(load) = self.loads.get_mut(&key) else {
            return ModelChoices::Loading;
        };
        let result = match load {
            ModelLoad::Loading(rx) => match rx.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return ModelChoices::Loading,
                Err(TryRecvError::Disconnected) => {
                    Err("model discovery worker stopped unexpectedly".to_string())
                }
            },
            ModelLoad::Ready(result) => return model_choices(result),
        };
        let choices = model_choices(&result);
        *load = ModelLoad::Ready(result);
        choices
    }

    #[cfg(test)]
    pub(crate) fn seed(&mut self, backend: BackendKind, cwd: &Path, models: Vec<String>) {
        self.loads.insert(
            (backend, cwd.to_path_buf()),
            ModelLoad::Ready(Ok(DiscoveredCatalog {
                models: models
                    .into_iter()
                    .enumerate()
                    .map(|(index, id)| {
                        let mut model = DiscoveredModel::simple(id);
                        model.is_default = index == 0;
                        model
                    })
                    .collect(),
                native_permission: None,
                ..Default::default()
            })),
        );
    }

    #[cfg(test)]
    pub(crate) fn seed_catalog(
        &mut self,
        backend: BackendKind,
        cwd: &Path,
        catalog: DiscoveredCatalog,
    ) {
        self.loads
            .insert((backend, cwd.to_path_buf()), ModelLoad::Ready(Ok(catalog)));
    }

    #[cfg(test)]
    pub(crate) fn seed_failed(&mut self, backend: BackendKind, cwd: &Path, error: &str) {
        self.loads.insert(
            (backend, cwd.to_path_buf()),
            ModelLoad::Ready(Err(error.to_string())),
        );
    }

    /// A lookup still in flight; send its result through the returned handle.
    #[cfg(test)]
    pub(crate) fn seed_pending(
        &mut self,
        backend: BackendKind,
        cwd: &Path,
    ) -> mpsc::Sender<Result<DiscoveredCatalog, String>> {
        let (tx, rx) = mpsc::channel();
        self.loads
            .insert((backend, cwd.to_path_buf()), ModelLoad::Loading(rx));
        tx
    }

    #[cfg(test)]
    pub(crate) fn seed_loading(&mut self, backend: BackendKind, cwd: &Path) {
        let (_tx, rx) = mpsc::channel();
        self.loads
            .insert((backend, cwd.to_path_buf()), ModelLoad::Loading(rx));
    }

    #[cfg(test)]
    pub(crate) fn seed_with_native_permission(
        &mut self,
        backend: BackendKind,
        cwd: &Path,
        permission: &str,
    ) {
        self.loads.insert(
            (backend, cwd.to_path_buf()),
            ModelLoad::Ready(Ok(DiscoveredCatalog {
                models: Vec::new(),
                native_permission: Some(permission.to_string()),
                ..Default::default()
            })),
        );
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, backend: BackendKind, cwd: &Path) -> bool {
        self.loads.contains_key(&(backend, cwd.to_path_buf()))
    }

    fn is_loading_for(&self, backend: BackendKind, cwd: &Path) -> bool {
        matches!(
            self.loads.get(&(backend, cwd.to_path_buf())),
            Some(ModelLoad::Loading(_))
        )
    }

    fn is_failed_for(&self, backend: BackendKind, cwd: &Path) -> bool {
        matches!(
            self.loads.get(&(backend, cwd.to_path_buf())),
            Some(ModelLoad::Ready(Err(_)))
        )
    }

    fn has_ready_catalog(&self, backend: BackendKind, cwd: &Path) -> bool {
        matches!(
            self.loads.get(&(backend, cwd.to_path_buf())),
            Some(ModelLoad::Ready(Ok(_)))
        )
    }

    /// Show a backend's configured permission default when Asterline leaves it
    /// unmodified. Codex is excluded: Asterline deliberately sends its product
    /// default (`never`) rather than inheriting a local CLI policy.
    pub(crate) fn native_permission_label(
        &self,
        member: &TeamMember,
        workspace: &Path,
    ) -> Option<String> {
        if member.backend == BackendKind::Codex {
            return None;
        }
        if !matches!(member.permission_mode, None | Some(PermissionMode::Default)) {
            return None;
        }
        let cwd = member.resolved_cwd(workspace);
        match self.loads.get(&(member.backend, cwd)) {
            Some(ModelLoad::Loading(_)) => Some("loading…".to_string()),
            Some(ModelLoad::Ready(Ok(catalog))) => catalog.native_permission.clone().map(|mode| {
                if member.backend == BackendKind::Grok {
                    crate::domain::team::normalize_grok_permission_label(&mode).to_string()
                } else {
                    mode
                }
            }),
            Some(ModelLoad::Ready(Err(_))) => Some("unavailable".to_string()),
            None => None,
        }
    }

    /// The model and reasoning effort a member actually runs, resolved in
    /// one order everywhere they are shown: the value pinned on the member,
    /// else the one the CLI's own configuration selects, else the catalog's
    /// default. Display only: nothing here is written back to `team.json`.
    pub(crate) fn effective_model(
        &self,
        backend: BackendKind,
        model: Option<&str>,
        effort: Option<Effort>,
        cwd: &Path,
    ) -> EffectiveModel {
        let catalog = match self.loads.get(&(backend, cwd.to_path_buf())) {
            Some(ModelLoad::Ready(Ok(catalog))) => Some(catalog),
            _ => None,
        };
        let id = model
            .map(str::to_string)
            .or_else(|| catalog.and_then(|catalog| catalog.native_model.clone()))
            .or_else(|| {
                catalog
                    .and_then(|catalog| catalog.models.iter().find(|model| model.is_default))
                    .map(|model| model.id.clone())
            });
        let discovered = id.as_deref().and_then(|id| {
            catalog.and_then(|catalog| catalog.models.iter().find(|model| model.matches_id(id)))
        });
        let supported = |effort: &Effort| {
            discovered.is_none_or(|model| {
                model.supported_efforts.is_empty() || model.supported_efforts.contains(effort)
            })
        };
        // Agy encodes the reasoning tier in the model name (for example
        // `gemini-…-high`), so a separate effort would only repeat it.
        let effort = if backend == BackendKind::Agy {
            None
        } else {
            effort
                .or_else(|| {
                    let id = id.as_deref()?;
                    catalog?
                        .native_efforts
                        .for_model(id)
                        .filter(|effort| supported(effort))
                })
                .or_else(|| {
                    discovered
                        .and_then(|model| model.default_effort)
                        .filter(|effort| supported(effort))
                })
        };
        EffectiveModel {
            id,
            name: discovered.map(|model| model.name.clone()),
            effort,
        }
    }

    /// `<Display name> · <effort>` for the model a member effectively runs,
    /// or just the part that is known; `None` when neither is.
    pub(crate) fn effective_summary(
        &self,
        backend: BackendKind,
        model: Option<&str>,
        effort: Option<Effort>,
        cwd: &Path,
    ) -> Option<String> {
        let effective = self.effective_model(backend, model, effort, cwd);
        let parts: Vec<String> = [
            effective.name.or(effective.id),
            effective.effort.map(|effort| effort.as_str().to_string()),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!parts.is_empty()).then(|| parts.join(" · "))
    }

    pub(crate) fn model_label(&self, member: &TeamMember, workspace: &Path) -> String {
        let cwd = member.resolved_cwd(workspace);
        let effective =
            self.effective_model(member.backend, member.model.as_deref(), member.effort, &cwd);
        let label = match (&member.model, &effective.id, &effective.name) {
            (Some(id), _, Some(name)) if name != id => format!("{name} · {id}"),
            (Some(id), _, _) => id.clone(),
            (None, _, Some(name)) => name.clone(),
            (None, Some(id), None) => id.clone(),
            (None, None, _) if self.is_loading_for(member.backend, &cwd) => "loading…".to_string(),
            (None, None, _) if self.is_failed_for(member.backend, &cwd) => {
                "unavailable".to_string()
            }
            // A successful catalog need not nominate a default model. It
            // still gives the picker real choices, so calling this state "not
            // loaded" made the field contradict the picker.
            (None, None, _) if self.has_ready_catalog(member.backend, &cwd) => {
                "CLI default".to_string()
            }
            (None, None, _) if self.frozen => "not loaded · press Enter".to_string(),
            (None, None, _) => "CLI default".to_string(),
        };
        match effective.effort {
            Some(effort) => format!("{label} · {}", effort.as_str()),
            None => label,
        }
    }

    pub(crate) fn backend_summary(&self, backend: BackendKind, cwd: &Path) -> String {
        match self.loads.get(&(backend, cwd.to_path_buf())) {
            None if self.frozen => "installed · select Model to load".to_string(),
            None => "installed · models load when selected".to_string(),
            Some(ModelLoad::Loading(_)) => "installed · loading models…".to_string(),
            Some(ModelLoad::Ready(Err(err))) => {
                format!("installed · model discovery failed: {}", one_line(err, 42))
            }
            Some(ModelLoad::Ready(Ok(catalog))) => {
                let model_names = catalog
                    .models
                    .iter()
                    .take(3)
                    .map(|model| model.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let suffix = if catalog.models.len() > 3 {
                    ", …"
                } else {
                    ""
                };
                let mut efforts = catalog
                    .models
                    .iter()
                    .flat_map(|model| model.supported_efforts.iter().copied())
                    .collect::<Vec<_>>();
                if !efforts.is_empty() {
                    efforts.sort_by_key(|effort| effort_rank(*effort));
                    efforts.dedup();
                }
                let efforts = efforts
                    .iter()
                    .map(|effort| effort.as_str())
                    .collect::<Vec<_>>()
                    .join("/");
                let effort_suffix = if efforts.is_empty() {
                    String::new()
                } else {
                    format!(" · effort {efforts}")
                };
                format!(
                    "installed · {} model(s): {model_names}{suffix}{effort_suffix}",
                    catalog.models.len()
                )
            }
        }
    }
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

fn one_line(value: &str, max: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= max {
        value
    } else {
        format!(
            "{}…",
            value
                .chars()
                .take(max.saturating_sub(1))
                .collect::<String>()
        )
    }
}

fn model_choices(result: &Result<DiscoveredCatalog, String>) -> ModelChoices {
    match result {
        Ok(catalog) => ModelChoices::Ready(catalog.models.clone()),
        Err(err) => ModelChoices::Failed(err.clone()),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelChoice {
    value: Option<String>,
    model: Option<DiscoveredModel>,
}

impl ModelChoice {
    pub(crate) fn name(&self) -> &str {
        if self.value.is_none() {
            "default"
        } else {
            self.model
                .as_ref()
                .map_or("Custom model", |model| &model.name)
        }
    }

    pub(crate) fn id(&self, backend: BackendKind) -> String {
        match (&self.value, &self.model) {
            (None, Some(model)) => format!("{} (CLI default)", model.id),
            (None, None) => format!("{} CLI default", backend.as_str()),
            (Some(value), _) => value.clone(),
        }
    }

    fn supported_efforts(&self) -> &[Effort] {
        self.model
            .as_ref()
            .map_or(&[], |model| &model.supported_efforts)
    }

    fn default_effort(&self) -> Option<Effort> {
        self.model.as_ref().and_then(|model| model.default_effort)
    }

    fn default_effort_label(&self) -> String {
        self.default_effort().map_or_else(
            || "default".to_string(),
            |effort| effort.as_str().to_string(),
        )
    }

    fn effort_choices_label(&self) -> String {
        std::iter::once("default")
            .chain(
                self.ordered_supported_efforts()
                    .iter()
                    .map(|effort| effort.as_str()),
            )
            .collect::<Vec<_>>()
            .join(" · ")
    }

    fn ordered_supported_efforts(&self) -> Vec<Effort> {
        let mut efforts = self.supported_efforts().to_vec();
        efforts.sort_by_key(|effort| effort_rank(*effort));
        efforts.dedup();
        efforts
    }

    pub(crate) fn description(&self) -> Option<&str> {
        self.model
            .as_ref()
            .and_then(|model| model.description.as_deref())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ModelPicker {
    backend: BackendKind,
    options: Vec<ModelChoice>,
    selected: usize,
    /// Draft effort overrides keyed by model row. An override belongs to the
    /// model it was chosen on: browsing a different model must show that
    /// model's native default, not carry an unrelated `max`/`high` setting.
    effort_overrides: HashMap<usize, Effort>,
    query: String,
}

impl ModelPicker {
    pub(crate) fn new(
        backend: BackendKind,
        current: Option<&str>,
        current_effort: Option<Effort>,
        models: Vec<DiscoveredModel>,
    ) -> Self {
        // Once the CLI gives us concrete models, make its actual default the
        // selected entry instead of obscuring it behind a generic `default`.
        // Keep `default` only as the empty-catalog fallback.
        let mut options = Vec::new();
        if models.is_empty() {
            options.push(ModelChoice {
                value: None,
                model: None,
            });
        }
        // A member saved under an alternate spelling (`claude-opus-5-5` for the
        // `opus` row) is on that row.
        let current = current.map(|current| {
            models
                .iter()
                .find(|model| model.id == current)
                .or_else(|| models.iter().find(|model| model.matches_id(current)))
                .map_or(current, |model| model.id.as_str())
                .to_string()
        });
        let current = current.as_deref();
        if let Some(current) = current
            && !models.iter().any(|model| model.id == current)
        {
            options.push(ModelChoice {
                value: Some(current.to_string()),
                model: Some(DiscoveredModel::simple(current)),
            });
        }
        options.extend(models.into_iter().map(|model| ModelChoice {
            value: Some(model.id.clone()),
            model: Some(model),
        }));
        let selected = current
            .and_then(|current| {
                options
                    .iter()
                    .position(|choice| choice.value.as_deref() == Some(current))
            })
            .or_else(|| {
                options
                    .iter()
                    .position(|choice| choice.model.as_ref().is_some_and(|model| model.is_default))
            })
            .unwrap_or(0);
        let mut effort_overrides = HashMap::new();
        if let Some(effort) = current_effort {
            effort_overrides.insert(selected, effort);
        }
        let mut picker = Self {
            backend,
            options,
            selected,
            effort_overrides,
            query: String::new(),
        };
        picker.normalize_effort();
        picker
    }

    pub(crate) fn backend(&self) -> BackendKind {
        self.backend
    }

    #[cfg(test)]
    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn query(&self) -> &str {
        &self.query
    }

    fn visible_indices(&self) -> Vec<usize> {
        let query = self.query.trim().to_ascii_lowercase();
        self.options
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                query.is_empty()
                    || choice.name().to_ascii_lowercase().contains(&query)
                    || choice
                        .id(self.backend)
                        .to_ascii_lowercase()
                        .contains(&query)
                    || choice
                        .description()
                        .is_some_and(|text| text.to_ascii_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    pub(crate) fn window(&self, max: usize) -> (usize, Vec<&ModelChoice>) {
        let visible = self.visible_indices();
        if visible.is_empty() {
            return (0, Vec::new());
        }
        let selected = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        let max = max.max(1).min(visible.len());
        let start = selected
            .saturating_sub(max / 2)
            .min(visible.len().saturating_sub(max));
        (
            start,
            visible[start..start + max]
                .iter()
                .map(|index| &self.options[*index])
                .collect(),
        )
    }

    pub(crate) fn visible_len(&self) -> usize {
        self.visible_indices().len()
    }

    pub(crate) fn selected_choice(&self) -> Option<&ModelChoice> {
        self.options.get(self.selected)
    }

    pub(crate) fn up(&mut self) {
        let visible = self.visible_indices();
        let position = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        if let Some(index) = visible.get(position.saturating_sub(1)) {
            self.selected = *index;
            self.normalize_effort();
        }
    }

    pub(crate) fn down(&mut self) {
        let visible = self.visible_indices();
        let position = visible
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        if let Some(index) = visible.get(position + 1) {
            self.selected = *index;
            self.normalize_effort();
        }
    }

    pub(crate) fn previous_effort(&mut self) {
        let choices = self.ordered_selected_efforts();
        if choices.is_empty() {
            return;
        }
        let effort = match self
            .effort()
            .and_then(|effort| choices.iter().position(|choice| *choice == effort))
        {
            None => self
                .selected_default_effort()
                .and_then(|effort| choices.iter().position(|choice| *choice == effort))
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| choices.get(index).copied()),
            Some(0) => None,
            Some(index) => Some(choices[index - 1]),
        };
        self.set_selected_effort(effort);
    }

    pub(crate) fn next_effort(&mut self) {
        let choices = self.ordered_selected_efforts();
        if choices.is_empty() {
            return;
        }
        let effort = match self
            .effort()
            .and_then(|effort| choices.iter().position(|choice| *choice == effort))
        {
            None => match self.selected_default_effort() {
                Some(effort) => choices
                    .iter()
                    .position(|choice| *choice == effort)
                    .and_then(|index| choices.get(index + 1).copied()),
                None => choices.first().copied(),
            },
            Some(index) => choices
                .get(index + 1)
                .copied()
                .or_else(|| choices.get(index).copied()),
        };
        self.set_selected_effort(effort);
    }

    pub(crate) fn push_query(&mut self, ch: char) {
        self.query.push(ch);
        self.select_first_visible();
    }

    pub(crate) fn pop_query(&mut self) {
        self.query.pop();
        self.select_first_visible();
    }

    pub(crate) fn clear_query(&mut self) {
        self.query.clear();
        self.select_first_visible();
    }

    fn select_first_visible(&mut self) {
        if let Some(index) = self.visible_indices().first() {
            self.selected = *index;
            self.normalize_effort();
        }
    }

    pub(crate) fn value(&self) -> Option<String> {
        self.options
            .get(self.selected)
            .and_then(|choice| choice.value.clone())
    }

    fn selected_efforts(&self) -> &[Effort] {
        self.selected_choice()
            .map_or(&[], ModelChoice::supported_efforts)
    }

    fn ordered_selected_efforts(&self) -> Vec<Effort> {
        let mut efforts = self.selected_efforts().to_vec();
        efforts.sort_by_key(|effort| effort_rank(*effort));
        efforts.dedup();
        efforts
    }

    fn selected_default_effort(&self) -> Option<Effort> {
        self.selected_choice().and_then(|choice| {
            choice
                .default_effort()
                .filter(|effort| choice.supported_efforts().contains(effort))
        })
    }

    fn normalize_effort(&mut self) {
        if self
            .selected_choice()
            .is_some_and(|choice| choice.value.is_none())
        {
            // Selecting the CLI-default row cannot retain an effort override.
            self.effort_overrides.remove(&self.selected);
        }
    }

    pub(crate) fn effort(&self) -> Option<Effort> {
        self.effort_overrides.get(&self.selected).copied()
    }

    fn set_selected_effort(&mut self, effort: Option<Effort>) {
        match effort {
            Some(effort) => {
                self.effort_overrides.insert(self.selected, effort);
            }
            None => {
                self.effort_overrides.remove(&self.selected);
            }
        }
    }

    /// An advertised capability list is authoritative when it is present.
    /// An empty list means the backend did not provide a machine-readable
    /// effort menu, so an existing explicit value must be preserved rather
    /// than guessed away.
    pub(crate) fn has_unsupported_effort_override(&self) -> bool {
        let choices = self.selected_efforts();
        !choices.is_empty()
            && self
                .effort()
                .is_some_and(|effort| !choices.contains(&effort))
    }

    pub(crate) fn unsupported_effort_notice(&self) -> Option<String> {
        let effort = self.effort()?;
        if !self.has_unsupported_effort_override() {
            return None;
        }
        let model = self
            .selected_choice()
            .map(|choice| choice.id(self.backend))
            .unwrap_or_else(|| "selected model".to_string());
        Some(format!(
            "{model} does not advertise {} · use ←/→ to choose an advertised effort",
            effort.as_str()
        ))
    }

    pub(crate) fn effort_label(&self) -> String {
        match self.effort() {
            Some(effort) if self.has_unsupported_effort_override() => {
                format!("{} (unsupported)", effort.as_str())
            }
            Some(effort) => format!("{} (override)", effort.as_str()),
            None => self.selected_choice().map_or_else(
                || "default".to_string(),
                |choice| format!("default {}", choice.default_effort_label()),
            ),
        }
    }

    pub(crate) fn effort_choices_label(&self) -> String {
        self.selected_choice()
            .map_or_else(|| "default".to_string(), ModelChoice::effort_choices_label)
    }

    pub(crate) fn has_effort_controls(&self) -> bool {
        self.options
            .iter()
            .any(|choice| !choice.supported_efforts().is_empty())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BackendChoice {
    pub(crate) backend: BackendKind,
    pub(crate) installed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BackendPicker {
    options: Vec<BackendChoice>,
    selected: usize,
}

impl BackendPicker {
    pub(crate) fn new(current: BackendKind, detected: DetectedBackends) -> Self {
        let options = [
            BackendKind::Codex,
            BackendKind::Claude,
            BackendKind::Grok,
            BackendKind::Agy,
        ]
        .into_iter()
        .map(|backend| BackendChoice {
            backend,
            installed: detected.contains(backend),
        })
        .collect::<Vec<_>>();
        let selected = options
            .iter()
            .position(|choice| choice.backend == current)
            .unwrap_or(0);
        Self { options, selected }
    }

    pub(crate) fn choices(&self) -> &[BackendChoice] {
        &self.options
    }

    pub(crate) fn selected(&self) -> usize {
        self.selected
    }

    pub(crate) fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub(crate) fn down(&mut self) {
        if self.selected + 1 < self.options.len() {
            self.selected += 1;
        }
    }

    pub(crate) fn selected_choice(&self) -> Option<BackendChoice> {
        self.options.get(self.selected).copied()
    }
}

pub(crate) fn backend_picker_lines(
    picker: &BackendPicker,
    catalog: &ModelCatalog,
    cwd: &Path,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled(" Agent CLI catalog", theme::accent_bold()),
        Line::styled(
            " Availability is checked first; installed model catalogs load at startup.",
            theme::muted(),
        ),
    ];
    for (index, choice) in picker.choices().iter().enumerate() {
        let selected = index == picker.selected();
        let status = if choice.installed {
            catalog.backend_summary(choice.backend, cwd)
        } else {
            "not installed on PATH".to_string()
        };
        lines.push(Line::from(vec![
            Span::styled(
                if selected { " ▶ " } else { "   " },
                if selected {
                    theme::warning_bold()
                } else {
                    theme::muted()
                },
            ),
            Span::styled(
                theme::pad_width(choice.backend.as_str(), 8),
                if selected {
                    theme::emphasis()
                } else {
                    Style::default().fg(theme::backend_color(choice.backend))
                },
            ),
            Span::styled(
                theme::clip_width(&status, width.saturating_sub(13)),
                if choice.installed {
                    theme::text()
                } else {
                    theme::muted()
                },
            ),
        ]));
    }
    lines.push(Line::styled(
        " ↑/↓ choose · Enter apply · Esc cancel",
        theme::muted_italic(),
    ));
    lines
}

pub(crate) fn model_picker_lines(
    picker: &ModelPicker,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled(" Model catalog", theme::accent_bold()),
        Line::from(vec![
            Span::styled(" Search: ", theme::muted()),
            Span::styled(
                if picker.query().is_empty() {
                    "type a model name or ID…".to_string()
                } else {
                    picker.query().to_string()
                },
                if picker.query().is_empty() {
                    theme::muted_italic()
                } else {
                    theme::emphasis()
                },
            ),
            Span::styled(
                format!("  {} match(es)", picker.visible_len()),
                theme::muted(),
            ),
        ]),
    ];
    if picker.visible_len() == 0 {
        lines.push(Line::styled(" No matching models.", theme::muted()));
        return lines;
    }

    let show_effort = picker.has_effort_controls();
    let available = width
        .saturating_sub(if show_effort { 7 } else { 4 })
        .max(30);
    let name_width = (available * 24 / 100).max(10);
    let id_width = if show_effort {
        (available * 43 / 100).max(12)
    } else {
        available.saturating_sub(name_width).max(12)
    };
    let effort_width = available.saturating_sub(name_width + id_width).max(8);
    let mut header = vec![
        Span::styled("   Model", theme::muted()),
        Span::styled(
            theme::pad_width("", name_width.saturating_sub(5)),
            theme::muted(),
        ),
        Span::styled("│ ID", theme::muted()),
        Span::styled(
            theme::pad_width("", id_width.saturating_sub(3)),
            theme::muted(),
        ),
    ];
    if show_effort {
        header.push(Span::styled("│ Effort (←/→)", theme::muted()));
    }
    lines.push(Line::from(header));

    let (start, choices) = picker.window(max_rows);
    if start > 0 {
        lines.push(Line::styled("   …", theme::muted()));
    }
    for choice in choices {
        let selected = picker
            .selected_choice()
            .is_some_and(|current| current == choice);
        let row_style = if selected {
            theme::emphasis()
        } else {
            theme::text()
        };
        let mut row = vec![
            Span::styled(
                if selected { " ▶ " } else { "   " },
                if selected {
                    theme::warning_bold()
                } else {
                    theme::muted()
                },
            ),
            Span::styled(
                theme::pad_width(&theme::clip_width(choice.name(), name_width), name_width),
                row_style,
            ),
            Span::styled("│ ", theme::muted()),
            Span::styled(
                theme::pad_width(
                    &theme::clip_width(&choice.id(picker.backend()), id_width.saturating_sub(2)),
                    id_width.saturating_sub(2),
                ),
                row_style,
            ),
        ];
        if show_effort {
            row.push(Span::styled("│ ", theme::muted()));
            row.push(Span::styled(
                theme::clip_width(
                    &if selected {
                        picker.effort_label()
                    } else {
                        choice.default_effort_label()
                    },
                    effort_width.saturating_sub(2),
                ),
                if selected { row_style } else { theme::muted() },
            ));
        }
        lines.push(Line::from(row));
    }
    if start + max_rows.min(picker.visible_len()) < picker.visible_len() {
        lines.push(Line::styled("   …", theme::muted()));
    }
    if let Some(description) = picker.selected_choice().and_then(ModelChoice::description) {
        lines.push(Line::from(vec![
            Span::styled(" About: ", theme::muted()),
            Span::styled(
                theme::clip_width(description, width.saturating_sub(8)),
                theme::text(),
            ),
        ]));
    }
    if show_effort {
        lines.push(Line::from(vec![
            Span::styled(" Effort choices: ", theme::muted()),
            Span::styled(
                theme::clip_width(&picker.effort_choices_label(), width.saturating_sub(17)),
                theme::text(),
            ),
        ]));
    }
    lines.push(Line::styled(
        if show_effort {
            " Type to filter · ↑/↓ model · ←/→ effort · Enter apply both · Esc cancel"
        } else {
            " Type to filter · ↑/↓ model · Enter apply · Esc cancel"
        },
        theme::muted_italic(),
    ));
    lines
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Field {
    Name,
    Backend,
    Role,
    Model,
    Sandbox,
    Permission,
    Session,
    SessionId,
}

impl Field {
    pub(crate) const ALL: &'static [Field] = &[
        Field::Name,
        Field::Backend,
        Field::Role,
        Field::Model,
        Field::Sandbox,
        Field::Permission,
        Field::Session,
        Field::SessionId,
    ];

    const CODEX_FIELDS: &'static [Field] = &[
        Field::Name,
        Field::Backend,
        Field::Role,
        Field::Model,
        Field::Permission,
        Field::Session,
        Field::SessionId,
    ];

    const CLAUDE_FIELDS: &'static [Field] = &[
        Field::Name,
        Field::Backend,
        Field::Role,
        Field::Model,
        Field::Permission,
        Field::Session,
        Field::SessionId,
    ];

    pub(crate) fn for_backend(backend: BackendKind) -> &'static [Field] {
        match backend {
            BackendKind::Codex => Self::CODEX_FIELDS,
            // Native mode pickers do not offer a sandbox row: Claude has no
            // sandbox flag, Grok Shift+Tab is Normal/Plan/Always-approve,
            // Agy Shift+Tab is default/accept-edits/plan. Adapters still
            // apply each backend's default sandbox underneath.
            BackendKind::Claude | BackendKind::Grok | BackendKind::Agy => Self::CLAUDE_FIELDS,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Backend => "backend",
            Self::Role => "role",
            Self::Model => "model",
            Self::Sandbox => "sandbox",
            Self::Permission => "permission",
            Self::Session => "session",
            Self::SessionId => "session id",
        }
    }

    pub(crate) fn label_for_backend(self, backend: BackendKind) -> &'static str {
        match (self, backend) {
            (Self::Permission, BackendKind::Codex) => "permissions",
            (Self::Permission, BackendKind::Claude | BackendKind::Grok) => "permission mode",
            (Self::Permission, BackendKind::Agy) => "execution mode",
            _ => self.label(),
        }
    }

    /// Detail rows are rendered as `label: value`. Keep their colons aligned
    /// even when a backend exposes longer, backend-specific field labels.
    pub(crate) fn label_width_for_backend(backend: BackendKind) -> usize {
        Self::for_backend(backend)
            .iter()
            .map(|field| field.label_for_backend(backend).len())
            .max()
            .unwrap_or_default()
    }

    pub(crate) fn is_text(self) -> bool {
        matches!(
            self,
            Self::Name | Self::Role | Self::Model | Self::SessionId
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EditState {
    pub(crate) field: Field,
    pub(crate) buffer: String,
    /// Unicode scalar index kept on a user-visible grapheme boundary.
    pub(crate) cursor: usize,
    pub(crate) title_override: Option<String>,
}

impl EditState {
    pub(crate) fn new(field: Field, buffer: String) -> Self {
        let cursor = buffer.chars().count();
        Self {
            field,
            buffer,
            cursor,
            title_override: None,
        }
    }

    pub(crate) fn named(title: impl Into<String>, buffer: String) -> Self {
        let mut edit = Self::new(Field::Name, buffer);
        edit.title_override = Some(title.into());
        edit
    }

    pub(crate) fn title(&self) -> &str {
        self.title_override
            .as_deref()
            .unwrap_or_else(|| self.field.label())
    }

    pub(crate) fn insert_text(&mut self, text: &str) {
        let text = text.replace("\r\n", " ").replace(['\r', '\n'], " ");
        let mut chars = self.buffer.chars().collect::<Vec<_>>();
        let inserted = text.chars().collect::<Vec<_>>();
        let count = inserted.len();
        let insert_at = self.cursor.min(chars.len());
        chars.splice(insert_at..insert_at, inserted);
        self.cursor = insert_at.saturating_add(count);
        self.buffer = chars.into_iter().collect();
        self.cursor = self.grapheme_boundary_at_or_after(self.cursor);
    }

    pub(crate) fn apply_key(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        let ctrl = modifiers.contains(KeyModifiers::CONTROL);
        let alt = modifiers.contains(KeyModifiers::ALT);
        match code {
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Char('b') if ctrl => self.move_left(),
            KeyCode::Char('f') if ctrl => self.move_right(),
            KeyCode::Char('b') if alt => self.move_word_left(),
            KeyCode::Char('f') if alt => self.move_word_right(),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.char_len(),
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.char_len(),
            KeyCode::Backspace => self.delete_backward(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Char('d') if ctrl => self.delete_forward(),
            KeyCode::Char('u') if ctrl => self.delete_to_start(),
            KeyCode::Char('k') if ctrl => self.delete_to_end(),
            KeyCode::Char('w') if ctrl => self.delete_word_backward(),
            KeyCode::Char(ch) if !ctrl && !alt && !ch.is_control() => {
                self.insert_text(&ch.to_string());
            }
            _ => {}
        }
    }

    pub(crate) fn visible_window(&self, width: usize) -> (String, u16) {
        if width == 0 {
            return (String::new(), 0);
        }
        let graphemes = self.buffer.graphemes(true).collect::<Vec<_>>();
        let cursor = self.grapheme_index_at_or_after(self.cursor);
        let mut start = cursor;
        let mut cursor_width = 0;
        while start > 0 {
            let grapheme_width = UnicodeWidthStr::width(graphemes[start - 1]);
            if grapheme_width > 0 && cursor_width + grapheme_width > width.saturating_sub(1) {
                break;
            }
            start -= 1;
            cursor_width += grapheme_width;
        }
        let mut visible = String::new();
        let mut visible_width = 0;
        for grapheme in &graphemes[start..] {
            let grapheme_width = UnicodeWidthStr::width(*grapheme);
            if grapheme_width > 0 && visible_width + grapheme_width > width {
                break;
            }
            visible.push_str(grapheme);
            visible_width += grapheme_width;
        }
        (visible, cursor_width.min(width) as u16)
    }

    fn char_len(&self) -> usize {
        self.buffer.chars().count()
    }

    fn move_left(&mut self) {
        self.cursor = self.previous_grapheme_boundary(self.cursor);
    }

    fn move_right(&mut self) {
        self.cursor = self.next_grapheme_boundary(self.cursor);
    }

    fn move_word_left(&mut self) {
        let graphemes = self.buffer.graphemes(true).collect::<Vec<_>>();
        let mut index = self.grapheme_index_at_or_after(self.cursor);
        while index > 0 && graphemes[index - 1].chars().all(char::is_whitespace) {
            index -= 1;
        }
        while index > 0 && !graphemes[index - 1].chars().all(char::is_whitespace) {
            index -= 1;
        }
        self.cursor = graphemes[..index]
            .iter()
            .map(|grapheme| grapheme.chars().count())
            .sum();
    }

    fn move_word_right(&mut self) {
        let graphemes = self.buffer.graphemes(true).collect::<Vec<_>>();
        let mut index = self.grapheme_index_at_or_after(self.cursor);
        while index < graphemes.len() && !graphemes[index].chars().all(char::is_whitespace) {
            index += 1;
        }
        while index < graphemes.len() && graphemes[index].chars().all(char::is_whitespace) {
            index += 1;
        }
        self.cursor = graphemes[..index]
            .iter()
            .map(|grapheme| grapheme.chars().count())
            .sum();
    }

    fn delete_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut chars = self.buffer.chars().collect::<Vec<_>>();
        let start = self.previous_grapheme_boundary(self.cursor);
        chars.drain(start..self.cursor);
        self.cursor = start;
        self.buffer = chars.into_iter().collect();
    }

    fn delete_forward(&mut self) {
        let mut chars = self.buffer.chars().collect::<Vec<_>>();
        if self.cursor < chars.len() {
            let end = self.next_grapheme_boundary(self.cursor);
            chars.drain(self.cursor..end);
            self.buffer = chars.into_iter().collect();
        }
    }

    fn delete_to_start(&mut self) {
        let mut chars = self.buffer.chars().collect::<Vec<_>>();
        chars.drain(..self.cursor.min(chars.len()));
        self.cursor = 0;
        self.buffer = chars.into_iter().collect();
    }

    fn delete_to_end(&mut self) {
        let mut chars = self.buffer.chars().collect::<Vec<_>>();
        chars.truncate(self.cursor.min(chars.len()));
        self.buffer = chars.into_iter().collect();
    }

    fn delete_word_backward(&mut self) {
        let end = self.cursor;
        self.move_word_left();
        let mut chars = self.buffer.chars().collect::<Vec<_>>();
        chars.drain(self.cursor..end.min(chars.len()));
        self.buffer = chars.into_iter().collect();
    }

    fn grapheme_boundaries(&self) -> Vec<usize> {
        let mut scalar = 0;
        let mut boundaries = vec![0];
        for grapheme in self.buffer.graphemes(true) {
            scalar += grapheme.chars().count();
            boundaries.push(scalar);
        }
        boundaries
    }

    fn previous_grapheme_boundary(&self, cursor: usize) -> usize {
        self.grapheme_boundaries()
            .into_iter()
            .take_while(|boundary| *boundary < cursor)
            .last()
            .unwrap_or(0)
    }

    fn next_grapheme_boundary(&self, cursor: usize) -> usize {
        self.grapheme_boundaries()
            .into_iter()
            .find(|boundary| *boundary > cursor)
            .unwrap_or(self.char_len())
    }

    fn grapheme_boundary_at_or_after(&self, cursor: usize) -> usize {
        self.grapheme_boundaries()
            .into_iter()
            .find(|boundary| *boundary >= cursor)
            .unwrap_or(self.char_len())
    }

    fn grapheme_index_at_or_after(&self, cursor: usize) -> usize {
        self.grapheme_boundaries()
            .into_iter()
            .position(|boundary| boundary >= cursor)
            .unwrap_or_else(|| self.buffer.graphemes(true).count())
    }
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width - width) / 2;
    let y = area.y + (area.height - height) / 2;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(y - area.y),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(x - area.x),
            Constraint::Length(width),
            Constraint::Min(0),
        ])
        .split(vertical[1])[1]
}

/// Focused single-line editor shared by startup and the live Team drawer.
pub(crate) fn render_edit_box(frame: &mut ratatui::Frame<'_>, area: Rect, edit: &EditState) {
    let popup = centered(
        area,
        area.width.saturating_sub(2).min(72),
        area.height.min(7),
    );
    frame.render_widget(Clear, popup);
    let block = Block::default()
        .title(format!(" Edit {} ", edit.title()))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::accent_bold());
    let body = block.inner(popup);
    frame.render_widget(block, popup);
    if body.height == 0 {
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(body);
    let input_block = Block::default()
        .title(" Value ")
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(theme::warning_bold());
    let input_inner = input_block.inner(chunks[0]);
    frame.render_widget(input_block, chunks[0]);
    let (visible, cursor_x) = edit.visible_window(input_inner.width as usize);
    frame.render_widget(Paragraph::new(Line::raw(visible)), input_inner);
    if input_inner.width > 0 && input_inner.height > 0 {
        frame.set_cursor_position((
            input_inner.x + cursor_x.min(input_inner.width.saturating_sub(1)),
            input_inner.y,
        ));
    }
    if chunks[1].height > 0 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                " Enter save · Esc cancel · ←/→ move · Ctrl+U/W/K edit",
                theme::muted_italic(),
            )),
            chunks[1],
        );
    }
}

pub(crate) fn field_value(member: &TeamMember, field: Field) -> String {
    match field {
        Field::Name => member.display_name.clone(),
        Field::Backend => member.backend.as_str().to_string(),
        Field::Role => member.role.clone(),
        Field::Model => member
            .model
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        Field::Sandbox => sandbox_value(member),
        Field::Permission => permission_value(member),
        Field::Session => {
            session_status_label(member.session_policy, member.session_id.as_deref(), false)
                .to_string()
        }
        Field::SessionId => match (&member.session_policy, &member.session_id) {
            (_, Some(session_id)) => session_id.clone(),
            (SessionPolicy::Resume, None) => "select a session · Enter to choose".to_string(),
            (SessionPolicy::Fresh, None) => "created on next send".to_string(),
        },
    }
}

pub(crate) fn session_status_label(
    policy: SessionPolicy,
    session_id: Option<&str>,
    creating: bool,
) -> &'static str {
    match (session_id.is_some(), creating, policy) {
        (true, _, _) => "bound",
        (false, true, _) => "freshing",
        (false, false, SessionPolicy::Resume) => "resume",
        (false, false, SessionPolicy::Fresh) => "fresh",
    }
}

fn sandbox_value(member: &TeamMember) -> String {
    member.sandbox_native_label().to_string()
}

pub(crate) fn cycle_sandbox(backend: BackendKind, current: SandboxPolicy) -> SandboxPolicy {
    match backend {
        // Agy's CLI flag is boolean. Two stored off-states exist only so
        // `--dangerously-skip-permissions` can be gated; both display as `off`.
        BackendKind::Agy => match current {
            SandboxPolicy::ReadOnly => SandboxPolicy::WorkspaceWrite,
            SandboxPolicy::WorkspaceWrite | SandboxPolicy::DangerFullAccess => {
                SandboxPolicy::ReadOnly
            }
        },
        _ => match current {
            SandboxPolicy::ReadOnly => SandboxPolicy::WorkspaceWrite,
            SandboxPolicy::WorkspaceWrite => SandboxPolicy::DangerFullAccess,
            SandboxPolicy::DangerFullAccess => SandboxPolicy::ReadOnly,
        },
    }
}

pub(crate) fn apply_sandbox_cycle(member: &mut TeamMember) {
    member.sandbox = cycle_sandbox(member.backend, member.sandbox);
    if member.backend == BackendKind::Agy
        && member.sandbox == SandboxPolicy::ReadOnly
        && member.permission_mode == Some(PermissionMode::BypassPermissions)
    {
        member.permission_mode = Some(PermissionMode::Plan);
    }
}

fn permission_value(member: &TeamMember) -> String {
    member.permission_native_label()
}

pub(crate) fn apply_backend_write_defaults(member: &mut TeamMember) {
    let defaults = default_member(member.backend);
    member.sandbox = defaults.sandbox;
    member.permission_mode = defaults.permission_mode;
    member.approvals_reviewer = defaults.approvals_reviewer;
}

pub(crate) fn copy_write_permissions(from: &TeamMember, to: &mut TeamMember) {
    to.sandbox = from.sandbox;
    to.permission_mode = from.permission_mode;
    to.approvals_reviewer = from.approvals_reviewer;
}

pub(crate) fn apply_permission_cycle(member: &mut TeamMember) {
    if member.backend == BackendKind::Codex {
        member.cycle_codex_permissions();
        return;
    }
    let current = if member.backend == BackendKind::Agy && member.sandbox == SandboxPolicy::ReadOnly
    {
        Some(PermissionMode::Plan)
    } else {
        member.permission_mode
    };
    member.permission_mode = cycle_permission_for_backend(member.backend, current);
    member.apply_visible_mode_sandbox();
}

pub(crate) fn cycle_permission_for_backend(
    backend: BackendKind,
    current: Option<PermissionMode>,
) -> Option<PermissionMode> {
    match backend {
        BackendKind::Codex => match current {
            None | Some(PermissionMode::Default) => Some(PermissionMode::AcceptEdits),
            Some(PermissionMode::AcceptEdits | PermissionMode::Plan) => Some(PermissionMode::Auto),
            Some(PermissionMode::Auto) => None,
            Some(PermissionMode::DontAsk | PermissionMode::BypassPermissions) => {
                Some(PermissionMode::AcceptEdits)
            }
        },
        BackendKind::Claude => match current {
            None | Some(PermissionMode::Default) => Some(PermissionMode::AcceptEdits),
            Some(PermissionMode::AcceptEdits) => Some(PermissionMode::Plan),
            Some(PermissionMode::Plan) => Some(PermissionMode::Auto),
            Some(PermissionMode::Auto) => Some(PermissionMode::DontAsk),
            Some(PermissionMode::DontAsk) => Some(PermissionMode::BypassPermissions),
            Some(PermissionMode::BypassPermissions) => None,
        },
        // Grok CLI-accepted cycle: default, auto, plan, always-approve.
        BackendKind::Grok => match current {
            None | Some(PermissionMode::Default | PermissionMode::DontAsk) => {
                Some(PermissionMode::Auto)
            }
            Some(PermissionMode::AcceptEdits | PermissionMode::Auto) => Some(PermissionMode::Plan),
            Some(PermissionMode::Plan) => Some(PermissionMode::BypassPermissions),
            Some(PermissionMode::BypassPermissions) => None,
        },
        BackendKind::Agy => match current {
            None
            | Some(PermissionMode::Default | PermissionMode::Auto | PermissionMode::DontAsk) => {
                Some(PermissionMode::AcceptEdits)
            }
            Some(PermissionMode::AcceptEdits) => Some(PermissionMode::Plan),
            Some(PermissionMode::Plan) => Some(PermissionMode::BypassPermissions),
            Some(PermissionMode::BypassPermissions) => None,
        },
    }
}

pub(crate) fn normalize_member_id(value: &str, fallback: &str) -> String {
    normalize_domain_member_id(value, fallback)
}

pub(crate) fn unique_member_id(base: &str, members: &[TeamMember], skip: Option<usize>) -> String {
    let base = normalize_member_id(base, "member");
    let mut candidate = base.clone();
    let mut suffix = 2usize;
    while candidate.eq_ignore_ascii_case("all")
        || members
            .iter()
            .enumerate()
            .any(|(idx, member)| Some(idx) != skip && member.id.as_str() == candidate.as_str())
    {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

pub(crate) fn unique_display_name(base: &str, members: &[TeamMember]) -> String {
    unique_display_name_except(base, members, None)
}

pub(crate) fn unique_display_name_except(
    base: &str,
    members: &[TeamMember],
    skip: Option<usize>,
) -> String {
    let mut candidate = base.to_string();
    let mut suffix = 2usize;
    while candidate.eq_ignore_ascii_case("all")
        || members.iter().enumerate().any(|(idx, member)| {
            Some(idx) != skip && member.display_name.eq_ignore_ascii_case(&candidate)
        })
    {
        candidate = format!("{base} {suffix}");
        suffix += 1;
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_fields_render_and_cycle_by_backend_capability() {
        let mut codex = TeamMember::new("codex", "Codex", BackendKind::Codex, "impl");
        assert_eq!(
            Field::Permission.label_for_backend(codex.backend),
            "permissions"
        );
        assert!(!Field::for_backend(BackendKind::Codex).contains(&Field::Sandbox));
        assert_eq!(field_value(&codex, Field::Permission), "Custom permissions");
        apply_permission_cycle(&mut codex);
        assert_eq!(field_value(&codex, Field::Permission), "Read Only");
        apply_permission_cycle(&mut codex);
        assert_eq!(field_value(&codex, Field::Permission), "Ask for approval");
        apply_permission_cycle(&mut codex);
        assert_eq!(field_value(&codex, Field::Permission), "Approve for me");
        apply_permission_cycle(&mut codex);
        assert_eq!(field_value(&codex, Field::Permission), "Full Access");

        let mut claude = TeamMember::new("claude", "Claude", BackendKind::Claude, "review");
        assert_eq!(
            Field::Permission.label_for_backend(claude.backend),
            "permission mode"
        );
        assert!(!Field::for_backend(BackendKind::Claude).contains(&Field::Sandbox));
        claude.permission_mode = cycle_permission_for_backend(claude.backend, None);
        assert_eq!(field_value(&claude, Field::Permission), "acceptEdits");

        let mut grok_perm = TeamMember::new("grok", "Grok", BackendKind::Grok, "impl");
        grok_perm.sandbox = SandboxPolicy::ReadOnly;
        assert_eq!(field_value(&grok_perm, Field::Permission), "default");
        apply_permission_cycle(&mut grok_perm);
        assert_eq!(field_value(&grok_perm, Field::Permission), "auto");
        assert_eq!(grok_perm.sandbox, SandboxPolicy::WorkspaceWrite);
        apply_permission_cycle(&mut grok_perm);
        assert_eq!(field_value(&grok_perm, Field::Permission), "plan");
        assert_eq!(grok_perm.sandbox, SandboxPolicy::WorkspaceWrite);
        apply_permission_cycle(&mut grok_perm);
        assert_eq!(field_value(&grok_perm, Field::Permission), "always-approve");
        assert_eq!(grok_perm.sandbox, SandboxPolicy::DangerFullAccess);
        apply_permission_cycle(&mut grok_perm);
        assert_eq!(field_value(&grok_perm, Field::Permission), "default");
        assert_eq!(grok_perm.sandbox, SandboxPolicy::WorkspaceWrite);

        let mut agy = TeamMember::new("agy", "Agy", BackendKind::Agy, "research");
        assert_eq!(
            Field::Permission.label_for_backend(agy.backend),
            "execution mode"
        );
        assert!(!Field::for_backend(BackendKind::Agy).contains(&Field::Sandbox));
        assert!(!Field::for_backend(BackendKind::Grok).contains(&Field::Sandbox));
        agy.sandbox = SandboxPolicy::WorkspaceWrite;
        assert_eq!(field_value(&agy, Field::Permission), "CLI default");
        apply_permission_cycle(&mut agy);
        assert_eq!(field_value(&agy, Field::Permission), "accept-edits");
        assert_eq!(agy.sandbox, SandboxPolicy::WorkspaceWrite);
        apply_permission_cycle(&mut agy);
        assert_eq!(field_value(&agy, Field::Permission), "plan");
        assert_eq!(agy.sandbox, SandboxPolicy::ReadOnly);
        apply_permission_cycle(&mut agy);
        assert_eq!(agy.sandbox, SandboxPolicy::DangerFullAccess);
        assert_eq!(
            field_value(&agy, Field::Permission),
            "dangerously-skip-permissions"
        );
        apply_permission_cycle(&mut agy);
        assert_eq!(field_value(&agy, Field::Permission), "CLI default");
        assert_eq!(agy.sandbox, SandboxPolicy::WorkspaceWrite);
    }

    #[test]
    fn catalog_shows_the_native_permission_when_member_has_no_override() {
        let cwd = Path::new("/tmp/ws");
        let mut catalog = ModelCatalog::default();
        catalog.seed_with_native_permission(BackendKind::Claude, cwd, "bypassPermissions");
        let member = TeamMember::new("claude", "Claude", BackendKind::Claude, "review");

        assert_eq!(
            catalog.native_permission_label(&member, cwd),
            Some("bypassPermissions".to_string())
        );
    }

    #[test]
    fn effort_is_configured_only_in_the_model_picker() {
        for backend in [
            BackendKind::Codex,
            BackendKind::Claude,
            BackendKind::Grok,
            BackendKind::Agy,
        ] {
            assert!(
                !Field::for_backend(backend)
                    .iter()
                    .any(|field| field.label() == "effort")
            );
        }
    }

    #[test]
    fn grok_picker_hides_unreported_effort_controls() {
        let picker = ModelPicker::new(
            BackendKind::Grok,
            None,
            None,
            vec![DiscoveredModel::simple("grok-4.5")],
        );
        let rendered = model_picker_lines(&picker, 90, 8)
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(!picker.has_effort_controls());
        assert!(!rendered.contains("Effort"));
        assert!(!rendered.contains("←/→"));
    }

    #[test]
    fn model_catalog_is_scoped_to_member_working_directory() {
        let mut catalog = ModelCatalog::default();
        catalog.seed(
            BackendKind::Claude,
            Path::new("/tmp/one"),
            vec!["project-one".to_string()],
        );
        catalog.seed(
            BackendKind::Claude,
            Path::new("/tmp/two"),
            vec!["project-two".to_string()],
        );

        let ModelChoices::Ready(one) = catalog.models(BackendKind::Claude, Path::new("/tmp/one"))
        else {
            panic!("expected first project model");
        };
        let ModelChoices::Ready(two) = catalog.models(BackendKind::Claude, Path::new("/tmp/two"))
        else {
            panic!("expected second project model");
        };

        assert_eq!(one[0].id, "project-one");
        assert_eq!(two[0].id, "project-two");
    }

    #[test]
    fn model_picker_keeps_an_unverified_custom_model_effort() {
        let picker = ModelPicker::new(
            BackendKind::Claude,
            Some("company-model"),
            Some(Effort::High),
            vec![DiscoveredModel::simple("sonnet")],
        );

        assert_eq!(picker.value().as_deref(), Some("company-model"));
        assert_eq!(picker.selected(), 0);
        assert_eq!(picker.effort(), Some(Effort::High));
        assert!(!picker.has_unsupported_effort_override());
    }

    #[test]
    fn model_picker_arrows_follow_increasing_effort_order() {
        let mut model = DiscoveredModel::simple("grok-4.6");
        model.default_effort = Some(Effort::High);
        // This is Grok's cache/menu order. The UI must still make right mean
        // "more", rather than walking that raw descending sequence.
        model.supported_efforts = vec![Effort::Xhigh, Effort::High, Effort::Medium, Effort::Low];
        let mut picker = ModelPicker::new(
            BackendKind::Grok,
            Some("grok-4.6"),
            Some(Effort::High),
            vec![model],
        );

        assert_eq!(
            picker.effort_choices_label(),
            "default · low · medium · high · xhigh"
        );
        picker.next_effort();
        assert_eq!(picker.effort(), Some(Effort::Xhigh));
        picker.previous_effort();
        picker.previous_effort();
        assert_eq!(picker.effort(), Some(Effort::Medium));
    }

    #[test]
    fn browsing_models_keeps_native_default_effort_inherited() {
        let mut medium = DiscoveredModel::simple("gpt-medium");
        medium.default_effort = Some(Effort::Medium);
        medium.supported_efforts = vec![Effort::Low, Effort::Medium, Effort::High];
        let mut low = DiscoveredModel::simple("gpt-low");
        low.default_effort = Some(Effort::Low);
        low.supported_efforts = vec![Effort::Low, Effort::Medium];
        let mut picker = ModelPicker::new(
            BackendKind::Codex,
            Some("gpt-medium"),
            None,
            vec![medium, low],
        );

        assert_eq!(picker.effort(), None);
        assert_eq!(picker.effort_label(), "default medium");

        picker.down();
        assert_eq!(picker.effort(), None);
        assert_eq!(picker.effort_label(), "default low");

        picker.up();
        assert_eq!(picker.effort(), None);
        assert_eq!(picker.effort_label(), "default medium");
    }

    #[test]
    fn effort_arrows_are_relative_to_the_native_default() {
        let mut model = DiscoveredModel::simple("gpt-medium");
        model.default_effort = Some(Effort::Medium);
        model.supported_efforts = vec![Effort::Low, Effort::Medium, Effort::High];
        let mut picker =
            ModelPicker::new(BackendKind::Codex, Some("gpt-medium"), None, vec![model]);

        assert_eq!(picker.effort(), None);
        assert_eq!(picker.effort_label(), "default medium");

        picker.next_effort();
        assert_eq!(picker.effort(), Some(Effort::High));

        let mut picker = ModelPicker::new(
            BackendKind::Codex,
            Some("gpt-medium"),
            None,
            vec![DiscoveredModel {
                id: "gpt-medium".to_string(),
                name: "gpt-medium".to_string(),
                description: None,
                default_effort: Some(Effort::Medium),
                supported_efforts: vec![Effort::Low, Effort::Medium, Effort::High],
                is_default: false,
                alt_ids: Vec::new(),
            }],
        );
        picker.previous_effort();
        assert_eq!(picker.effort(), Some(Effort::Low));
    }

    #[test]
    fn browsing_models_uses_each_models_own_effort_default() {
        let mut spark = DiscoveredModel::simple("gpt-5.3-codex-spark");
        spark.default_effort = Some(Effort::High);
        spark.supported_efforts = vec![Effort::Medium, Effort::High];
        let mut sol = DiscoveredModel::simple("gpt-5.6-sol");
        sol.default_effort = Some(Effort::Low);
        sol.supported_efforts = vec![Effort::Low];
        let mut picker = ModelPicker::new(
            BackendKind::Codex,
            Some("gpt-5.3-codex-spark"),
            Some(Effort::High),
            vec![spark, sol],
        );

        assert_eq!(picker.effort(), Some(Effort::High));
        assert!(!picker.has_unsupported_effort_override());
        assert_eq!(picker.effort_label(), "high (override)");

        picker.down();
        assert_eq!(picker.effort(), None);
        assert!(!picker.has_unsupported_effort_override());
        assert_eq!(picker.effort_label(), "default low");

        picker.up();
        assert_eq!(picker.effort(), Some(Effort::High));
        assert!(!picker.has_unsupported_effort_override());
        assert_eq!(picker.effort_label(), "high (override)");
    }

    #[test]
    fn backend_picker_shows_only_reported_model_capabilities() {
        let detected = DetectedBackends {
            codex: true,
            claude: false,
            grok: false,
            agy: false,
        };
        let picker = BackendPicker::new(BackendKind::Codex, detected);
        let mut catalog = ModelCatalog::default();
        catalog.seed(
            BackendKind::Codex,
            Path::new("/tmp/ws"),
            vec!["gpt-test".to_string()],
        );

        let text = backend_picker_lines(&picker, &catalog, Path::new("/tmp/ws"), 120)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("codex"));
        assert!(text.contains("installed · 1 model(s): gpt-test"));
        assert!(!text.contains("effort "));
        assert!(text.contains("claude"));
        assert!(text.contains("not installed on PATH"));
    }

    #[test]
    fn model_picker_filters_names_ids_and_descriptions() {
        let mut sol = DiscoveredModel::simple("gpt-5.6-sol");
        sol.name = "GPT-5.6-Sol".to_string();
        sol.description = Some("Frontier coding model".to_string());
        sol.is_default = true;
        let mut picker = ModelPicker::new(
            BackendKind::Codex,
            None,
            None,
            vec![sol, DiscoveredModel::simple("gpt-5.4-mini")],
        );

        for ch in "frontier".chars() {
            picker.push_query(ch);
        }
        assert_eq!(picker.visible_len(), 1);
        assert_eq!(picker.value().as_deref(), Some("gpt-5.6-sol"));
        picker.clear_query();
        for ch in "mini".chars() {
            picker.push_query(ch);
        }
        assert_eq!(picker.visible_len(), 1);
        assert_eq!(picker.value().as_deref(), Some("gpt-5.4-mini"));
    }

    #[test]
    fn catalog_shows_detected_model_without_default_prefix() {
        let mut model = DiscoveredModel::simple("gpt-5.6-sol");
        model.name = "GPT-5.6-Sol".to_string();
        model.default_effort = Some(Effort::Medium);
        model.supported_efforts = vec![Effort::Low, Effort::Medium, Effort::High];
        model.is_default = true;
        let mut catalog = ModelCatalog::default();
        catalog.loads.insert(
            (BackendKind::Codex, PathBuf::from("/tmp/ws")),
            ModelLoad::Ready(Ok(DiscoveredCatalog {
                models: vec![model],
                native_permission: None,
                ..Default::default()
            })),
        );
        let member = TeamMember::new("builder", "Builder", BackendKind::Codex, "impl");

        assert_eq!(
            catalog.model_label(&member, Path::new("/tmp/ws")),
            "GPT-5.6-Sol · medium"
        );
    }

    fn sol_catalog(native_effort: Option<Effort>, native_model: Option<&str>) -> DiscoveredCatalog {
        let mut sol = DiscoveredModel::simple("gpt-6.1-sol");
        sol.name = "GPT-6.1-Sol".to_string();
        sol.supported_efforts = vec![Effort::Low, Effort::Medium, Effort::High];
        sol.default_effort = Some(Effort::Low);
        sol.is_default = true;
        let mut astra = DiscoveredModel::simple("gpt-6-astra");
        astra.name = "GPT-6-Astra".to_string();
        astra.supported_efforts = vec![Effort::Low, Effort::Medium, Effort::High];
        astra.default_effort = Some(Effort::Medium);
        DiscoveredCatalog {
            models: vec![sol, astra],
            native_model: native_model.map(str::to_string),
            native_efforts: crate::adapter::models::NativeEfforts {
                global: native_effort,
                by_model: Vec::new(),
            },
            ..Default::default()
        }
    }

    /// The Claude rows as discovery builds them: aliases carry their current
    /// full id, and the fable row is Claude's cached `[1m]` option.
    fn claude_alias_catalog(native_model: Option<&str>) -> DiscoveredCatalog {
        let row = |id: &str, name: &str, alt: &[&str]| {
            let mut model = DiscoveredModel::simple(id);
            model.name = name.to_string();
            model.supported_efforts = vec![Effort::Low, Effort::High];
            model.alt_ids = alt.iter().map(|alt| alt.to_string()).collect();
            model
        };
        DiscoveredCatalog {
            models: vec![
                row("opus", "Opus 5.5", &["claude-opus-5-5"]),
                row(
                    "claude-fable-5-1[1m]",
                    "Fable 5.1",
                    &["fable", "claude-fable-5-1"],
                ),
            ],
            native_model: native_model.map(str::to_string),
            native_efforts: crate::adapter::models::NativeEfforts {
                global: None,
                by_model: vec![("claude-opus-5-5".to_string(), Effort::High)],
            },
            ..Default::default()
        }
    }

    #[test]
    fn a_member_saved_under_a_full_id_or_alias_sits_on_the_same_claude_row() {
        use crate::tui::model_picker::{ModelKey, current_key};
        let cwd = Path::new("/tmp/ws");
        let mut catalog = ModelCatalog::default();
        catalog.seed_catalog(BackendKind::Claude, cwd, claude_alias_catalog(None));

        for saved in ["opus", "claude-opus-5-5", "claude-opus-5-5[1m]"] {
            assert_eq!(
                catalog.effective_summary(BackendKind::Claude, Some(saved), None, cwd),
                Some("Opus 5.5 · high".to_string()),
                "{saved}"
            );
            assert_eq!(
                current_key(BackendKind::Claude, Some(saved), &catalog, cwd),
                Some(ModelKey {
                    backend: BackendKind::Claude,
                    model: "opus".to_string()
                }),
                "{saved}"
            );
        }
        for saved in ["fable", "claude-fable-5-1", "claude-fable-5-1[1m]"] {
            assert_eq!(
                catalog.effective_summary(BackendKind::Claude, Some(saved), None, cwd),
                Some("Fable 5.1".to_string()),
                "{saved}"
            );
            assert_eq!(
                current_key(BackendKind::Claude, Some(saved), &catalog, cwd).map(|key| key.model),
                Some("claude-fable-5-1[1m]".to_string()),
                "{saved}"
            );
        }

        // The CLI's own configured full id resolves the same way when unpinned.
        let mut native = ModelCatalog::default();
        native.seed_catalog(
            BackendKind::Claude,
            cwd,
            claude_alias_catalog(Some("claude-opus-5-5")),
        );
        assert_eq!(
            native.effective_summary(BackendKind::Claude, None, None, cwd),
            Some("Opus 5.5 · high".to_string())
        );
        assert_eq!(
            current_key(BackendKind::Claude, None, &native, cwd).map(|key| key.model),
            Some("opus".to_string())
        );
    }

    #[test]
    fn the_team_editor_model_picker_selects_the_row_for_an_alternate_spelling() {
        let catalog = claude_alias_catalog(None);
        let picker = ModelPicker::new(
            BackendKind::Claude,
            Some("claude-opus-5-5"),
            None,
            catalog.models,
        );
        assert_eq!(picker.options.len(), 2);
        assert_eq!(picker.selected(), 0);
    }

    #[test]
    fn effective_model_prefers_pinned_then_the_clis_own_then_the_catalog_default() {
        let cwd = Path::new("/tmp/ws");
        let mut catalog = ModelCatalog::default();
        catalog.seed_catalog(
            BackendKind::Codex,
            cwd,
            sol_catalog(Some(Effort::High), None),
        );

        // Nothing pinned, the CLI names no model: the catalog default, with
        // the CLI's configured effort ahead of the catalog's default effort.
        let eff = catalog.effective_model(BackendKind::Codex, None, None, cwd);
        assert_eq!(eff.id.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(eff.name.as_deref(), Some("GPT-6.1-Sol"));
        assert_eq!(eff.effort, Some(Effort::High));

        // No native effort at all: the model's own default effort.
        catalog.seed_catalog(BackendKind::Codex, cwd, sol_catalog(None, None));
        let eff = catalog.effective_model(BackendKind::Codex, None, None, cwd);
        assert_eq!(eff.effort, Some(Effort::Low));

        // The CLI's configured model outranks the catalog default.
        catalog.seed_catalog(
            BackendKind::Codex,
            cwd,
            sol_catalog(None, Some("gpt-6-astra")),
        );
        let eff = catalog.effective_model(BackendKind::Codex, None, None, cwd);
        assert_eq!(eff.id.as_deref(), Some("gpt-6-astra"));
        assert_eq!(eff.effort, Some(Effort::Medium));

        // Pinned values outrank both.
        let eff = catalog.effective_model(
            BackendKind::Codex,
            Some("gpt-6.1-sol"),
            Some(Effort::Medium),
            cwd,
        );
        assert_eq!(eff.id.as_deref(), Some("gpt-6.1-sol"));
        assert_eq!(eff.effort, Some(Effort::Medium));

        // A native effort the model does not support is skipped.
        let mut limited = sol_catalog(Some(Effort::Max), None);
        limited.models[0].supported_efforts = vec![Effort::Low, Effort::High];
        catalog.seed_catalog(BackendKind::Codex, cwd, limited);
        let eff = catalog.effective_model(BackendKind::Codex, None, None, cwd);
        assert_eq!(eff.effort, Some(Effort::Low));

        // Nothing loaded: nothing to say.
        let empty = ModelCatalog::default();
        let eff = empty.effective_model(BackendKind::Codex, None, None, cwd);
        assert_eq!((eff.id, eff.name, eff.effort), (None, None, None));
        assert_eq!(
            empty.effective_summary(BackendKind::Codex, None, None, cwd),
            None
        );
        assert_eq!(
            empty
                .effective_summary(BackendKind::Codex, None, Some(Effort::High), cwd)
                .as_deref(),
            Some("high")
        );
    }

    #[test]
    fn the_team_editor_label_and_the_picker_marker_agree_on_the_effective_model() {
        let cwd = Path::new("/tmp/ws");
        let mut catalog = ModelCatalog::default();
        // The CLI configures `high`; the catalog's own default effort is `low`.
        catalog.seed_catalog(
            BackendKind::Codex,
            cwd,
            sol_catalog(Some(Effort::High), None),
        );
        let member = TeamMember::new("main", "Main", BackendKind::Codex, "general");

        assert_eq!(catalog.model_label(&member, cwd), "GPT-6.1-Sol · high");
        assert_eq!(
            catalog
                .effective_summary(BackendKind::Codex, None, None, cwd)
                .as_deref(),
            Some("GPT-6.1-Sol · high")
        );
        let current =
            crate::tui::model_picker::current_key(BackendKind::Codex, None, &catalog, cwd)
                .expect("the effective model is a catalog entry");
        assert_eq!(current.model, "gpt-6.1-sol");

        // A pinned model shows the pinned name and still takes the CLI effort.
        let mut pinned = member.clone();
        pinned.model = Some("gpt-6-astra".to_string());
        assert_eq!(
            catalog.model_label(&pinned, cwd),
            "GPT-6-Astra · gpt-6-astra · high"
        );
        pinned.effort = Some(Effort::Medium);
        assert_eq!(
            catalog.model_label(&pinned, cwd),
            "GPT-6-Astra · gpt-6-astra · medium"
        );
    }

    #[test]
    fn model_label_shows_the_effective_effort_except_for_agy() {
        let cwd = Path::new("/tmp/ws");
        let mut catalog = ModelCatalog::default();
        catalog.seed(BackendKind::Grok, cwd, vec!["grok-4.6".to_string()]);
        let mut grok = TeamMember::new("grok", "Grok", BackendKind::Grok, "review");
        grok.model = Some("grok-4.6".to_string());
        grok.effort = Some(Effort::Xhigh);
        assert_eq!(catalog.model_label(&grok, cwd), "grok-4.6 · xhigh");

        catalog.seed(
            BackendKind::Agy,
            cwd,
            vec!["gemini-3.6-flash-high".to_string()],
        );
        let mut agy = TeamMember::new("agy", "Agy", BackendKind::Agy, "research");
        agy.model = Some("gemini-3.6-flash-high".to_string());
        assert_eq!(catalog.model_label(&agy, cwd), "gemini-3.6-flash-high");
    }

    #[test]
    fn catalog_and_picker_show_the_detected_default_model() {
        let mut catalog = ModelCatalog::default();
        catalog.seed(
            BackendKind::Claude,
            Path::new("/tmp/ws"),
            vec![
                "claude-sonnet-4-6".to_string(),
                "claude-opus-4-6".to_string(),
            ],
        );
        let member = TeamMember::new("builder", "Builder", BackendKind::Claude, "impl");
        let ModelChoices::Ready(models) = catalog.models(BackendKind::Claude, Path::new("/tmp/ws"))
        else {
            panic!("expected detected models");
        };
        let picker = ModelPicker::new(BackendKind::Claude, None, None, models);

        assert_eq!(
            catalog.model_label(&member, Path::new("/tmp/ws")),
            "claude-sonnet-4-6"
        );
        assert_eq!(picker.visible_len(), 2);
        assert_eq!(picker.value().as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(
            picker.selected_choice().map(ModelChoice::name),
            Some("claude-sonnet-4-6")
        );
    }

    #[test]
    fn loaded_catalog_without_a_reported_default_is_not_labeled_not_loaded() {
        let mut catalog = ModelCatalog::default();
        catalog.loads.insert(
            (BackendKind::Agy, PathBuf::from("/tmp/ws")),
            ModelLoad::Ready(Ok(DiscoveredCatalog {
                models: vec![DiscoveredModel::simple("gemini-3.6-pro")],
                native_permission: None,
                ..Default::default()
            })),
        );
        let member = TeamMember::new("planner", "Planner", BackendKind::Agy, "plan");

        assert_eq!(
            catalog.model_label(&member, Path::new("/tmp/ws")),
            "CLI default"
        );
        let ModelChoices::Ready(models) = catalog.models(BackendKind::Agy, Path::new("/tmp/ws"))
        else {
            panic!("expected the successful catalog to stay available to the picker");
        };
        assert_eq!(models[0].id, "gemini-3.6-pro");
    }

    #[test]
    fn model_picker_keeps_cli_default_only_when_no_models_are_discovered() {
        let picker = ModelPicker::new(BackendKind::Claude, None, None, Vec::new());

        assert_eq!(picker.visible_len(), 1);
        assert_eq!(picker.value(), None);
        assert_eq!(
            picker.selected_choice().map(ModelChoice::name),
            Some("default")
        );
    }

    #[test]
    fn catalog_shows_loading_instead_of_default_while_discovery_runs() {
        let (_tx, rx) = mpsc::channel();
        let mut catalog = ModelCatalog::default();
        catalog.loads.insert(
            (BackendKind::Codex, PathBuf::from("/tmp/ws")),
            ModelLoad::Loading(rx),
        );
        let member = TeamMember::new("builder", "Builder", BackendKind::Codex, "impl");

        assert_eq!(
            catalog.model_label(&member, Path::new("/tmp/ws")),
            "loading…"
        );
    }

    #[test]
    fn catalog_marks_failed_default_model_as_unavailable() {
        let mut catalog = ModelCatalog::default();
        catalog.loads.insert(
            (BackendKind::Grok, PathBuf::from("/tmp/ws")),
            ModelLoad::Ready(Err("network unavailable".to_string())),
        );
        let member = TeamMember::new("builder", "Builder", BackendKind::Grok, "impl");

        assert_eq!(
            catalog.model_label(&member, Path::new("/tmp/ws")),
            "unavailable"
        );
        assert!(
            catalog
                .backend_summary(BackendKind::Grok, Path::new("/tmp/ws"))
                .contains("model discovery failed")
        );
    }

    #[test]
    fn frozen_catalog_labels_a_new_member_as_explicitly_loadable() {
        let mut catalog = ModelCatalog::default();
        let cwd = Path::new("/tmp/new-member-workspace");
        catalog.freeze();
        let member = TeamMember::new("planner", "Planner", BackendKind::Agy, "plan");
        assert_eq!(
            catalog.model_label(&member, cwd),
            "not loaded · press Enter"
        );
        assert_eq!(
            catalog.backend_summary(BackendKind::Agy, cwd),
            "installed · select Model to load"
        );
    }

    #[test]
    fn session_fields_show_resume_freshing_and_bound_states() {
        let mut member = TeamMember::new("builder", "Builder", BackendKind::Codex, "impl");

        assert_eq!(field_value(&member, Field::Model), "default");
        assert_eq!(field_value(&member, Field::Session), "resume");
        assert_eq!(
            field_value(&member, Field::SessionId),
            "select a session · Enter to choose"
        );

        member.session_id = Some("thread-abc123".to_string());
        assert_eq!(field_value(&member, Field::Session), "bound");
        assert_eq!(field_value(&member, Field::SessionId), "thread-abc123");

        member.session_policy = SessionPolicy::Fresh;
        assert_eq!(field_value(&member, Field::Session), "bound");
        assert_eq!(field_value(&member, Field::SessionId), "thread-abc123");

        member.session_id = None;
        assert_eq!(field_value(&member, Field::Session), "fresh");
        assert_eq!(
            field_value(&member, Field::SessionId),
            "created on next send"
        );

        assert_eq!(
            session_status_label(SessionPolicy::Resume, None, true),
            "freshing"
        );
    }

    #[test]
    fn edit_state_moves_and_edits_unicode_at_the_cursor() {
        let mut edit = EditState::new(Field::Role, "你 model".to_string());
        edit.apply_key(KeyCode::Left, KeyModifiers::NONE);
        edit.apply_key(KeyCode::Left, KeyModifiers::NONE);
        edit.insert_text("好");
        assert_eq!(edit.buffer, "你 mod好el");

        edit.apply_key(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(edit.buffer, "el");
        assert_eq!(edit.cursor, 0);
        edit.apply_key(KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(edit.buffer, "l");
    }

    #[test]
    fn edit_state_windows_long_values_around_cursor() {
        let mut edit = EditState::new(Field::SessionId, "/very/long/session-id".to_string());
        edit.apply_key(KeyCode::Home, KeyModifiers::NONE);
        edit.apply_key(KeyCode::Right, KeyModifiers::NONE);
        let (visible, cursor) = edit.visible_window(8);
        assert!(theme::display_width(&visible) <= 8);
        assert_eq!(cursor, 1);
    }

    #[test]
    fn edit_state_moves_and_deletes_complete_graphemes() {
        let mut combining = EditState::new(Field::Role, "e\u{301}".to_string());
        combining.apply_key(KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(combining.cursor, 0);
        combining.apply_key(KeyCode::Right, KeyModifiers::NONE);
        assert_eq!(combining.cursor, 2);
        combining.apply_key(KeyCode::Backspace, KeyModifiers::NONE);
        assert!(combining.buffer.is_empty());

        let mut family = EditState::new(Field::Role, "👨‍👩‍👧‍👦".to_string());
        family.apply_key(KeyCode::Home, KeyModifiers::NONE);
        family.apply_key(KeyCode::Delete, KeyModifiers::NONE);
        assert!(family.buffer.is_empty());
    }
}
