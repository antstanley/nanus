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
    CONFIG_VERSION, DEFAULT_MAX_PARALLEL_TOOLS, DEFAULT_MAX_STEPS_PER_TURN, NanusConfig,
    ReasoningEffort, TuiDetail,
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
    // No model is named by default: absent means "the provider's own default", and a
    // schema that named one provider's model would be unusable with another.
    assert!(config.model.is_none(), "no model is named by default");
    assert!(config.provider.is_none(), "no provider is named by default");
    assert!(config.plan.is_none(), "no plan is named by default");
    assert!(config.base_url.is_none(), "no endpoint is named by default");
    assert_eq!(
        config.max_tokens, None,
        "an unset output budget is the model's own ceiling"
    );
    assert_eq!(
        config.context_budget, None,
        "an unset context budget is the model's own window"
    );
    assert_eq!(config.max_steps_per_turn, DEFAULT_MAX_STEPS_PER_TURN);
    assert_eq!(config.max_parallel_tools, DEFAULT_MAX_PARALLEL_TOOLS);
    assert_eq!(
        config.reasoning_effort, None,
        "no step is named by default, so a plan's own default can apply"
    );
    assert_eq!(
        ReasoningEffort::default().to_port(),
        nanus_ports::ReasoningEffort::Medium
    );
    assert_eq!(config.approval_policy, ApprovalPolicy::PerCall);
    assert!(
        config.approved_tools.is_empty(),
        "no tool is approved for every session unless the file names it"
    );
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
    assert!(
        config.remember_selection,
        "a change of selection is remembered unless the file says otherwise"
    );
    assert!(
        config.selection_file.is_none(),
        "the record's place is the home's unless the file names one"
    );
    assert!(
        config.home.is_none(),
        "the home is the environment's or the platform's unless the file names one"
    );
    assert!(
        config.credential_service.is_none(),
        "credentials are filed under the shared name unless the file names another"
    );
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
    assert_eq!(config.model.as_deref(), Some("deepseek-v4-pro"));
    assert_eq!(config.max_tokens, None, "unset fields default");
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
        "reasoning_effort = \"minimal\"\napproval_policy = \"permitted\"\nsandbox_mode = \"workspace_write\"\n",
    )
    .expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    assert_eq!(config.reasoning_effort, Some(ReasoningEffort::Minimal));
    assert_eq!(config.approval_policy, ApprovalPolicy::Permitted);
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
fn standing_tool_approvals_are_read_and_survive_a_save() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(&path, "approved_tools = [\"bash\", \"write\"]\n").expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    assert_eq!(config.approved_tools, ["bash", "write"]);
    assert_eq!(
        config.approval_policy,
        ApprovalPolicy::PerCall,
        "naming tools does not move the state"
    );
    config.save_to(&path).expect("save");
    assert_eq!(NanusConfig::load_from(&path).expect("reload"), config);

    // A file written before the field existed approves nothing, at the same version.
    std::fs::write(&path, "config_version = 1\nmodel = \"deepseek-flash\"\n").expect("seed");
    let older = NanusConfig::load(Some(&path)).expect("load");
    assert!(older.approved_tools.is_empty());
    assert_eq!(older.config_version, CONFIG_VERSION);

    // The other direction: a single name rather than a list is refused, not read as one entry.
    std::fs::write(&path, "approved_tools = \"bash\"\n").expect("seed invalid");
    assert!(
        NanusConfig::load_from(&path).is_err(),
        "a tool list that is not a list is refused rather than guessed at"
    );
}

#[test]
fn the_remembered_selection_can_be_moved_or_turned_off_and_survives_a_save() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "remember_selection = false\nselection_file = \"/elsewhere/selection.toml\"\n",
    )
    .expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    assert!(!config.remember_selection);
    assert_eq!(
        config.selection_file.as_deref(),
        Some(Path::new("/elsewhere/selection.toml"))
    );
    config.save_to(&path).expect("save");
    assert_eq!(NanusConfig::load_from(&path).expect("reload"), config);

    // A file written before the fields existed reads as the defaults, at the same version: an
    // added field with a default is not a migration.
    std::fs::write(&path, "config_version = 1\nmodel = \"deepseek-flash\"\n").expect("seed");
    let older = NanusConfig::load(Some(&path)).expect("load");
    assert!(older.remember_selection);
    assert_eq!(older.selection_file, None);
    assert_eq!(older.config_version, CONFIG_VERSION);

    std::fs::write(&path, "remember_selection = \"no\"\n").expect("seed invalid");
    assert!(
        NanusConfig::load_from(&path).is_err(),
        "a switch that is not a boolean is refused rather than read as on"
    );
}

#[test]
fn a_home_and_a_credential_service_are_read_and_survive_a_save() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "home = \"/srv/example-host/nanus\"\ncredential_service = \"example-host\"\n",
    )
    .expect("seed");
    let config = NanusConfig::load(Some(&path)).expect("load");
    assert_eq!(
        config.home.as_deref(),
        Some(Path::new("/srv/example-host/nanus"))
    );
    assert_eq!(config.credential_service.as_deref(), Some("example-host"));
    config.save_to(&path).expect("save");
    assert_eq!(NanusConfig::load_from(&path).expect("reload"), config);

    // A file written before the fields existed reads as the defaults, at the same version.
    std::fs::write(&path, "config_version = 1\nmodel = \"deepseek-flash\"\n").expect("seed");
    let older = NanusConfig::load(Some(&path)).expect("load");
    assert_eq!(older.home, None);
    assert_eq!(older.credential_service, None);
    assert_eq!(older.config_version, CONFIG_VERSION);

    std::fs::write(&path, "credential_service = 7\n").expect("seed invalid");
    assert!(
        NanusConfig::load_from(&path).is_err(),
        "a service name that is not a string is refused rather than read as the default"
    );
}

#[test]
fn complete_tui_palettes_and_native_mouse_selection_round_trip() {
    use nanus_adapter_config::TuiTheme;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    for (spelling, theme) in [
        ("terminal", TuiTheme::Terminal),
        ("light", TuiTheme::Light),
        ("dark", TuiTheme::Dark),
        ("high_contrast", TuiTheme::HighContrast),
    ] {
        std::fs::write(
            &path,
            format!("tui_theme = \"{spelling}\"\ntui_mouse = false\n"),
        )
        .expect("seed");
        let config = NanusConfig::load(Some(&path)).expect("load");
        assert_eq!(config.tui_theme, theme);
        assert_eq!(config.tui_theme.to_string(), spelling);
        assert!(!config.tui_mouse);
        config.save_to(&path).expect("save");
        assert_eq!(NanusConfig::load_from(&path).expect("reload"), config);
    }
    std::fs::write(&path, "tui_theme = \"unknown\"\n").expect("seed invalid");
    assert!(
        NanusConfig::load_from(&path).is_err(),
        "a mistyped preference is refused"
    );
    let default = NanusConfig::default();
    assert_eq!(default.tui_theme, TuiTheme::Terminal);
    assert!(default.tui_mouse);
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
        config.max_tokens,
        Some(4096),
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
    assert_eq!(config.max_tokens, Some(777));
    assert_eq!(config.config_version, CONFIG_VERSION);
    // The migration is idempotent in effect: a document that already has the new
    // field keeps its value rather than being overwritten by a stale one.
    document = serde_json::json!({ "max_output_tokens": 111, "max_tokens": 222 });
    let config = NanusConfig::migrate(document).expect("migrate");
    assert_eq!(config.max_tokens, Some(222), "the existing field wins");
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

// ---------------------------------------------------------------------------
// Saving.
// ---------------------------------------------------------------------------

#[test]
fn save_then_load_round_trips() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("config.toml");
    let original = NanusConfig {
        provider: Some(String::from("zai")),
        plan: Some(String::from("coding")),
        base_url: Some(String::from("https://api.z.ai/api/coding/paas/v4")),
        model: Some(String::from("glm-4.5")),
        max_tokens: Some(1234),
        context_budget: Some(200_000),
        reasoning_effort: Some(ReasoningEffort::Low),
        approval_policy: ApprovalPolicy::AllCalls,
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
    assert_eq!(
        config.model.as_deref(),
        Some("explicit-model"),
        "the explicit path won"
    );
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
    assert_eq!(
        config.model.as_deref(),
        Some("from-env"),
        "NANUS_CONFIG was used"
    );
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
    assert_eq!(config.model.as_deref(), Some("from-platform-path"));
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
    assert!(config.model.is_none(), "an empty home names no model");
}
