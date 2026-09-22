//! The selection a run leaves behind, so the next one begins where this one stopped.
//!
//! A configuration names a provider, a plan, a model, and an effort. The interface can change
//! all four while an agent is running — `/provider`, `/model`, `/effort`, `Alt+P`, `Alt+T` —
//! and without a record of that change the next start forgets it: the reader picks a model,
//! quits, and is answered by the old one. This module is the record.
//!
//! ## Why a file of its own rather than the configuration
//!
//! Writing the change into `config.toml` would mean rewriting a document a person maintains —
//! dropping its comments, and pinning every field it left absent, which is what makes a
//! provider change mean "the provider's own model". It would also have nowhere to put the
//! effort: the configuration's field has four steps and a model's scale can have seven
//! (`none` and `max` among them), so the value that was actually in force would not round
//! trip. A small file beside the sessions is the honest shape.
//!
//! ## When it is written, and why only then
//!
//! **On a change, never on a start.** An agent that wrote its selection at every startup would
//! make merely opening the interface enough to override a configuration the reader then edits;
//! recording only the moments a client *changes* something keeps the configuration the default
//! until somebody asks for a different one.
//!
//! ## What it does not mean
//!
//! The record is the *default*, not a lock. A start applies it and the configuration resolves
//! the rest, so a provider the record names but this build no longer offers is reported the
//! same way a configuration naming one is.

use std::path::{Path, PathBuf};

use nanus_adapter_config::NanusConfig;
use nanus_ports::ReasoningEffort;
use serde::{Deserialize, Serialize};

/// The file, inside the nanus home, that remembers the last selection.
pub const SELECTION_FILE: &str = "selection.toml";

/// A provider, plan, model, and effort as a run left them.
///
/// Every field is optional and absent means *not recorded*, exactly as it does in a session's
/// origin: a provider without an effort knob records no effort rather than a plausible one,
/// and a field a future build adds would be absent in a file this build wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSelection {
    /// The provider's name, as the provider table spells it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The plan in force, when the provider has more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The model id the requests named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning effort in the port vocabulary — not the configuration's four-step
    /// spelling, because `none` and `max` are steps a model takes and the configuration
    /// cannot name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl LastSelection {
    /// Returns where the record lives, given a nanus home.
    #[must_use]
    pub fn path(home: &Path) -> PathBuf {
        home.join(SELECTION_FILE)
    }

    /// Reads the record, or `None` when nothing is stored or the file cannot be read.
    ///
    /// A record that cannot be read is not a reason to refuse a run: it is a preference, and
    /// the configuration is a complete answer without it. The failure is logged rather than
    /// swallowed, because a record that keeps failing to load is a file to look at.
    #[must_use]
    pub fn load(home: &Path) -> Option<Self> {
        let path = Self::path(home);
        let text = std::fs::read_to_string(&path).ok()?;
        match toml::from_str::<Self>(&text) {
            Ok(selection) => Some(selection),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "the remembered selection could not be read");
                None
            }
        }
    }

    /// Writes the record, so the next start begins where this one left off.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the record cannot be encoded or written. A caller is free to
    /// ignore it — the selection is in force either way — which is why this is not a
    /// [`BundleError`](crate::BundleError).
    pub fn save(&self, home: &Path) -> std::io::Result<PathBuf> {
        let path = Self::path(home);
        let body = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Written to a sibling and renamed, the same rule the configuration follows: a reader
        // between the two steps sees the old file or the new one, never a half-written pair.
        let staged = path.with_extension("toml.new");
        std::fs::write(&staged, body.as_bytes())?;
        std::fs::rename(&staged, &path)?;
        Ok(path)
    }

    /// Overlays the remembered provider, plan, and model onto a configuration.
    ///
    /// Taken together when a provider is recorded, because they are one choice: a provider
    /// without the plan and model that were used with it would send the previous provider's
    /// model to the new host, which is a refused request rather than a substitution. A record
    /// with a model and no provider — one a future build wrote, or a person edited — moves only
    /// the model.
    pub fn apply(&self, config: &mut NanusConfig) {
        if let Some(provider) = &self.provider {
            config.provider = Some(provider.clone());
            config.plan.clone_from(&self.plan);
            config.model.clone_from(&self.model);
        } else if self.model.is_some() {
            config.model.clone_from(&self.model);
        }
    }

    /// Returns the remembered effort, when it names a step on the scale.
    ///
    /// Read by the port's own parser rather than a second table, so a record naming a step this
    /// build does not have is ignored rather than mapped onto a neighbouring one.
    #[must_use]
    pub fn effort(&self) -> Option<ReasoningEffort> {
        self.effort.as_deref().and_then(ReasoningEffort::parse)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home of this test's own, removed first so a rerun does not read the last one's file.
    fn home(name: &str) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("nanus-selection-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        home
    }

    #[test]
    fn a_record_round_trips_through_the_file() {
        let home = home("round-trip");
        let selection = LastSelection {
            provider: Some(String::from("zai")),
            plan: Some(String::from("coding")),
            model: Some(String::from("glm-5.3-flashx")),
            effort: Some(String::from("max")),
        };
        let path = selection.save(&home).expect("the record is written");
        assert_eq!(path, LastSelection::path(&home));
        assert_eq!(LastSelection::load(&home).as_ref(), Some(&selection));
    }

    #[test]
    fn an_absent_or_unreadable_record_is_none_rather_than_an_error() {
        let home = home("absent");
        assert!(LastSelection::load(&home).is_none(), "nothing stored");

        std::fs::create_dir_all(&home).expect("a home");
        std::fs::write(LastSelection::path(&home), "this is not toml = =").expect("a bad file");
        assert!(
            LastSelection::load(&home).is_none(),
            "a damaged record is ignored rather than fatal"
        );
    }

    #[test]
    fn applying_a_provider_takes_its_plan_and_model_with_it() {
        // The pair is what makes a provider a choice rather than a host: the new provider's
        // model is not the old one's id.
        let mut config = NanusConfig {
            provider: Some(String::from("deepseek")),
            plan: Some(String::from("api")),
            model: Some(String::from("deepseek-flash")),
            ..NanusConfig::default()
        };
        let selection = LastSelection {
            provider: Some(String::from("openai")),
            plan: Some(String::from("subscription")),
            model: Some(String::from("gpt-5.3-codex")),
            effort: None,
        };
        selection.apply(&mut config);
        assert_eq!(config.provider.as_deref(), Some("openai"));
        assert_eq!(config.plan.as_deref(), Some("subscription"));
        assert_eq!(config.model.as_deref(), Some("gpt-5.3-codex"));
    }

    #[test]
    fn a_plan_and_a_model_are_dropped_when_the_provider_names_none() {
        // A provider whose own answer is the plan: recording `provider = "deepseek"` must not
        // leave the previous provider's plan or model on the configuration.
        let mut config = NanusConfig {
            plan: Some(String::from("subscription")),
            model: Some(String::from("gpt-5.3-codex")),
            ..NanusConfig::default()
        };
        let selection = LastSelection {
            provider: Some(String::from("deepseek")),
            plan: None,
            model: None,
            effort: None,
        };
        selection.apply(&mut config);
        assert_eq!(config.plan, None);
        assert_eq!(config.model, None);
    }

    #[test]
    fn a_model_with_no_provider_moves_only_the_model() {
        let mut config = NanusConfig {
            provider: Some(String::from("deepseek")),
            model: Some(String::from("deepseek-flash")),
            ..NanusConfig::default()
        };
        let selection = LastSelection {
            model: Some(String::from("deepseek-v4-pro")),
            ..LastSelection::default()
        };
        selection.apply(&mut config);
        assert_eq!(config.provider.as_deref(), Some("deepseek"));
        assert_eq!(config.model.as_deref(), Some("deepseek-v4-pro"));
    }

    #[test]
    fn the_effort_is_read_in_the_ports_vocabulary_and_a_word_off_the_scale_is_none() {
        // `max` is a step DeepSeek takes and the configuration's four-value field cannot name,
        // which is the whole reason the record stores the port spelling.
        let effort = |raw: Option<&str>| LastSelection {
            effort: raw.map(str::to_owned),
            ..LastSelection::default()
        };
        assert_eq!(effort(Some("max")).effort(), Some(ReasoningEffort::Max));
        assert_eq!(effort(Some("none")).effort(), Some(ReasoningEffort::None));
        assert_eq!(effort(Some("not-a-step")).effort(), None);
        assert_eq!(effort(None).effort(), None);
    }
}
