//! Per-user default settings, loaded from `~/.kres/settings.json`,
//! optionally overlaid by a project-local `<project>/.kres/settings.json`.
//!
//! Schema:
//!
//! ```json
//! {
//!   "models": {
//!     "fast": "anthropic.json:claude-sonnet-5",
//!     "slow": "anthropic.json:claude-opus-4-8",
//!     "slow_secondary": "openai.json:gpt-5.4",
//!     "main": "anthropic.json:claude-sonnet-5",
//!     "todo": "anthropic.json:claude-sonnet-5",
//!     "classifier": "anthropic.json:claude-haiku-4-5"
//!   },
//!   "model_aliases": {
//!     "sonnet": "anthropic.json:claude-sonnet-5",
//!     "opus": "anthropic.json:claude-opus-4-8"
//!   },
//!   "actions": {
//!     "allowed": ["grep", "find", "read", "git", "edit", "bash"]
//!   }
//! }
//! ```
//!
//! Precedence when picking a model for an agent role:
//! Settings and CLI flags choose a model selector. Loading the selected
//! provider config resolves that selector to the concrete API model id.
//!
//! Precedence for the action allowlist:
//!   1. CLI `--allow <action>` flags (additive on top of the list
//!      below — an operator saying `--allow bash` gets bash for this
//!      session regardless of what the files say);
//!   2. project `<cwd>/.kres/settings.json` `actions.allowed` if set;
//!   3. global `~/.kres/settings.json` `actions.allowed` if set;
//!   4. `DEFAULT_ALLOWED_ACTIONS` (grep/find/read/git/edit/make/meson/cargo — bash is
//!      excluded by default because operators report it gets used as
//!      a general escape hatch for things the typed tools already
//!      handle).
//!
//! A missing or empty settings.json file is not an error — every
//! field is optional and the default struct just returns None from
//! every lookup. Distinct from this: an empty `actions.allowed`
//! array in a PRESENT file (`{"actions":{"allowed":[]}}`) is the
//! explicit "deny every non-MCP action" signal and the dispatcher
//! enforces it — see precedence list above.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use kres_llm::Model;

/// Action types (non-MCP dispatch) that are allowed when no
/// settings.json has spoken and the operator didn't pass --allow.
/// Bash is deliberately excluded — operators report it being used
/// as an escape hatch for things the typed tools already cover
/// (`bash sed` for range reads, `bash find` for file locates).
/// Coding flows that genuinely need `cc && ./repro` can opt in via
/// `--allow bash` or via settings.actions.allowed.
pub const DEFAULT_ALLOWED_ACTIONS: &[&str] = &[
    "grep", "find", "read", "git", "edit", "make", "meson", "cargo",
];

/// Every action type the main agent might emit. Used for typo
/// detection when an operator writes `--allow bsah` or sticks
/// `"fnid"` in `actions.allowed`. Keep in sync with the `match ty
/// { ... }` arms in `kres_agents::main_agent::dispatch_non_mcp`
/// PLUS the separately-routed `"mcp"` action. `"mcp"` is listed so
/// `--allow mcp` doesn't false-positive as a typo; the allowlist
/// gate in dispatch_non_mcp never consults the `"mcp"` entry (MCP
/// actions are gated by mcp.json server registration, not this
/// list), so including it here is effectively documentation.
pub const KNOWN_ACTION_TYPES: &[&str] = &[
    "grep", "find", "read", "git", "edit", "bash", "make", "meson", "cargo", "mcp",
];

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub models: Models,
    #[serde(default)]
    pub model_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub actions: ActionSettings,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ActionSettings {
    /// Explicit allowlist. When `Some`, replaces the built-in
    /// `DEFAULT_ALLOWED_ACTIONS` entirely. When `None`, the default
    /// list is used. Project-local settings.json replaces the global
    /// list rather than unioning — the usual "more specific config
    /// wins" behaviour.
    #[serde(default)]
    pub allowed: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Models {
    #[serde(default)]
    pub fast: Option<String>,
    #[serde(default)]
    pub slow: Option<String>,
    /// Optional second review model. It runs the supplemental lens only:
    /// `general` for /review and `maintainer` for /fix.
    #[serde(default)]
    pub slow_secondary: Option<String>,
    #[serde(default)]
    pub main: Option<String>,
    #[serde(default)]
    pub todo: Option<String>,
    #[serde(default)]
    pub classifier: Option<String>,
}

/// Which agent we're resolving a model for. Matches the per-role
/// keys in `Models`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRole {
    Fast,
    Slow,
    Main,
    Todo,
    Classifier,
}

impl Settings {
    /// Default on-disk path: `$HOME/.kres/settings.json`. Returns
    /// None when `$HOME` is unset.
    pub fn default_path() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".kres").join("settings.json"))
    }

    /// Load from an explicit path. Missing or empty file returns
    /// `Settings::default()` so callers never have to care whether
    /// the operator has populated it yet.
    pub fn load_from(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => s,
            _ => return Settings::default(),
        };
        match serde_json::from_str::<Settings>(&raw) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("settings: parse error in {}: {e}; ignoring", path.display());
                Settings::default()
            }
        }
    }

    /// Load from `~/.kres/settings.json` when `$HOME` is set,
    /// otherwise return defaults.
    pub fn load_default() -> Self {
        match Self::default_path() {
            Some(p) => Self::load_from(&p),
            None => Settings::default(),
        }
    }

    /// Load the global settings and overlay per-project settings
    /// from `<project_root>/.kres/settings.json`. Project values
    /// take precedence field-by-field:
    ///   - `models.*`: project's Some replaces global's Some;
    ///     project's None leaves the global value in place.
    ///   - `model_aliases`: project entries replace global entries by name;
    ///   - `actions.allowed`: project's Some REPLACES global's Some
    ///     (allowlists don't union — the more specific config wins).
    ///
    /// A missing project settings file is not an error.
    pub fn load_merged(project_root: &Path) -> Self {
        let proj_path = project_root.join(".kres").join("settings.json");
        Self::load_merged_with_paths(Self::default_path().as_deref(), &proj_path)
    }

    /// Testable core of `load_merged`. `global` is the path to
    /// `~/.kres/settings.json` (or `None` when `$HOME` isn't set);
    /// `project` is the path to the per-project overrides. Public
    /// so the test suite can exercise the merge without having to
    /// mock the operator's real home directory.
    pub fn load_merged_with_paths(global: Option<&Path>, project: &Path) -> Self {
        let mut s = match global {
            Some(p) => Self::load_from(p),
            None => Self::default(),
        };
        let proj = Self::load_from(project);
        s.apply_project_overrides(proj);
        s
    }

    /// Field-by-field overlay: project's Some values replace the
    /// global ones (where "global" is `self`). Exposed separately
    /// so tests can drive the merge from in-memory values without
    /// touching the filesystem.
    pub fn apply_project_overrides(&mut self, proj: Settings) {
        if proj.models.fast.is_some() {
            self.models.fast = proj.models.fast;
        }
        if proj.models.slow.is_some() {
            self.models.slow = proj.models.slow;
        }
        if proj.models.slow_secondary.is_some() {
            self.models.slow_secondary = proj.models.slow_secondary;
        }
        if proj.models.main.is_some() {
            self.models.main = proj.models.main;
        }
        if proj.models.todo.is_some() {
            self.models.todo = proj.models.todo;
        }
        if proj.models.classifier.is_some() {
            self.models.classifier = proj.models.classifier;
        }
        self.model_aliases.extend(proj.model_aliases);
        if proj.actions.allowed.is_some() {
            self.actions.allowed = proj.actions.allowed;
        }
    }

    /// Warn on stderr for any token in `cli_extras` or
    /// `self.actions.allowed` that isn't a recognised action type
    /// (e.g. typo `bsah` for `bash`). The special CLI token `"all"`
    /// is exempt. The warning includes a closest-match suggestion
    /// when the distance is small. Returns the number of warnings.
    pub fn warn_unknown_action_tokens(&self, cli_extras: &[String]) -> usize {
        let known: BTreeSet<&str> = KNOWN_ACTION_TYPES.iter().copied().collect();
        let mut warned = 0usize;
        let mut check = |tok: &str, origin: &str| {
            if tok == "all" || known.contains(tok) {
                return;
            }
            warned += 1;
            let suggestion = closest_known_action(tok);
            match suggestion {
                Some(s) => eprintln!(
                    "settings: unknown action token '{tok}' ({origin}) — did you mean '{s}'? known: {}",
                    KNOWN_ACTION_TYPES.join(", ")
                ),
                None => eprintln!(
                    "settings: unknown action token '{tok}' ({origin}) — known: {}",
                    KNOWN_ACTION_TYPES.join(", ")
                ),
            }
        };
        if let Some(list) = &self.actions.allowed {
            for t in list {
                check(t, "settings.json:actions.allowed");
            }
        }
        for t in cli_extras {
            check(t, "--allow");
        }
        warned
    }

    /// Compute the effective action allowlist for this session.
    ///
    /// Semantics of `actions.allowed` in settings.json:
    /// - `null` or key absent → fall back to `DEFAULT_ALLOWED_ACTIONS`.
    /// - `[]` → empty, EVERY non-MCP action is denied. The
    ///   dispatcher enforces this — it does not collapse empty to
    ///   defaults. This is the operator's "lock it down" signal.
    /// - `["read", ...]` → that exact set, no defaults merged in.
    ///
    /// `cli_extras` are the `--allow ACTION` flags. They are added
    /// on top of whatever the settings resolved to.
    ///
    /// Unknown tokens (not in `KNOWN_ACTION_TYPES`, and not the
    /// special CLI escape-hatch `"all"`) are DROPPED rather than
    /// silently inserted — a dead entry in the allowlist serves no
    /// purpose and masks the typo. `warn_unknown_action_tokens`
    /// prints a warning for each dropped token; call it before (or
    /// alongside) this function so the operator sees why their
    /// flag didn't take.
    ///
    /// Special tokens in `cli_extras`:
    /// - `"all"` expands to the full built-in set plus `bash`
    ///   (every action the dispatcher knows). Useful for one-off
    ///   runs where the operator wants a total escape hatch.
    pub fn effective_allowed_actions(&self, cli_extras: &[String]) -> BTreeSet<String> {
        let known: BTreeSet<&str> = KNOWN_ACTION_TYPES.iter().copied().collect();
        let mut out: BTreeSet<String> = match &self.actions.allowed {
            Some(list) => list
                .iter()
                .filter(|t| known.contains(t.as_str()))
                .cloned()
                .collect(),
            None => DEFAULT_ALLOWED_ACTIONS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        };
        for e in cli_extras {
            if e == "all" {
                for a in DEFAULT_ALLOWED_ACTIONS {
                    out.insert((*a).to_string());
                }
                out.insert("bash".to_string());
            } else if known.contains(e.as_str()) {
                out.insert(e.clone());
            }
            // Unknown tokens are silently dropped here; the
            // companion warn_unknown_action_tokens surfaces them.
        }
        out
    }

    /// Expand one configured short model name, leaving concrete selectors
    /// unchanged. Aliases intentionally expand once: values must name a model
    /// id or a provider-qualified selector, not another alias.
    pub fn resolve_model_selector<'a>(&'a self, selector: &'a str) -> &'a str {
        self.model_aliases
            .get(selector)
            .map(String::as_str)
            .unwrap_or(selector)
    }

    /// Model selector for a role, or `None` when settings.json did not
    /// specify one. Configured short names are expanded before returning.
    pub fn model_for(&self, role: ModelRole) -> Option<&str> {
        let slot = match role {
            ModelRole::Fast => &self.models.fast,
            ModelRole::Slow => &self.models.slow,
            ModelRole::Main => &self.models.main,
            ModelRole::Todo => &self.models.todo,
            ModelRole::Classifier => &self.models.classifier,
        };
        slot.as_deref().map(|id| self.resolve_model_selector(id))
    }

    /// Optional supplemental review model, with configured aliases expanded.
    pub fn secondary_slow_model(&self) -> Option<&str> {
        self.models
            .slow_secondary
            .as_deref()
            .map(|id| self.resolve_model_selector(id))
    }

    /// Override the model id for a single role. `Some(id)` replaces
    /// whatever was loaded from settings.json; `None` is a no-op so
    /// callers can pass CLI `Option<String>` through directly.
    pub fn set_model(&mut self, role: ModelRole, id: Option<String>) {
        let Some(id) = id else { return };
        let slot = match role {
            ModelRole::Fast => &mut self.models.fast,
            ModelRole::Slow => &mut self.models.slow,
            ModelRole::Main => &mut self.models.main,
            ModelRole::Todo => &mut self.models.todo,
            ModelRole::Classifier => &mut self.models.classifier,
        };
        *slot = Some(id);
    }
}

/// Cheap Levenshtein-like distance for the typo suggester. Only
/// called when a token isn't in the known set; return `Some(best)`
/// when the closest known action is within edit-distance 2.
fn closest_known_action(tok: &str) -> Option<&'static str> {
    let mut best: Option<(&'static str, usize)> = None;
    for cand in KNOWN_ACTION_TYPES {
        let d = levenshtein(tok, cand);
        if d <= 2 && best.map(|(_, bd)| d < bd).unwrap_or(true) {
            best = Some((*cand, d));
        }
    }
    best.map(|(s, _)| s)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let av: Vec<char> = a.chars().collect();
    let bv: Vec<char> = b.chars().collect();
    let (n, m) = (av.len(), bv.len());
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut cur = vec![0usize; m + 1];
    for i in 1..=n {
        cur[0] = i;
        for j in 1..=m {
            let cost = if av[i - 1] == bv[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[m]
}

/// Resolve a model for a role using the documented precedence:
/// agent config → settings.json → Model::sonnet_4_6() fallback.
pub fn pick_model(cfg_model: Option<&str>, role: ModelRole, settings: &Settings) -> Model {
    if let Some(id) = cfg_model {
        return Model::from_id(id);
    }
    if let Some(id) = settings.model_for(role) {
        return Model::from_id(id);
    }
    Model::sonnet_4_6()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_settings_template_matches_strict_schema() {
        let rendered = include_str!("../../configs/settings.json")
            .replace("@MODEL@", "claude-sonnet-5")
            .replace("@SLOW_MODEL@", "claude-opus-4-8")
            .replace("@CLASSIFIER_MODEL@", "claude-haiku-4-5");
        let settings: Settings = serde_json::from_str(&rendered).unwrap();
        assert_eq!(settings.models.fast.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(settings.models.slow.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(settings.resolve_model_selector("sonnet"), "claude-sonnet-5");
        assert_eq!(settings.resolve_model_selector("opus"), "claude-opus-4-8");
    }

    #[test]
    fn missing_file_yields_defaults() {
        let s = Settings::load_from(Path::new("/tmp/kres-settings-does-not-exist.json"));
        assert!(s.models.fast.is_none());
        assert_eq!(
            pick_model(None, ModelRole::Fast, &s).id,
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn settings_fills_in_when_cfg_is_silent() {
        let dir = std::env::temp_dir().join(format!("kres-settings-fills-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        std::fs::write(
            &p,
            r#"{"models":{"slow":"claude-opus-4-7","main":"claude-sonnet-4-6"}}"#,
        )
        .unwrap();
        let s = Settings::load_from(&p);
        assert_eq!(pick_model(None, ModelRole::Slow, &s).id, "claude-opus-4-7");
        assert_eq!(
            pick_model(None, ModelRole::Main, &s).id,
            "claude-sonnet-4-6"
        );
        // fast role has nothing in settings → falls back to sonnet_4_6.
        assert_eq!(
            pick_model(None, ModelRole::Fast, &s).id,
            "claude-sonnet-4-6"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn model_roles_expand_configured_aliases() {
        let s: Settings = serde_json::from_str(
            r#"{"models":{"slow":"deep"},"model_aliases":{"deep":"provider.json:model-v2"}}"#,
        )
        .unwrap();
        assert_eq!(s.model_for(ModelRole::Slow), Some("provider.json:model-v2"));
        assert_eq!(s.resolve_model_selector("unaliased"), "unaliased");
    }

    #[test]
    fn secondary_slow_model_expands_configured_alias() {
        let s: Settings = serde_json::from_str(
            r#"{"models":{"slow_secondary":"general"},"model_aliases":{"general":"openai.json:gpt-5.4"}}"#,
        )
        .unwrap();
        assert_eq!(s.secondary_slow_model(), Some("openai.json:gpt-5.4"));
    }

    #[test]
    fn cfg_model_always_wins() {
        let s = Settings {
            models: Models {
                slow: Some("claude-opus-4-7".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            pick_model(Some("claude-sonnet-4-6"), ModelRole::Slow, &s).id,
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn default_allowlist_excludes_bash() {
        let s = Settings::default();
        let a = s.effective_allowed_actions(&[]);
        assert!(a.contains("read"));
        assert!(a.contains("grep"));
        assert!(a.contains("find"));
        assert!(a.contains("git"));
        assert!(a.contains("edit"));
        assert!(
            !a.contains("bash"),
            "bash should be off by default, got {a:?}"
        );
    }

    #[test]
    fn cli_allow_adds_bash() {
        let s = Settings::default();
        let a = s.effective_allowed_actions(&["bash".into()]);
        assert!(a.contains("bash"));
        // All defaults still there.
        assert!(a.contains("read"));
    }

    #[test]
    fn cli_allow_all_adds_everything() {
        // `--allow all` expands to DEFAULT_ALLOWED_ACTIONS + bash,
        // overriding even a narrow settings.json allowlist. Assert
        // every element of the default set plus bash, so a future
        // shrink of DEFAULT_ALLOWED_ACTIONS fails here instead of
        // silently missing the "all" expansion.
        let s = Settings {
            actions: ActionSettings {
                allowed: Some(vec!["read".into()]),
            },
            ..Default::default()
        };
        let a = s.effective_allowed_actions(&["all".into()]);
        for expected in DEFAULT_ALLOWED_ACTIONS {
            assert!(
                a.contains(*expected),
                "'all' should enable '{expected}', got {a:?}"
            );
        }
        assert!(a.contains("bash"), "'all' must enable bash, got {a:?}");
    }

    #[test]
    fn settings_allowlist_replaces_default() {
        // An explicit `["read","grep"]` kills the rest of the
        // default set (not a union).
        let s = Settings {
            actions: ActionSettings {
                allowed: Some(vec!["read".into(), "grep".into()]),
            },
            ..Default::default()
        };
        let a = s.effective_allowed_actions(&[]);
        assert!(a.contains("read"));
        assert!(a.contains("grep"));
        assert!(!a.contains("git"), "git shouldn't leak from default: {a:?}");
        assert!(!a.contains("bash"));
    }

    #[test]
    fn project_only_allowlist_replaces_defaults() {
        // A single project file containing a narrow allowlist must
        // produce exactly that set — no defaults leaking through.
        // Uses load_from directly rather than load_merged to avoid
        // touching the operator's real ~/.kres/settings.json.
        let dir =
            std::env::temp_dir().join(format!("kres-settings-proj-only-{}", std::process::id()));
        let proj = dir.join(".kres");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("settings.json"),
            r#"{"actions":{"allowed":["read"]}}"#,
        )
        .unwrap();
        let s = Settings::load_from(&proj.join("settings.json"));
        let a = s.effective_allowed_actions(&[]);
        assert_eq!(
            a.iter().cloned().collect::<Vec<_>>(),
            vec!["read".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_merged_project_overrides_global() {
        // Global settings allow read+grep and set slow=opus;
        // project narrows the allowlist to just read and overrides
        // main=sonnet. Result: allowlist={read} (project wins),
        // slow=opus (project didn't touch), main=sonnet (project
        // wins).
        let dir =
            std::env::temp_dir().join(format!("kres-settings-merge-real-{}", std::process::id()));
        let global_dir = dir.join("global");
        let proj_dir = dir.join("project").join(".kres");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::create_dir_all(&proj_dir).unwrap();
        let global_path = global_dir.join("settings.json");
        let proj_path = proj_dir.join("settings.json");
        std::fs::write(
            &global_path,
            r#"{"models":{"slow":"claude-opus-4-7"},"actions":{"allowed":["read","grep"]}}"#,
        )
        .unwrap();
        std::fs::write(
            &proj_path,
            r#"{"models":{"main":"claude-sonnet-4-6"},"actions":{"allowed":["read"]}}"#,
        )
        .unwrap();
        let s = Settings::load_merged_with_paths(Some(&global_path), &proj_path);
        assert_eq!(s.models.slow.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(s.models.main.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(
            s.effective_allowed_actions(&[])
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["read".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_merged_global_only_when_no_project() {
        // Project file absent → result should be exactly the
        // global settings.
        let dir = std::env::temp_dir().join(format!(
            "kres-settings-merge-global-only-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let global = dir.join("global.json");
        std::fs::write(&global, r#"{"actions":{"allowed":["read","grep"]}}"#).unwrap();
        let missing_project = dir.join("nope/.kres/settings.json");
        let s = Settings::load_merged_with_paths(Some(&global), &missing_project);
        let a: Vec<String> = s.effective_allowed_actions(&[]).iter().cloned().collect();
        assert_eq!(a, vec!["grep".to_string(), "read".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn project_model_aliases_override_by_name() {
        let mut global: Settings =
            serde_json::from_str(r#"{"model_aliases":{"sonnet":"sonnet-v1","opus":"opus-v1"}}"#)
                .unwrap();
        let project: Settings =
            serde_json::from_str(r#"{"model_aliases":{"sonnet":"sonnet-v2"}}"#).unwrap();

        global.apply_project_overrides(project);

        assert_eq!(global.resolve_model_selector("sonnet"), "sonnet-v2");
        assert_eq!(global.resolve_model_selector("opus"), "opus-v1");
    }

    #[test]
    fn project_secondary_slow_model_overrides_global() {
        let mut global: Settings =
            serde_json::from_str(r#"{"models":{"slow_secondary":"secondary-v1"}}"#).unwrap();
        let project: Settings =
            serde_json::from_str(r#"{"models":{"slow_secondary":"secondary-v2"}}"#).unwrap();

        global.apply_project_overrides(project);

        assert_eq!(global.secondary_slow_model(), Some("secondary-v2"));
    }

    #[test]
    fn warn_unknown_action_tokens_flags_typos() {
        let s = Settings {
            actions: ActionSettings {
                allowed: Some(vec!["read".into(), "fnid".into()]),
            },
            ..Default::default()
        };
        let n = s.warn_unknown_action_tokens(&["bsah".into(), "all".into()]);
        // "read" is known; "all" is the CLI escape-hatch; "fnid"
        // (typo for "find") and "bsah" (typo for "bash") warn.
        assert_eq!(n, 2, "expected 2 warnings, got {n}");
    }

    #[test]
    fn warn_unknown_action_tokens_silent_for_clean_input() {
        let s = Settings {
            actions: ActionSettings {
                allowed: Some(vec!["read".into(), "bash".into()]),
            },
            ..Default::default()
        };
        let n = s.warn_unknown_action_tokens(&["grep".into(), "all".into()]);
        assert_eq!(n, 0);
    }

    #[test]
    fn closest_known_action_suggests_within_edit_distance_two() {
        assert_eq!(closest_known_action("bsah"), Some("bash"));
        assert_eq!(closest_known_action("fnid"), Some("find"));
        assert_eq!(closest_known_action("rad"), Some("read"));
        // Completely unrelated token — no suggestion.
        assert_eq!(closest_known_action("completely_wrong"), None);
    }

    #[test]
    fn explicit_empty_allowlist_stays_empty() {
        // settings.json `{"actions":{"allowed":[]}}` means deny
        // everything, not fall-back-to-defaults. Verify the
        // resolved set is empty so the dispatcher sees the lockdown.
        let s = Settings {
            actions: ActionSettings {
                allowed: Some(vec![]),
            },
            ..Default::default()
        };
        let a = s.effective_allowed_actions(&[]);
        assert!(a.is_empty(), "explicit [] should stay empty, got {a:?}");
    }

    #[test]
    fn empty_file_yields_defaults() {
        let dir = std::env::temp_dir().join(format!("kres-settings-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("settings.json");
        std::fs::write(&p, "").unwrap();
        let s = Settings::load_from(&p);
        assert!(s.models.fast.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }
}
