//! The one configuration schema, its defaults, its precedence, and its migrations.
//!
//! The schema is deliberately a single flat table: a partial file works because
//! every field defaults, and the file is the *only* durable configuration state.
//! The API key is not part of it and never will be — see [`api_key`].

use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nanus_domain::{ApprovalPolicy, SandboxMode};
use nanus_kernel::{Migration, run_startup_migrations};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::ConfigError;

/// The configuration schema version this build reads and writes.
pub const CONFIG_VERSION: u32 = 1;

/// The environment variable that names an explicit configuration file.
pub const CONFIG_ENV: &str = "NANUS_CONFIG";

/// The environment variable that carries the provider API key.
///
/// The key is read from the environment on every use and is never stored in
/// [`NanusConfig`], never serialised, and never rendered by `Debug`.
pub const API_KEY_ENV: &str = "DEEPSEEK_API_KEY";

/// The model used when the configuration names none.
pub const DEFAULT_MODEL: &str = "deepseek-flash";

/// The per-response token budget used when the configuration names none.
pub const DEFAULT_MAX_TOKENS: u32 = 8_192;

/// The step budget for one turn used when the configuration names none.
///
/// The number exists to bound a *runaway* loop, and every value it has had was chosen to
/// stop it bounding the task instead. It began at thirty-two and was measured against real
/// work within a day: the first non-trivial task this harness was given — add three
/// counters to the interface, touching two crates — spent twenty-seven of its thirty-two
/// steps reading before it made a single edit, and the turn closed mid-change. Five
/// hundred and twelve is headroom for the work that actually takes a long time: a
/// refactor across several crates, or a task whose build and test cycle runs a dozen times,
/// is hundreds of steps of honest work rather than a loop that will not stop. A runaway is
/// still stopped — just later than a long task ends.
pub const DEFAULT_MAX_STEPS_PER_TURN: u32 = 512;

/// The tool-concurrency budget used when the configuration names none.
pub const DEFAULT_MAX_PARALLEL_TOOLS: u32 = 4;

/// How much reasoning the model is asked to spend.
///
/// The spellings and the variant set mirror [`nanus_ports::ReasoningEffort`],
/// which is the boundary vocabulary the model adapter consumes. This type exists
/// separately only because configuration must be `serde`-able, defaulted, and
/// versioned, and the port's type is deliberately none of those; [`to_port`](Self::to_port)
/// is the bridge, so the two can never drift into different spellings.
///
/// The field is an enum rather than a string so an unknown spelling is a startup
/// error, not a silent default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Spend as little as possible.
    Minimal,
    /// Spend a little.
    Low,
    /// Spend the default amount.
    #[default]
    Medium,
    /// Spend as much as the provider allows.
    High,
}

impl ReasoningEffort {
    /// Returns the spelling the provider expects.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    /// Converts to the port vocabulary the model adapter consumes.
    #[must_use]
    pub const fn to_port(self) -> nanus_ports::ReasoningEffort {
        match self {
            Self::Minimal => nanus_ports::ReasoningEffort::Minimal,
            Self::Low => nanus_ports::ReasoningEffort::Low,
            Self::Medium => nanus_ports::ReasoningEffort::Medium,
            Self::High => nanus_ports::ReasoningEffort::High,
        }
    }
}

impl core::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// How much of a tool call and a thinking segment the interface shows.
///
/// The interface draws the parts of a turn that are *about* the work rather than the work
/// itself as one line each — which tool is called and what it is doing, and the newest
/// line of the model's thinking — because a transcript that spells out every argument
/// block and every paragraph of reasoning buries the answer a reader came for. This
/// setting asks for the whole of them instead.
///
/// One setting rather than two, and named for both halves deliberately: they are the same
/// preference about the same thing, and a user who wants the argument blocks wants the
/// reasoning paragraphs too. Someone who wants neither still has the interface's own
/// `Ctrl+T` and `Ctrl+R`, which fold runs of them away entirely.
///
/// An enum rather than a string so an unknown spelling is a startup error rather than a
/// silent default, exactly as the reasoning effort is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TuiDetail {
    /// One line per tool call, and the newest line of a thinking segment.
    #[default]
    Compact,
    /// The whole tool call, its arguments included, and the whole thinking segment.
    Full,
}

impl TuiDetail {
    /// Returns the spelling the configuration file uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }
}

impl core::fmt::Display for TuiDetail {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The whole of nanus's durable configuration.
///
/// `#[serde(default)]` is at the *struct* level, so a file that sets one field
/// gets [`NanusConfig::default`] for every other field. A missing file is not an
/// error at all: it yields the defaults.
///
/// There is no API-key field. A file that contains one is not rejected — unknown
/// keys are ignored, which is what makes "a config file cannot smuggle a secret
/// into the logs" true rather than merely intended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NanusConfig {
    /// The schema version the file was written with.
    pub config_version: u32,
    /// The model id to send requests to.
    pub model: String,
    /// The per-response token budget.
    pub max_tokens: u32,
    /// How much reasoning to ask for.
    pub reasoning_effort: ReasoningEffort,
    /// Whether a tool call needs a human decision.
    pub approval_policy: ApprovalPolicy,
    /// What a tool call may touch.
    pub sandbox_mode: SandboxMode,
    /// How many model steps one turn may take.
    pub max_steps_per_turn: u32,
    /// How many tool calls may run at once.
    pub max_parallel_tools: u32,
    /// How much of a tool call and a thinking segment the interface draws.
    pub tui_detail: TuiDetail,
    /// An override for the built-in system prompt.
    pub system_prompt: Option<String>,
    /// An override for the workspace root the tools are confined to.
    pub workspace_root: Option<PathBuf>,
    /// The socket a `nanus service` listens on.
    ///
    /// Defaults to `<nanus home>/run/agent.sock`, which is also the path a client
    /// computes, so a service and the interface that talks to it never have to exchange
    /// it. Setting it is for running two services on one machine.
    pub service_socket: Option<PathBuf>,
    /// Where a detached `nanus service` writes its output.
    ///
    /// A detached process has no terminal, so without this its diagnostics go nowhere at
    /// all. Defaults to `<nanus home>/nanus-service.log`.
    pub service_log: Option<PathBuf>,
}

impl Default for NanusConfig {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            model: DEFAULT_MODEL.to_owned(),
            max_tokens: DEFAULT_MAX_TOKENS,
            reasoning_effort: ReasoningEffort::default(),
            approval_policy: ApprovalPolicy::default(),
            sandbox_mode: SandboxMode::default(),
            max_steps_per_turn: DEFAULT_MAX_STEPS_PER_TURN,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            tui_detail: TuiDetail::default(),
            system_prompt: None,
            workspace_root: None,
            service_socket: None,
            service_log: None,
        }
    }
}

/// The startup migrations this build knows how to apply, in ascending order.
///
/// The chain is data, not control flow: adding a version means adding one entry
/// here and bumping [`CONFIG_VERSION`], and the kernel applies whatever steps lie
/// between the file's version and this build's.
const MIGRATIONS: [Migration; 1] = [Migration::new(
    0,
    1,
    "rename the pre-1.0 `max_output_tokens` field to `max_tokens`",
    rename_max_output_tokens,
)];

/// Renames the pre-1.0 `max_output_tokens` field to `max_tokens`.
///
/// A migration reads and writes raw JSON rather than the typed config, because
/// the typed struct cannot represent the *old* shape — that is the whole reason
/// migrations exist.
fn rename_max_output_tokens(document: &mut Value) -> Result<(), nanus_kernel::Error> {
    let Some(table) = document.as_object_mut() else {
        return Err(nanus_kernel::Error::Config(String::from(
            "the configuration document is not a table",
        )));
    };
    if let Some(legacy) = table.remove("max_output_tokens") {
        table.entry("max_tokens").or_insert(legacy);
    }
    Ok(())
}

/// Reads the provider API key from the environment.
///
/// Returned to the caller and then forgotten: this crate has no place to put it,
/// and deliberately no function that would write it to disk.
#[must_use]
pub fn api_key() -> Option<String> {
    std::env::var(API_KEY_ENV)
        .ok()
        .filter(|key| !key.trim().is_empty())
}

impl NanusConfig {
    /// Loads the configuration using the documented precedence.
    ///
    /// A path that does not exist yields [`NanusConfig::default`]; a path that
    /// exists but is malformed is an error. Silence is for absence, never for
    /// damage.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file cannot be read, is not TOML, does
    /// not fit the schema, or was written by a newer build.
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let path = Self::source_path(explicit)?;
        if !path.exists() {
            tracing::debug!(path = %path.display(), "no configuration file; using defaults");
            return Ok(Self::default());
        }
        Self::load_from(&path)
    }

    /// Loads the configuration from exactly `path`.
    ///
    /// # Errors
    ///
    /// As [`NanusConfig::load`], minus the precedence.
    pub fn load_from(path: &Path) -> Result<Self, ConfigError> {
        assert!(
            !path.as_os_str().is_empty(),
            "a configuration path is named"
        );
        let text = fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(&text, path)
    }

    /// Parses and migrates a configuration document.
    ///
    /// The migration runs on the raw document *before* deserialisation, because
    /// the typed struct cannot represent a pre-migration shape and because the
    /// struct's `Default` would otherwise hide a missing `config_version`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Malformed`] for invalid TOML,
    /// [`ConfigError::UnsupportedVersion`] for a newer file,
    /// [`ConfigError::Migration`] when a step fails, and
    /// [`ConfigError::Invalid`] when the migrated document does not fit.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, ConfigError> {
        assert!(
            !text.is_empty() || path.exists(),
            "an empty document came from a file"
        );
        let parsed: toml::Value =
            toml::from_str(text).map_err(|source| ConfigError::Malformed {
                path: path.to_path_buf(),
                source,
            })?;
        let document = serde_json::to_value(parsed).map_err(|source| ConfigError::Invalid {
            path: path.to_path_buf(),
            source,
        })?;
        let config = Self::migrate(document)?;
        tracing::debug!(path = %path.display(), version = config.config_version, "loaded configuration");
        Ok(config)
    }

    /// Runs the migration chain over a raw document and deserialises the result.
    ///
    /// # Errors
    ///
    /// As [`NanusConfig::from_toml`] for the version and migration failures.
    pub fn migrate(mut document: Value) -> Result<Self, ConfigError> {
        let declared = document
            .get("config_version")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let found = u32::try_from(declared).unwrap_or(u32::MAX);
        if found > CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                found,
                supported: CONFIG_VERSION,
            });
        }
        let applied = run_startup_migrations(&mut document, found, CONFIG_VERSION, &MIGRATIONS)?;
        assert_eq!(
            applied, CONFIG_VERSION,
            "the migration chain reaches this build's version"
        );
        if let Some(table) = document.as_object_mut() {
            table.insert(String::from("config_version"), Value::from(applied));
        }
        serde_json::from_value(document).map_err(|source| ConfigError::Invalid {
            path: PathBuf::from("<migrated document>"),
            source,
        })
    }

    /// Returns the path the configuration would be read from, by precedence.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NoConfigDirectory`] when neither an explicit path,
    /// nor [`CONFIG_ENV`], nor a platform configuration directory is available.
    pub fn source_path(explicit: Option<&Path>) -> Result<PathBuf, ConfigError> {
        if let Some(path) = explicit {
            assert!(
                !path.as_os_str().is_empty(),
                "an explicit configuration path is not empty"
            );
            return Ok(path.to_path_buf());
        }
        if let Some(raw) = std::env::var_os(CONFIG_ENV)
            && !raw.is_empty()
        {
            return Ok(PathBuf::from(raw));
        }
        Self::default_path()
    }

    /// Returns the platform configuration path: `<config dir>/nanus/config.toml`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::NoConfigDirectory`] when the platform lookup fails.
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        use etcetera::BaseStrategy as _;
        let strategy =
            etcetera::choose_base_strategy().map_err(|error| ConfigError::NoConfigDirectory {
                reason: error.to_string(),
            })?;
        let path = strategy.config_dir().join("nanus").join("config.toml");
        assert!(path.is_absolute(), "a platform config path is absolute");
        Ok(path)
    }

    /// Writes the configuration to the precedence-resolved path.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the path cannot be resolved or written.
    pub fn save(&self, explicit: Option<&Path>) -> Result<PathBuf, ConfigError> {
        let path = Self::source_path(explicit)?;
        self.save_to(&path)?;
        Ok(path)
    }

    /// Writes the configuration to exactly `path`, atomically.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Serialize`] when the document cannot be encoded and
    /// [`ConfigError::Io`] when the write, sync, or rename fails.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        assert!(
            !path.as_os_str().is_empty(),
            "a configuration path is named"
        );
        let body =
            toml::to_string_pretty(self).map_err(|source| ConfigError::Serialize { source })?;
        write_atomic(path, &body)
    }
}

/// Writes `body` to `path` atomically: a temp file in the same directory, then a
/// rename, so a reader never observes a half-written configuration.
fn write_atomic(path: &Path, body: &str) -> Result<(), ConfigError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let temp = temp_path(path);
    let io = |source: std::io::Error, at: &Path| ConfigError::Io {
        path: at.to_path_buf(),
        source,
    };
    {
        let mut file = fs::File::create(&temp).map_err(|source| io(source, &temp))?;
        file.write_all(body.as_bytes())
            .map_err(|source| io(source, &temp))?;
        // fsync before the rename is what makes the rename a commit rather than a
        // hope: without it a crash can leave a zero-length file under the real name.
        file.sync_all().map_err(|source| io(source, &temp))?;
    }
    if let Err(source) = fs::rename(&temp, path) {
        if fs::remove_file(&temp).is_ok() {
            tracing::debug!(temp = %temp.display(), "discarded the temporary file");
        }
        return Err(io(source, path));
    }
    Ok(())
}

/// Builds a unique sibling temp path for `path`.
fn temp_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let name = path.file_name().and_then(OsStr::to_str).map_or_else(
        || format!(".nanus.{pid}.{seq}.tmp"),
        |name| format!(".{name}.{pid}.{seq}.tmp"),
    );
    path.with_file_name(name)
}
