use super::support::{command_paths_outside_workspace, validate_explore_terminal_command};
use super::*;
use crate::cancel::CancelToken;
use crate::checkpoint::CheckpointRecorder;
use crate::mcp::McpRegistry;
use crate::sandbox::SandboxAvailability;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

fn write_permissions(workspace: &Path, json: &str) {
    fs::create_dir_all(workspace.join(".medusa")).unwrap();
    fs::write(workspace.join(".medusa/permissions.json"), json).unwrap();
}

fn ask_workspace() -> PathBuf {
    let workspace = temp_workspace();
    write_permissions(&workspace, r#"{"mode":"ask","sandbox":{"enabled":false}}"#);
    workspace
}

fn temp_workspace() -> PathBuf {
    static TEMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let index = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("medusa-tools-test-{pid}-{suffix}-{index}"));
    fs::create_dir_all(&path).unwrap();
    crate::permissions::PermissionPolicy::write_mode(
        &path,
        crate::permissions::PermissionMode::Open,
    )
    .unwrap();
    path
}

mod checkpoints;
mod filesystem;
mod mcp;
mod mutations;
mod planning;
mod terminal;
mod validation;
