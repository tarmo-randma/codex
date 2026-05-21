use std::collections::BTreeMap;
use std::path::PathBuf;

pub const CODEX_TRACE_ENV: &str = "CODEX_TRACE";
pub const CODEX_TRACE_DIR_ENV: &str = "CODEX_TRACE_DIR";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceConfig {
    enabled: bool,
    trace_dir: Option<PathBuf>,
    categories: TraceCategories,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceCategories {
    pub model: bool,
    pub tools: bool,
    pub usage: bool,
    pub subagents: bool,
    pub config: bool,
}

impl TraceConfig {
    pub fn from_env() -> Self {
        Self::from_env_map(std::env::vars())
    }

    pub fn from_env_map<K, V, I>(vars: I) -> Self
    where
        K: Into<String>,
        V: Into<String>,
        I: IntoIterator<Item = (K, V)>,
    {
        let vars = vars
            .into_iter()
            .map(|(key, value)| (key.into(), value.into()))
            .collect::<BTreeMap<_, _>>();
        let enabled = vars
            .get(CODEX_TRACE_ENV)
            .is_some_and(|value| matches!(value.as_str(), "1" | "true"));
        let trace_dir = vars
            .get(CODEX_TRACE_DIR_ENV)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Self {
            enabled,
            trace_dir,
            categories: TraceCategories::from_enabled(enabled),
        }
    }

    pub fn disabled() -> Self {
        Self {
            enabled: false,
            trace_dir: None,
            categories: TraceCategories::from_enabled(false),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn trace_dir(&self) -> Option<&PathBuf> {
        self.trace_dir.as_ref()
    }

    pub fn categories(&self) -> &TraceCategories {
        &self.categories
    }

    pub fn model_enabled(&self) -> bool {
        self.categories.model
    }

    pub fn tools_enabled(&self) -> bool {
        self.categories.tools
    }

    pub fn usage_enabled(&self) -> bool {
        self.categories.usage
    }

    pub fn subagents_enabled(&self) -> bool {
        self.categories.subagents
    }

    pub fn config_enabled(&self) -> bool {
        self.categories.config
    }
}

impl TraceCategories {
    fn from_enabled(enabled: bool) -> Self {
        Self {
            model: enabled,
            tools: enabled,
            usage: enabled,
            subagents: enabled,
            config: enabled,
        }
    }
}
