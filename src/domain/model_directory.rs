//! The models each installed backend offers, as the runtime needs them: to
//! tell agents which ids they may use in `@@spawn`, and to turn the model an
//! agent asked for into an exact id (or a refusal that lists the valid ones).

use crate::adapter::models::DiscoveredModel;
use crate::domain::config::DetectedBackends;
use crate::domain::team::{BackendKind, Effort};

/// Effort levels from lowest to highest.
const EFFORT_ORDER: [Effort; 6] = [
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Xhigh,
    Effort::Max,
    Effort::Ultra,
];

/// Words that name a vendor rather than a model: `gpt-astra` is asking for
/// `astra`.
const VENDOR_WORDS: [&str; 4] = ["gpt", "claude", "grok", "gemini"];

/// What discovery has found so far. A backend without an entry has an unknown
/// catalog (discovery unfinished or failed): nothing is checked for it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ModelDirectory {
    /// Which backends are installed; `None` until detection has finished.
    pub installed: Option<DetectedBackends>,
    /// Each discovered catalog, newest model first as the backend lists them.
    pub catalogs: Vec<(BackendKind, Vec<DiscoveredModel>)>,
}

/// A model request resolved against a catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedModel {
    /// The id to run.
    pub id: String,
    /// The catalog's display name for it.
    pub name: String,
    pub supported_efforts: Vec<Effort>,
    /// Set when the agent's wording was not an exact id, or matched several
    /// models: what the parent should be told.
    pub remark: Option<String>,
}

impl ModelDirectory {
    pub fn models(&self, backend: BackendKind) -> Option<&[DiscoveredModel]> {
        self.catalogs
            .iter()
            .find(|(kind, _)| *kind == backend)
            .map(|(_, models)| models.as_slice())
    }

    /// Whether `backend` is known not to be installed.
    pub fn lacks(&self, backend: BackendKind) -> bool {
        self.installed
            .is_some_and(|installed| !installed.contains(backend))
    }

    /// The catalog row for `id` (an id, an alternate id), if the catalog is known.
    pub fn find(&self, backend: BackendKind, id: &str) -> Option<&DiscoveredModel> {
        self.models(backend)?
            .iter()
            .find(|model| model.matches_id(id))
    }

    /// The catalog's entry for the model `id` names, or the backend's default
    /// when no model is pinned; `None` when the catalog does not know it.
    pub fn effective(&self, backend: BackendKind, id: Option<&str>) -> Option<ResolvedModel> {
        let model = match id {
            Some(id) => self.find(backend, id)?,
            None => self.default_model(backend)?,
        };
        Some(ResolvedModel {
            id: id.unwrap_or(&model.id).to_string(),
            name: model.name.clone(),
            supported_efforts: model.supported_efforts.clone(),
            remark: None,
        })
    }

    /// The backend's default model, if its catalog marks one.
    pub fn default_model(&self, backend: BackendKind) -> Option<&DiscoveredModel> {
        self.models(backend)?.iter().find(|model| model.is_default)
    }

    /// Resolve the `model` of an `@@spawn` against `backend`'s catalog. `None`
    /// when that catalog is unknown (the request passes through unchanged).
    pub fn resolve(
        &self,
        backend: BackendKind,
        requested: &str,
    ) -> Option<Result<ResolvedModel, String>> {
        let models = self.models(backend)?;
        Some(resolve_in(models, backend, requested))
    }

    /// The compact "Models you can use in @@spawn" block for the protocol text
    /// of members that may spawn. `None` when no catalog is known.
    pub fn protocol_block(&self) -> Option<String> {
        let lines: Vec<String> = self
            .catalogs
            .iter()
            .filter(|(backend, models)| !models.is_empty() && !self.lacks(*backend))
            .map(|(backend, models)| {
                let entries: Vec<String> = models.iter().map(model_entry).collect();
                format!("{backend}: {}", entries.join(", "))
            })
            .collect();
        if lines.is_empty() {
            return None;
        }
        Some(format!(
            "Models you can use in @@spawn (use these exact ids; Asterline refuses unknown ones and \
             tells you the valid ids). When the user names a model loosely, such as \"astra\", pick \
             the matching id here:\n{}\n",
            lines.join("\n")
        ))
    }
}

impl ModelDirectory {
    /// The contents of `<asterline home>/skills/models.md`: every installed
    /// backend's models with the exact id to pass as `model`, the display
    /// name, and the efforts allowed. Backends come in a fixed order and
    /// models in catalog order, so the same directory always gives the same
    /// text. `None` when no catalog is known.
    pub fn models_file(&self) -> Option<String> {
        let sections: Vec<String> = BACKEND_ORDER
            .into_iter()
            .filter(|backend| !self.lacks(*backend))
            .filter_map(|backend| {
                let models = self.models(backend).filter(|models| !models.is_empty())?;
                let lines: Vec<String> = models.iter().map(file_entry).collect();
                Some(format!("## {backend}\n\n{}\n", lines.join("\n")))
            })
            .collect();
        if sections.is_empty() {
            return None;
        }
        Some(format!(
            "# Models Asterline can open with @@spawn\n\n\
             Kept up to date by Asterline; it is rewritten whenever the installed models change.\n\n\
             Pass the exact id (the part in backticks) as \"model\", and the section heading as \"backend\". \
             \"backend\", \"model\", and \"effort\" are optional and default to the parent's. Asterline refuses an unknown model or an effort the model does not offer.\n\n{}",
            sections.join("\n")
        ))
    }
}

/// The order backends appear in the model list file.
const BACKEND_ORDER: [BackendKind; 4] = [
    BackendKind::Codex,
    BackendKind::Claude,
    BackendKind::Grok,
    BackendKind::Agy,
];

/// `- `id` — Display Name; efforts: low, medium (default: low)`.
fn file_entry(model: &DiscoveredModel) -> String {
    let mut details = Vec::new();
    if !model.name.is_empty() && model.name != model.id {
        details.push(model.name.clone());
    }
    if model.is_default {
        details.push("backend default".to_string());
    }
    let efforts = effort_list(&model.supported_efforts);
    if !efforts.is_empty() {
        let mut text = format!("efforts: {efforts}");
        if let Some(default) = model.default_effort {
            text.push_str(&format!(" (default: {})", default.as_str()));
        }
        details.push(text);
    }
    if details.is_empty() {
        format!("- `{}`", model.id)
    } else {
        format!("- `{}` — {}", model.id, details.join("; "))
    }
}

/// `id (Display Name; low…max)`, leaving out what is unknown or the same as
/// the id.
fn model_entry(model: &DiscoveredModel) -> String {
    let mut details = Vec::new();
    if !model.name.is_empty() && model.name != model.id {
        details.push(model.name.clone());
    }
    if let Some(efforts) = effort_range(&model.supported_efforts) {
        details.push(efforts);
    }
    if details.is_empty() {
        model.id.clone()
    } else {
        format!("{} ({})", model.id, details.join("; "))
    }
}

/// `low…ultra` for an unbroken run of levels, else the levels joined by commas.
pub fn effort_range(efforts: &[Effort]) -> Option<String> {
    let mut ordered: Vec<Effort> = EFFORT_ORDER
        .into_iter()
        .filter(|effort| efforts.contains(effort))
        .collect();
    ordered.dedup();
    let (first, last) = (*ordered.first()?, *ordered.last()?);
    if ordered.len() == 1 {
        return Some(first.as_str().to_string());
    }
    let position = |effort: Effort| EFFORT_ORDER.iter().position(|e| *e == effort);
    let contiguous = position(last)? - position(first)? + 1 == ordered.len();
    if contiguous {
        Some(format!("{}…{}", first.as_str(), last.as_str()))
    } else {
        Some(
            ordered
                .iter()
                .map(|effort| effort.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// The efforts as a list for a refusal message.
pub fn effort_list(efforts: &[Effort]) -> String {
    EFFORT_ORDER
        .into_iter()
        .filter(|effort| efforts.contains(effort))
        .map(Effort::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Refusal text for an effort the model does not offer; `None` when it does
/// (or when the model's levels are not known).
pub fn effort_refusal(model: &ResolvedModel, effort: Effort) -> Option<String> {
    if model.supported_efforts.is_empty() || model.supported_efforts.contains(&effort) {
        return None;
    }
    Some(format!(
        "{} does not support {} effort (supported: {})",
        model.id,
        effort.as_str(),
        effort_list(&model.supported_efforts)
    ))
}

fn resolve_in(
    models: &[DiscoveredModel],
    backend: BackendKind,
    requested: &str,
) -> Result<ResolvedModel, String> {
    let request = requested.trim();
    let resolved = |model: &DiscoveredModel, id: String, remark: Option<String>| ResolvedModel {
        id,
        name: model.name.clone(),
        supported_efforts: model.supported_efforts.clone(),
        remark,
    };

    // 1. An exact id, or another id of the same model. It keeps the spelling
    // it was given (the CLI accepts both, and `[1m]` stays).
    if let Some(model) = models.iter().find(|model| model.matches_id(request)) {
        return Ok(resolved(model, request.to_string(), None));
    }

    // 2. The display name, ignoring case, spaces, hyphens, and dots.
    let squashed = squash(request);
    if !squashed.is_empty()
        && let Some(model) = models
            .iter()
            .find(|model| squash(&model.name) == squashed || squash(&model.id) == squashed)
    {
        return Ok(resolved(model, model.id.clone(), None));
    }

    // 3. Every word of the request appears in the id or display name.
    let words = words_of(request);
    let mut matches = loose_matches(models, &words);
    if matches.is_empty()
        && words.len() > 1
        && words
            .first()
            .is_some_and(|word| VENDOR_WORDS.contains(&word.as_str()))
    {
        matches = loose_matches(models, &words[1..]);
    }
    match matches.as_slice() {
        [] => Err(format!(
            "{backend} has no model matching \"{request}\". Valid {backend} models: {}",
            valid_models(models)
        )),
        [only] => Ok(resolved(only, only.id.clone(), None)),
        [first, rest @ ..] => {
            let others: Vec<&str> = rest.iter().take(4).map(|model| model.id.as_str()).collect();
            let remark = format!(
                "\"{request}\" matches several {backend} models; used the first, {} (others: {}). \
                 Use an exact id to choose another.",
                first.id,
                others.join(", ")
            );
            Ok(resolved(first, first.id.clone(), Some(remark)))
        }
    }
}

fn valid_models(models: &[DiscoveredModel]) -> String {
    models
        .iter()
        .map(|model| {
            if model.name.is_empty() || model.name == model.id {
                model.id.clone()
            } else {
                format!("{} ({})", model.id, model.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Lowercase with spaces, hyphens, dots, and underscores removed.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|ch| !matches!(ch, ' ' | '-' | '.' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

fn words_of(text: &str) -> Vec<String> {
    text.split(['-', '_', ' ', '.'])
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Models whose id, alternate ids, or display name contain every word: as whole
/// words first, and only when none do, as pieces of words.
fn loose_matches<'a>(models: &'a [DiscoveredModel], words: &[String]) -> Vec<&'a DiscoveredModel> {
    if words.is_empty() {
        return Vec::new();
    }
    let texts = |model: &DiscoveredModel| -> String {
        std::iter::once(model.id.as_str())
            .chain(std::iter::once(model.name.as_str()))
            .chain(model.alt_ids.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let whole: Vec<&DiscoveredModel> = models
        .iter()
        .filter(|model| {
            let have = words_of(&texts(model));
            words.iter().all(|word| have.contains(word))
        })
        .collect();
    if !whole.is_empty() {
        return whole;
    }
    models
        .iter()
        .filter(|model| {
            let text = texts(model);
            words.iter().all(|word| text.contains(word.as_str()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(id: &str, name: &str, efforts: &[Effort]) -> DiscoveredModel {
        let mut model = DiscoveredModel::simple(id);
        model.name = name.to_string();
        model.supported_efforts = efforts.to_vec();
        model
    }

    const ALL: [Effort; 5] = [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::Xhigh,
        Effort::Max,
    ];

    fn codex() -> ModelDirectory {
        let mut ultra = ALL.to_vec();
        ultra.push(Effort::Ultra);
        ModelDirectory {
            installed: None,
            catalogs: vec![(
                BackendKind::Codex,
                vec![
                    model("gpt-6.1-sol", "GPT-6.1-Sol", &ultra),
                    model("gpt-6-astra", "GPT-6-Astra", &ultra),
                    model("gpt-6.1-terra", "GPT-6.1-Terra", &ALL),
                    model("gpt-6-sol", "GPT-6-Sol", &ALL),
                    model("gpt-reserve", "gpt-reserve", &[]),
                ],
            )],
        }
    }

    fn resolve(request: &str) -> Result<ResolvedModel, String> {
        codex().resolve(BackendKind::Codex, request).unwrap()
    }

    #[test]
    fn exact_id_and_display_name_resolve_without_a_remark() {
        let exact = resolve("gpt-6-astra").unwrap();
        assert_eq!((exact.id.as_str(), exact.remark), ("gpt-6-astra", None));
        let display = resolve("GPT 6 Astra").unwrap();
        assert_eq!(display.id, "gpt-6-astra");
        assert_eq!(resolve("gpt6.1sol").unwrap().id, "gpt-6.1-sol");
    }

    #[test]
    fn alternate_id_keeps_the_spelling_it_was_given() {
        let mut opus = model("opus", "Opus", &ALL);
        opus.alt_ids = vec!["claude-opus-4-8".to_string()];
        let directory = ModelDirectory {
            installed: None,
            catalogs: vec![(BackendKind::Claude, vec![opus])],
        };
        let resolved = directory
            .resolve(BackendKind::Claude, "claude-opus-4-8[1m]")
            .unwrap()
            .unwrap();
        assert_eq!(resolved.id, "claude-opus-4-8[1m]");
        assert_eq!(resolved.name, "Opus");
    }

    #[test]
    fn loose_unique_match_names_the_exact_id() {
        let resolved = resolve("gpt-astra").unwrap();
        assert_eq!(resolved.id, "gpt-6-astra");
        assert_eq!(resolved.name, "GPT-6-Astra");
        assert!(resolved.remark.is_none());
        assert_eq!(resolve("astra").unwrap().id, "gpt-6-astra");
        assert_eq!(resolve("ast").unwrap().id, "gpt-6-astra", "piece of a word");
    }

    #[test]
    fn ambiguous_request_uses_the_first_model_and_says_so() {
        let resolved = resolve("sol").unwrap();
        assert_eq!(resolved.id, "gpt-6.1-sol");
        let remark = resolved.remark.unwrap();
        assert!(
            remark.contains("gpt-6.1-sol") && remark.contains("gpt-6-sol"),
            "{remark}"
        );
    }

    #[test]
    fn leading_vendor_word_is_dropped_only_when_needed() {
        let mut directory = codex();
        directory.catalogs[0].1.push(model("luna", "Luna", &ALL));
        let resolved = directory
            .resolve(BackendKind::Codex, "gpt-luna")
            .unwrap()
            .unwrap();
        assert_eq!(resolved.id, "luna");
    }

    #[test]
    fn unknown_model_lists_the_valid_ids() {
        let err = resolve("banana").unwrap_err();
        assert!(err.contains("gpt-6-astra (GPT-6-Astra)"), "{err}");
        assert!(err.contains("gpt-reserve"), "{err}");
    }

    #[test]
    fn unknown_catalog_passes_through() {
        assert!(codex().resolve(BackendKind::Grok, "whatever").is_none());
    }

    #[test]
    fn effort_check_names_the_supported_levels() {
        let sol = resolve("gpt-6-sol").unwrap();
        let err = effort_refusal(&sol, Effort::Ultra).unwrap();
        assert!(err.contains("low, medium, high, xhigh, max"), "{err}");
        assert!(effort_refusal(&sol, Effort::Max).is_none());
        let unknown = resolve("gpt-reserve").unwrap();
        assert!(effort_refusal(&unknown, Effort::Ultra).is_none());
    }

    #[test]
    fn protocol_block_lists_ids_names_and_effort_ranges() {
        let block = codex().protocol_block().unwrap();
        assert!(
            block.contains(
                "codex: gpt-6.1-sol (GPT-6.1-Sol; low…ultra), gpt-6-astra (GPT-6-Astra; low…ultra), \
                 gpt-6.1-terra (GPT-6.1-Terra; low…max)"
            ),
            "{block}"
        );
        assert!(block.contains("gpt-reserve"), "{block}");
        assert!(block.contains("use these exact ids"), "{block}");
        assert_eq!(
            effort_range(&[Effort::Low, Effort::High]).as_deref(),
            Some("low, high")
        );
        assert_eq!(effort_range(&[Effort::Max]).as_deref(), Some("max"));
        assert!(ModelDirectory::default().protocol_block().is_none());
    }

    #[test]
    fn uninstalled_backends_are_not_listed() {
        let mut directory = codex();
        directory.installed = Some(DetectedBackends {
            codex: false,
            claude: true,
            grok: false,
            agy: false,
        });
        assert!(directory.lacks(BackendKind::Codex));
        assert!(directory.protocol_block().is_none());
    }

    #[test]
    fn the_models_file_lists_installed_backends_in_a_fixed_order() {
        let mut luna = model("luna", "luna", &[Effort::Low, Effort::High]);
        luna.default_effort = Some(Effort::High);
        luna.is_default = true;
        let directory = ModelDirectory {
            installed: Some(DetectedBackends {
                codex: true,
                claude: true,
                grok: false,
                agy: true,
            }),
            // Arrival order differs from the file's order; grok is not installed.
            catalogs: vec![
                (BackendKind::Agy, vec![model("flash", "Flash", &[])]),
                (BackendKind::Grok, vec![model("grok-4", "Grok 4", &[])]),
                (BackendKind::Claude, vec![model("opus", "Opus", &ALL)]),
                (
                    BackendKind::Codex,
                    vec![model("gpt-6-astra", "GPT-6-Astra", &ALL), luna],
                ),
            ],
        };
        let text = directory.models_file().unwrap();
        assert_eq!(
            text,
            "# Models Asterline can open with @@spawn\n\n\
             Kept up to date by Asterline; it is rewritten whenever the installed models change.\n\n\
             Pass the exact id (the part in backticks) as \"model\", and the section heading as \"backend\". \
             \"backend\", \"model\", and \"effort\" are optional and default to the parent's. \
             Asterline refuses an unknown model or an effort the model does not offer.\n\n\
             ## codex\n\n\
             - `gpt-6-astra` — GPT-6-Astra; efforts: low, medium, high, xhigh, max\n\
             - `luna` — backend default; efforts: low, high (default: high)\n\n\
             ## claude\n\n\
             - `opus` — Opus; efforts: low, medium, high, xhigh, max\n\n\
             ## agy\n\n\
             - `flash` — Flash\n"
        );
        // Deterministic, and nothing to say without a catalog.
        assert_eq!(directory.models_file(), Some(text));
        assert_eq!(ModelDirectory::default().models_file(), None);
    }
}
