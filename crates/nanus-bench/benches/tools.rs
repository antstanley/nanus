//! The stock tools, run over a real temporary workspace through the local adapters.
//!
//! A tool call is the one thing in a turn whose cost the model chooses: a `grep` over the
//! whole tree or a `read` of a large file is paid every time the model asks, and a model asks
//! often. These benchmarks run the shipped tools exactly as the loop dispatches them — the
//! public factory, `ToolDefinition::execute`, the real `LocalFs` and `LocalShell` — so what
//! they measure is the tool, its port, and the filesystem, with no model and no loop.
//!
//! Two caveats belong with every number here. The filesystem is real, so a timing includes
//! the operating system's page cache and directory lookups and moves with them. And the
//! allocation counters are process-wide: anything the adapters or the runtime do on another
//! thread during a call — `bash` reaping its child, in particular — is counted against the
//! call, so the allocation figures for these tools are close rather than exact.

use std::path::Path;
use std::rc::Rc;

use criterion::{BatchSize, Criterion, Throughput};
use nanus_bench::{Metric, fixtures};
use nanus_domain::{SandboxMode, ToolCall, ToolCallId, ToolDefinition, ToolOutcome, ToolResult};
use nanus_ports::{FsHandle, SandboxPolicy, ShellHandle};
use serde_json::{Value, json};
use std::hint::black_box;
use tokio::runtime::Runtime;

/// The lines in the file `read` is benchmarked on: past the default window, so the window
/// is what bounds the default read.
const LARGE_FILE_LINES: usize = 5_000;
/// The lines in the file `edit` rewrites.
const EDIT_FILE_LINES: usize = 2_000;
/// The search tree: `DIRS` directories of `SUBDIRS` directories of `FILES` files each.
const DIRS: usize = 10;
const SUBDIRS: usize = 5;
const FILES: usize = 10;
/// The lines in each file of the search tree.
const TREE_FILE_LINES: usize = 200;
/// A word only a handful of files in the tree contain, so `grep` walks everything and
/// reports little: the shape of a model looking for one definition.
const NEEDLE: &str = "NEEDLE_MARKER";

/// A temporary workspace and the ports over it, kept together so the directory outlives
/// every tool built on it.
struct Workspace {
    dir: tempfile::TempDir,
    fs: FsHandle,
    shell: ShellHandle,
    runtime: Runtime,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir()
            .unwrap_or_else(|error| unreachable!("a temporary workspace: {error}"));
        let fs = nanus_adapter_local::LocalFs::new(dir.path())
            .unwrap_or_else(|error| unreachable!("a temporary workspace is readable: {error}"))
            .handle();
        let shell = nanus_adapter_local::LocalShell::new(SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            dir.path(),
        ))
        .handle();
        // Current-thread, so nothing but the routine runs while the counters are read.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| unreachable!("a current-thread runtime: {error}"));
        Self {
            dir,
            fs,
            shell,
            runtime,
        }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn seed(&self, relative: &str, content: &str) {
        let path = self.root().join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|error| unreachable!("creating {relative}'s directory: {error}"));
        }
        std::fs::write(&path, content)
            .unwrap_or_else(|error| unreachable!("seeding {relative}: {error}"));
    }

    /// Runs one call to `tool` to completion.
    fn run(&self, tool: &ToolDefinition, arguments: Value) -> ToolResult {
        let call = ToolCall::new(ToolCallId::new("bench"), tool.name().clone(), arguments);
        self.runtime.block_on(tool.execute(call))
    }

    /// Runs one call and insists it succeeded, so a benchmark never measures a refusal.
    fn check(&self, tool: &ToolDefinition, arguments: &Value) {
        let result = self.run(tool, arguments.clone());
        assert!(
            matches!(result.outcome, ToolOutcome::Success { .. }),
            "{} {arguments} must succeed before it is measured: {:?}",
            tool.name(),
            result.outcome
        );
    }
}

/// Builds the search tree, with the needle in five of its files.
fn search_tree(workspace: &Workspace) {
    let body = fixtures::source_file(TREE_FILE_LINES);
    let mut written = 0_usize;
    for dir in 0..DIRS {
        for sub in 0..SUBDIRS {
            for file in 0..FILES {
                let path = format!("src_{dir}/mod_{sub}/file_{file}.rs");
                // One file in every hundred carries the needle.
                if written % 100 == 7 {
                    workspace.seed(&path, &format!("{body}// {NEEDLE}\n"));
                } else {
                    workspace.seed(&path, &body);
                }
                written = written.saturating_add(1);
            }
        }
    }
    assert_eq!(written, DIRS.saturating_mul(SUBDIRS).saturating_mul(FILES));
}

/// `read`: the default window over a large file, and a narrow window in its middle.
fn read<M: Metric>(c: &mut Criterion<M>) {
    let workspace = Workspace::new();
    let content = fixtures::source_file(LARGE_FILE_LINES);
    workspace.seed("large.rs", &content);
    let tool = nanus_bundle::tools::read_tool(Rc::clone(&workspace.fs));
    let cases = [
        ("default_window", json!({ "file_path": "large.rs" })),
        (
            "window_100",
            json!({ "file_path": "large.rs", "offset": 2_500, "limit": 100 }),
        ),
        (
            "whole_file",
            json!({ "file_path": "large.rs", "limit": LARGE_FILE_LINES }),
        ),
    ];
    let mut group = c.benchmark_group(M::group("tools/read"));
    // The whole file is read from disk in every case; the window decides what is rendered.
    group.throughput(Throughput::Bytes(content.len() as u64));
    for (name, arguments) in cases {
        workspace.check(&tool, &arguments);
        group.bench_function(name, |b| {
            b.iter(|| workspace.run(&tool, black_box(arguments.clone())));
        });
    }
    group.finish();
}

/// `grep` and `glob` over a five-hundred-file tree: a rare needle, a pattern on every file
/// (capped by the tool's match limit), and two globs.
fn search<M: Metric>(c: &mut Criterion<M>) {
    let workspace = Workspace::new();
    search_tree(&workspace);
    let files = DIRS.saturating_mul(SUBDIRS).saturating_mul(FILES) as u64;
    let grep = nanus_bundle::tools::grep_tool(Rc::clone(&workspace.fs));
    let glob = nanus_bundle::tools::glob_tool(Rc::clone(&workspace.fs));
    let cases = [
        ("grep/rare", &grep, json!({ "pattern": NEEDLE })),
        (
            "grep/common_capped",
            &grep,
            json!({ "pattern": "checked_sub" }),
        ),
        (
            "grep/include_rs",
            &grep,
            json!({ "pattern": NEEDLE, "include": "*.rs" }),
        ),
        ("glob/all_rs", &glob, json!({ "pattern": "**/*.rs" })),
        (
            "glob/narrow",
            &glob,
            json!({ "pattern": "src_3/**/file_7.rs" }),
        ),
    ];
    let mut group = c.benchmark_group(M::group("tools/search"));
    group.throughput(Throughput::Elements(files));
    for (name, tool, arguments) in cases {
        workspace.check(tool, &arguments);
        group.bench_function(name, |b| {
            b.iter(|| workspace.run(tool, black_box(arguments.clone())));
        });
    }
    group.finish();
}

/// `edit` and `write`: an exactly-once replacement in a two-thousand-line file, and a
/// whole-file overwrite of the same size.
fn modify<M: Metric>(c: &mut Criterion<M>) {
    let workspace = Workspace::new();
    let original = fixtures::source_file(EDIT_FILE_LINES);
    // A line that occurs once, so the edit is the exactly-once case rather than a refusal.
    let unique = "    let index = input.len().checked_sub(1004)?;";
    assert_eq!(
        original.matches(unique).count(),
        1,
        "the edit target is unique"
    );
    workspace.seed("edit.rs", &original);
    let edit = nanus_bundle::tools::edit_tool(Rc::clone(&workspace.fs));
    let write = nanus_bundle::tools::write_tool(Rc::clone(&workspace.fs));
    let edit_arguments = json!({
        "file_path": "edit.rs",
        "old_string": unique,
        "new_string": "    let index = input.len().saturating_sub(1004);",
    });
    let write_arguments = json!({
        "file_path": "write.rs",
        "content": original,
        "mode": "overwrite",
    });
    workspace.seed("write.rs", "");
    workspace.check(&edit, &edit_arguments);
    workspace.check(&write, &write_arguments);

    let mut group = c.benchmark_group(M::group("tools/modify"));
    group.throughput(Throughput::Bytes(original.len() as u64));
    group.bench_function("edit", |b| {
        // Each iteration needs the original back, so the reset runs before every call and
        // outside the measurement; batching it would leave all but the first edit refused.
        b.iter_batched(
            || workspace.seed("edit.rs", &original),
            |()| workspace.run(&edit, edit_arguments.clone()),
            BatchSize::PerIteration,
        );
    });
    group.bench_function("write_overwrite", |b| {
        b.iter(|| workspace.run(&write, black_box(write_arguments.clone())));
    });
    group.finish();
}

/// `bash` running `true`: the floor every shell call pays — a process group, a spawn, a
/// wait, and the captured output — before the command does any work at all.
fn bash<M: Metric>(c: &mut Criterion<M>) {
    let workspace = Workspace::new();
    let tool = nanus_bundle::tools::bash_tool(Rc::clone(&workspace.shell));
    let mut group = c.benchmark_group(M::group("tools/bash"));
    group.sample_size(20);
    for (name, command) in [("true", "true"), ("echo", "echo hello")] {
        let arguments = json!({ "command": command });
        workspace.check(&tool, &arguments);
        group.bench_function(name, |b| {
            b.iter(|| workspace.run(&tool, black_box(arguments.clone())));
        });
    }
    group.finish();
}

nanus_bench::benches!(read, search, modify, bash);
