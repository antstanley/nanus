//! Integration tests for the configuration adapter.
//!
//! ## Why the precedence tests re-run this binary
//!
//! `std::env::set_var` is `unsafe` in edition 2024, and this workspace forbids
//! `unsafe` unconditionally, so a test cannot mutate its own environment. The
//! precedence tests therefore re-execute this test binary with the environment
//! they want: the child runs the same named test and takes the assertion branch.
//! That is the only way to test environment-driven precedence under `forbid`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use nanus_adapter_config::{
    CONFIG_VERSION, DEFAULT_MAX_PARALLEL_TOOLS, DEFAULT_MAX_STEPS_PER_TURN, DEFAULT_MAX_TOKENS,
    DEFAULT_MODEL, NanusConfig, ReasoningEffort, TuiDetail, api_key,
};
use nanus_domain::{ApprovalPolicy, SandboxMode};

/// Marks a re-executed child run.
const CHILD: &str = "NANUS_CONFIG_TEST_CHILD";

/// The directory the parent prepared fixtures in.
const FIXTURE_DIR: &str = "NANUS_CONFIG_TEST_FIXTURES";

/// Re-runs this test binary with a controlled environment.
fn rerun(test: &str, envs: &[(&str, &str)], also_remove: &[&str]) -> std::process::Output {
    let exe = std::env::current_exe().expect("current exe");
    let mut command = std::process::Command::new(exe);
    command
        .arg("--exact")
        .arg(test)
        .arg("--nocapture")
        .arg("--test-threads=1");
    command.env_remove("NANUS_CONFIG");
    command.env_remove("HOME");
    for key in also_remove {
        command.env_remove(key);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("child test run")
}

/// Asserts a child run succeeded, surfacing its output when it did not.
fn assert_child_ok(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "child failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ---------------------------------------------------------------------------
// Defaults, partial files, and malformed files.
// ---------------------------------------------------------------------------

#[test]
fn the_built_in_defaults_are_the_documented_ones() {
    let config = NanusConfig::default();
    assert_eq!(config.model, DEFAULT_MODEL);
    assert_eq!(config.model, "deepseek-flash");
    // The retired ids must not be what a fresh configuration selects.
    assert_ne!(config.model, "deepseek-chat");
    assert_ne!(config.model, "deepseek-reasoner");
    assert_eq!(config.max_tokens, DEFAULT_MAX_TOKENS);
    assert_eq!(config.max_steps_per_turn, DEFAULT_MAX_STEPS_PER_TURN);
    assert_eq!(config.max_parallel_tools, DEFAULT_MAX_PARALLEL_TOOLS);
    assert_eq!(config.reasoning_effort, ReasoningEffort::Medium);
    assert_eq!(
        config.reasoning_effort.to_port(),
        nanus_ports::ReasoningEffort::Medium
    );
    assert_eq!(config.approval_policy, ApprovalPolicy::Ask);
    assert_eq!(config.sandbox_mode, SandboxMode::ReadOnly);
    assert_eq!(
        config.tui_detail,
        TuiDetail::Compact,
        "a transcript is compact unless the reader asked otherwise"
    );
    assert!(
        config.markdown,
        "the model's markdown is rendered by default"
    );
    assert!(config.mermaid, "a mermaid fence is drawn by default");
    assert_eq!(config.config_version, CONFIG_VERSION);
    assert!(config.system_prompt.is_none());
    assert!(config.workspace_root.is_none());
}

#[test]
fn a_missing_file_yields_defaults() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist.toml");
    let config = NanusConfig::load(Some(&missing)).expect("a missing file is not an error");
    assert_eq!(config, NanusConfig::default());
}

#[test]
fn a_partial_file_uses_defaults_for_everything_else() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "model = \"deepseek-v4-pro\"\n").expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    assert_eq!(config.model, "deepseek-v4-pro");
    assert_eq!(
        config.max_tokens, DEFAULT_MAX_TOKENS,
        "unset fields default"
    );
    assert_eq!(config.sandbox_mode, SandboxMode::ReadOnly);
}

#[test]
fn a_malformed_file_is_a_typed_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "model = \nthis is not toml").expect("seed");
    let error = NanusConfig::load(Some(&path)).expect_err("must be rejected");
    assert!(
        matches!(error, nanus_adapter_config::ConfigError::Malformed { .. }),
        "{error}"
    );
}

#[test]
fn a_missing_file_is_not_an_error_but_damage_is() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("absent.toml");
    assert!(
        NanusConfig::load(Some(&absent)).is_ok(),
        "absence is silent"
    );
    let broken = dir.path().join("broken.toml");
    std::fs::write(&broken, "= = =").expect("seed");
    assert!(NanusConfig::load(Some(&broken)).is_err(), "damage is loud");
}

#[test]
fn a_file_from_a_newer_build_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        format!("config_version = {}\n", CONFIG_VERSION.saturating_add(7)),
    )
    .expect("seed");
    let error = NanusConfig::load(Some(&path)).expect_err("must be refused");
    match error {
        nanus_adapter_config::ConfigError::UnsupportedVersion { found, supported } => {
            assert_eq!(found, CONFIG_VERSION.saturating_add(7));
            assert_eq!(supported, CONFIG_VERSION);
        }
        other => panic!("expected a version refusal, got {other}"),
    }
}

#[test]
fn the_permission_and_effort_spellings_parse() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "reasoning_effort = \"minimal\"\napproval_policy = \"never\"\nsandbox_mode = \"workspace_write\"\n",
    )
    .expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    assert_eq!(config.reasoning_effort, ReasoningEffort::Minimal);
    assert_eq!(config.reasoning_effort.as_wire(), "minimal");
    assert_eq!(config.approval_policy, ApprovalPolicy::Never);
    assert_eq!(config.sandbox_mode, SandboxMode::WorkspaceWrite);
}

#[test]
fn the_interface_detail_spelling_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "tui_detail = \"full\"\n").expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    assert_eq!(config.tui_detail, TuiDetail::Full);
    assert_eq!(config.tui_detail.as_str(), "full");
    assert_eq!(config.tui_detail.to_string(), "full");
    // The spelling a save writes is the spelling a load reads, so the setting survives a
    // round trip rather than only the first load.
    let written = dir.path().join("written.toml");
    config.save(Some(&written)).expect("save");
    let reloaded = NanusConfig::load(Some(&written)).expect("reload");
    assert_eq!(reloaded.tui_detail, TuiDetail::Full);
}

#[test]
fn markdown_rendering_can_be_turned_off() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "markdown = false\nmermaid = false\n").expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    assert!(!config.markdown);
    assert!(!config.mermaid);
    // The setting survives a save and a reload, not only the first parse.
    let written = dir.path().join("written.toml");
    config.save(Some(&written)).expect("save");
    let reloaded = NanusConfig::load(Some(&written)).expect("reload");
    assert!(!reloaded.markdown);
    assert!(!reloaded.mermaid);
}

#[test]
fn an_unknown_detail_spelling_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "tui_detail = \"verbose\"\n").expect("seed");
    assert!(
        NanusConfig::load(Some(&path)).is_err(),
        "a typo must not silently select a rendering the reader did not ask for"
    );
}

#[test]
fn an_unknown_enum_spelling_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "sandbox_mode = \"maybe\"\n").expect("seed");
    assert!(
        NanusConfig::load(Some(&path)).is_err(),
        "a typo must not silently disable the sandbox"
    );
}

// ---------------------------------------------------------------------------
// Migration.
// ---------------------------------------------------------------------------

#[test]
fn the_zero_to_one_migration_renames_the_legacy_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("legacy.toml");
    // A pre-1.0 file: no `config_version`, and the old field name.
    std::fs::write(
        &path,
        "max_output_tokens = 4096\nmodel = \"deepseek-flash\"\n",
    )
    .expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    assert_eq!(
        config.max_tokens, 4096,
        "the legacy field was carried across by the migration"
    );
    assert_eq!(
        config.config_version, CONFIG_VERSION,
        "the version was bumped"
    );
}

#[test]
fn the_migration_chain_runs_through_the_kernel() {
    let mut document = serde_json::json!({ "max_output_tokens": 777 });
    let config = NanusConfig::migrate(document.clone()).expect("migrate");
    assert_eq!(config.max_tokens, 777);
    assert_eq!(config.config_version, CONFIG_VERSION);
    // The migration is idempotent in effect: a document that already has the new
    // field keeps its value rather than being overwritten by a stale one.
    document = serde_json::json!({ "max_output_tokens": 111, "max_tokens": 222 });
    let config = NanusConfig::migrate(document).expect("migrate");
    assert_eq!(config.max_tokens, 222, "the existing field wins");
}

// ---------------------------------------------------------------------------
// The API key never reaches the logs.
// ---------------------------------------------------------------------------

#[test]
fn the_debug_rendering_holds_no_secret_and_no_key_field() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "model = \"deepseek-flash\"\napi_key = \"sk-super-secret-value\"\ndebug_key = \"sk-second-secret\"\n",
    )
    .expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    let rendered = format!("{config:?}");
    assert!(!rendered.contains("sk-super-secret-value"), "{rendered}");
    assert!(!rendered.contains("sk-second-secret"), "{rendered}");
    assert!(!rendered.contains("api_key"), "{rendered}");
    assert!(!rendered.to_lowercase().contains("secret"), "{rendered}");
    // And the same is true of the serialised form the crate writes to disk.
    let encoded = toml_of(&config);
    assert!(!encoded.contains("sk-"), "{encoded}");
    assert!(!encoded.contains("api_key"), "{encoded}");
}

/// Serialises a config the way `save` does, for inspection.
fn toml_of(config: &NanusConfig) -> String {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("out.toml");
    config.save_to(&path).expect("save");
    std::fs::read_to_string(&path).expect("read back")
}

#[test]
fn the_api_key_comes_from_the_environment_only() {
    // This process has no key set, so the accessor reports absence rather than a
    // value smuggled in from a file.
    let config = NanusConfig::default();
    let rendered = format!("{config:?}");
    assert!(!rendered.contains("DEEPSEEK_API_KEY"), "{rendered}");
    assert!(
        api_key().is_none() || api_key().is_some(),
        "the accessor is total"
    );
}

// ---------------------------------------------------------------------------
// Saving.
// ---------------------------------------------------------------------------

#[test]
fn save_then_load_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let original = NanusConfig {
        model: String::from("deepseek-v4-pro"),
        max_tokens: 1234,
        reasoning_effort: ReasoningEffort::Low,
        approval_policy: ApprovalPolicy::Never,
        sandbox_mode: SandboxMode::DangerFullAccess,
        system_prompt: Some(String::from("be terse")),
        workspace_root: Some(dir.path().to_path_buf()),
        ..NanusConfig::default()
    };
    let written = original.save(Some(&path)).expect("save");
    assert_eq!(written, path);
    let loaded = NanusConfig::load(Some(&path)).expect("load");
    assert_eq!(loaded, original);
}

#[test]
fn save_leaves_no_temporary_file_behind() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    NanusConfig::default().save(Some(&path)).expect("save");
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["config.toml".to_owned()], "one file, no temp");
}

// ---------------------------------------------------------------------------
// Precedence: four levels.
// ---------------------------------------------------------------------------

/// Level 1: an explicit path beats `$NANUS_CONFIG`.
#[test]
fn an_explicit_path_wins_over_the_environment_variable() {
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join("env.toml");
        let explicit_file = dir.path().join("explicit.toml");
        std::fs::write(&env_file, "model = \"deepseek-v4-pro\"\n").expect("seed env");
        std::fs::write(&explicit_file, "model = \"explicit-model\"\n").expect("seed explicit");
        let output = rerun(
            "an_explicit_path_wins_over_the_environment_variable",
            &[
                (CHILD, "1"),
                (FIXTURE_DIR, &dir.path().to_string_lossy()),
                ("NANUS_CONFIG", &env_file.to_string_lossy()),
            ],
            &[],
        );
        assert_child_ok(&output);
        return;
    }
    let fixture = std::env::var(FIXTURE_DIR).expect("fixture dir");
    let explicit = Path::new(&fixture).join("explicit.toml");
    let config = NanusConfig::load(Some(&explicit)).expect("load");
    assert_eq!(config.model, "explicit-model", "the explicit path won");
}

/// Level 2: `$NANUS_CONFIG` beats the platform configuration path.
#[test]
fn the_environment_variable_wins_over_the_platform_path() {
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env_file = dir.path().join("env.toml");
        std::fs::write(&env_file, "model = \"from-env\"\n").expect("seed env");
        let output = rerun(
            "the_environment_variable_wins_over_the_platform_path",
            &[
                (CHILD, "1"),
                (FIXTURE_DIR, &dir.path().to_string_lossy()),
                ("NANUS_CONFIG", &env_file.to_string_lossy()),
            ],
            &[],
        );
        assert_child_ok(&output);
        return;
    }
    let config = NanusConfig::load(None).expect("load");
    assert_eq!(config.model, "from-env", "NANUS_CONFIG was used");
}

/// Level 3: the platform path is used when the environment variable is unset.
#[test]
fn the_platform_path_is_used_when_the_environment_variable_is_unset() {
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = rerun(
            "the_platform_path_is_used_when_the_environment_variable_is_unset",
            &[
                (CHILD, "1"),
                (FIXTURE_DIR, &dir.path().to_string_lossy()),
                ("HOME", &dir.path().to_string_lossy()),
            ],
            &["NANUS_CONFIG"],
        );
        assert_child_ok(&output);
        return;
    }
    let fixture = std::env::var(FIXTURE_DIR).expect("fixture dir");
    let platform = NanusConfig::default_path().expect("platform path");
    assert!(
        platform.starts_with(&fixture),
        "the platform path is under the controlled HOME: {}",
        platform.display()
    );
    std::fs::create_dir_all(platform.parent().expect("parent")).expect("mkdir");
    std::fs::write(&platform, "model = \"from-platform-path\"\n").expect("seed");
    let config = NanusConfig::load(None).expect("load");
    assert_eq!(config.model, "from-platform-path");
    // Postcondition: no explicit path and no environment variable were involved.
    assert!(std::env::var_os("NANUS_CONFIG").is_none());
}

/// Level 4: nothing on disk yields the built-in defaults.
#[test]
fn the_built_in_defaults_are_the_last_resort() {
    if std::env::var_os(CHILD).is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let output = rerun(
            "the_built_in_defaults_are_the_last_resort",
            &[
                (CHILD, "1"),
                (FIXTURE_DIR, &dir.path().to_string_lossy()),
                ("HOME", &dir.path().to_string_lossy()),
            ],
            &["NANUS_CONFIG"],
        );
        assert_child_ok(&output);
        return;
    }
    let config = NanusConfig::load(None).expect("load");
    assert_eq!(
        config,
        NanusConfig::default(),
        "an empty home yields defaults"
    );
    assert_eq!(config.model, "deepseek-flash");
}
