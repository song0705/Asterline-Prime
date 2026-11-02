//! The model list kept between runs: `<asterline home>/models.json`.
//!
//! Asterline loads it first at launch so the model picker, the `/team` editor,
//! and the model list sent to the runtime are usable before any CLI has been
//! asked. Discovery still runs in the background afterwards; the file is only
//! rewritten when its result differs. A missing, unreadable, malformed, or
//! older/newer-schema file is ignored, exactly as if there were no cache.

use std::io;
use std::path::Path;

use serde_json::{Map, Value, json};

use crate::adapter::models::{DiscoveredCatalog, DiscoveredModel, NativeEfforts};
use crate::domain::config::DetectedBackends;
use crate::domain::team::{BackendKind, Effort};

/// Bump when the layout changes; a file with another version is ignored.
pub const CACHE_VERSION: u64 = 1;

const BACKEND_ORDER: [BackendKind; 4] = [
    BackendKind::Codex,
    BackendKind::Claude,
    BackendKind::Grok,
    BackendKind::Agy,
];

/// What was last discovered: which CLIs were installed and each one's catalog.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ModelCache {
    /// `None` when detection had not finished.
    pub(crate) installed: Option<DetectedBackends>,
    /// In the fixed backend order (codex, claude, grok, agy).
    pub(crate) catalogs: Vec<(BackendKind, DiscoveredCatalog)>,
}

impl ModelCache {
    /// Keep `catalogs` in the fixed order, one per backend, so equal contents
    /// always compare (and serialize) equal.
    pub(crate) fn new(
        installed: Option<DetectedBackends>,
        mut catalogs: Vec<(BackendKind, DiscoveredCatalog)>,
    ) -> Self {
        catalogs.retain(|(backend, _)| installed.is_none_or(|found| found.contains(*backend)));
        catalogs.sort_by_key(|(backend, _)| order(*backend));
        catalogs.dedup_by_key(|(backend, _)| *backend);
        Self {
            installed,
            catalogs,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.installed.is_none() && self.catalogs.is_empty()
    }

    pub(crate) fn to_json(&self) -> String {
        let installed = self.installed.map(|found| {
            json!({
                "codex": found.codex,
                "claude": found.claude,
                "grok": found.grok,
                "agy": found.agy,
            })
        });
        let mut catalogs = Map::new();
        for (backend, catalog) in &self.catalogs {
            catalogs.insert(backend.as_str().to_string(), catalog_to_json(catalog));
        }
        let mut text = serde_json::to_string_pretty(&json!({
            "version": CACHE_VERSION,
            "installed": installed,
            "catalogs": catalogs,
        }))
        .unwrap_or_default();
        text.push('\n');
        text
    }

    /// `None` for anything that is not a current-version cache file.
    pub(crate) fn from_json(text: &str) -> Option<Self> {
        let root: Value = serde_json::from_str(text).ok()?;
        if root.get("version")?.as_u64()? != CACHE_VERSION {
            return None;
        }
        let installed = match root.get("installed")? {
            Value::Null => None,
            found => Some(DetectedBackends {
                codex: found.get("codex")?.as_bool()?,
                claude: found.get("claude")?.as_bool()?,
                grok: found.get("grok")?.as_bool()?,
                agy: found.get("agy")?.as_bool()?,
            }),
        };
        let mut catalogs = Vec::new();
        for (name, value) in root.get("catalogs")?.as_object()? {
            let backend = BackendKind::try_from(name.as_str()).ok()?;
            catalogs.push((backend, catalog_from_json(value)?));
        }
        Some(Self::new(installed, catalogs))
    }

    /// The cache stored at `path`, if there is a usable one.
    pub(crate) fn load(path: &Path) -> Option<Self> {
        let text = crate::fs_safety::read_regular_to_string(path, "model cache").ok()?;
        Self::from_json(&text)
    }

    /// Write the cache atomically; an identical file is left alone. Returns
    /// whether the file changed.
    pub(crate) fn save(&self, path: &Path) -> io::Result<bool> {
        crate::state_paths::write_state_file(path, "model cache", &self.to_json())
    }
}

fn order(backend: BackendKind) -> usize {
    BACKEND_ORDER
        .iter()
        .position(|candidate| *candidate == backend)
        .unwrap_or(BACKEND_ORDER.len())
}

fn catalog_to_json(catalog: &DiscoveredCatalog) -> Value {
    let by_model: Vec<Value> = catalog
        .native_efforts
        .by_model
        .iter()
        .map(|(id, effort)| json!([id, effort.as_str()]))
        .collect();
    json!({
        "models": catalog.models.iter().map(model_to_json).collect::<Vec<_>>(),
        "native_permission": catalog.native_permission,
        "native_model": catalog.native_model,
        "native_efforts": {
            "global": catalog.native_efforts.global.map(Effort::as_str),
            "by_model": by_model,
        },
    })
}

fn model_to_json(model: &DiscoveredModel) -> Value {
    json!({
        "id": model.id,
        "name": model.name,
        "description": model.description,
        "default_effort": model.default_effort.map(Effort::as_str),
        "supported_efforts": model
            .supported_efforts
            .iter()
            .map(|effort| effort.as_str())
            .collect::<Vec<_>>(),
        "is_default": model.is_default,
        "alt_ids": model.alt_ids,
    })
}

fn catalog_from_json(value: &Value) -> Option<DiscoveredCatalog> {
    let models = value
        .get("models")?
        .as_array()?
        .iter()
        .map(model_from_json)
        .collect::<Option<Vec<_>>>()?;
    let efforts = value.get("native_efforts")?;
    let by_model = efforts
        .get("by_model")?
        .as_array()?
        .iter()
        .map(|pair| {
            let pair = pair.as_array()?;
            Some((
                pair.first()?.as_str()?.to_string(),
                Effort::parse(pair.get(1)?.as_str()?)?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(DiscoveredCatalog {
        models,
        native_permission: optional_string(value, "native_permission")?,
        native_model: optional_string(value, "native_model")?,
        native_efforts: NativeEfforts {
            global: optional_effort(efforts, "global")?,
            by_model,
        },
    })
}

fn model_from_json(value: &Value) -> Option<DiscoveredModel> {
    Some(DiscoveredModel {
        id: value.get("id")?.as_str()?.to_string(),
        name: value.get("name")?.as_str()?.to_string(),
        description: optional_string(value, "description")?,
        default_effort: optional_effort(value, "default_effort")?,
        supported_efforts: value
            .get("supported_efforts")?
            .as_array()?
            .iter()
            .map(|effort| Effort::parse(effort.as_str()?))
            .collect::<Option<Vec<_>>>()?,
        is_default: value.get("is_default")?.as_bool()?,
        alt_ids: value
            .get("alt_ids")?
            .as_array()?
            .iter()
            .map(|id| id.as_str().map(str::to_string))
            .collect::<Option<Vec<_>>>()?,
    })
}

/// `Some(None)` for null, `None` for a value of the wrong type or a missing key.
fn optional_string(value: &Value, key: &str) -> Option<Option<String>> {
    match value.get(key)? {
        Value::Null => Some(None),
        Value::String(text) => Some(Some(text.clone())),
        _ => None,
    }
}

fn optional_effort(value: &Value, key: &str) -> Option<Option<Effort>> {
    match value.get(key)? {
        Value::Null => Some(None),
        Value::String(text) => Some(Some(Effort::parse(text)?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample() -> ModelCache {
        let mut sol = DiscoveredModel::simple("gpt-6.1-sol");
        sol.name = "GPT-6.1-Sol".to_string();
        sol.description = Some("Fast".to_string());
        sol.default_effort = Some(Effort::Medium);
        sol.supported_efforts = vec![Effort::Low, Effort::Medium, Effort::High];
        sol.is_default = true;
        let mut opus = DiscoveredModel::simple("opus");
        opus.alt_ids = vec!["claude-opus-4-8".to_string()];
        ModelCache::new(
            Some(DetectedBackends {
                codex: true,
                claude: true,
                grok: false,
                agy: false,
            }),
            vec![
                (
                    BackendKind::Claude,
                    DiscoveredCatalog {
                        models: vec![opus],
                        native_efforts: NativeEfforts {
                            global: Some(Effort::High),
                            by_model: vec![("opus".to_string(), Effort::Max)],
                        },
                        ..DiscoveredCatalog::default()
                    },
                ),
                (
                    BackendKind::Codex,
                    DiscoveredCatalog {
                        models: vec![sol],
                        native_permission: Some("never".to_string()),
                        native_model: Some("gpt-6.1-sol".to_string()),
                        ..DiscoveredCatalog::default()
                    },
                ),
            ],
        )
    }

    #[test]
    fn a_cache_survives_a_round_trip_in_backend_order() {
        let cache = sample();
        assert_eq!(cache.catalogs[0].0, BackendKind::Codex, "codex sorts first");
        assert_eq!(ModelCache::from_json(&cache.to_json()), Some(cache));
    }

    #[test]
    fn uninstalled_backends_are_left_out() {
        let mut cache = sample();
        cache.installed = Some(DetectedBackends {
            codex: false,
            claude: true,
            grok: false,
            agy: false,
        });
        let cache = ModelCache::new(cache.installed, cache.catalogs);
        assert_eq!(cache.catalogs.len(), 1);
        assert_eq!(cache.catalogs[0].0, BackendKind::Claude);
    }

    #[test]
    fn anything_but_a_current_cache_is_ignored() {
        assert_eq!(ModelCache::from_json(""), None);
        assert_eq!(ModelCache::from_json("not json"), None);
        assert_eq!(ModelCache::from_json("[]"), None);
        let current = sample().to_json();
        for old in ["\"version\": 0", "\"version\": 2"] {
            let other = current.replace("\"version\": 1", old);
            assert_eq!(ModelCache::from_json(&other), None, "{old}");
        }
        let damaged = current.replace("\"supported_efforts\"", "\"supported\"");
        assert_eq!(ModelCache::from_json(&damaged), None);
        let unknown = current.replace("\"codex\": {", "\"nonesuch\": {");
        assert_eq!(ModelCache::from_json(&unknown), None);
    }

    #[test]
    fn saving_rewrites_only_a_changed_cache() {
        let dir = std::env::temp_dir().join(format!("asterline-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("models.json");
        let cache = sample();
        assert!(cache.save(&path).unwrap(), "first write");
        assert_eq!(ModelCache::load(&path), Some(cache.clone()));
        assert!(!cache.save(&path).unwrap(), "identical: untouched");
        let mut next = cache;
        next.catalogs[0].1.models[0].id = "gpt-7".to_string();
        assert!(next.save(&path).unwrap());
        assert_eq!(ModelCache::load(&path), Some(next));
        std::fs::write(&path, "{broken").unwrap();
        assert_eq!(ModelCache::load(&path), None);
        std::fs::remove_dir_all(dir).ok();
    }
}
