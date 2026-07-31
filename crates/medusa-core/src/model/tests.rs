use super::{
    DirectCodexBackend, exec, retry_backoff, retryable_status, schema, sleep_with_cancel, types,
    wire, with_ultra_orchestration_context,
};
use crate::harness::HarnessPolicy;
use crate::tools::ToolRuntime;
use serde_json::{Value, json};
use std::{fs, path::Path};

fn temp_workspace() -> std::path::PathBuf {
    static TEMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let index = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("medusa-model-test-{pid}-{suffix}-{index}"));
    fs::create_dir_all(&path).unwrap();
    crate::permissions::PermissionPolicy::write_mode(
        &path,
        crate::permissions::PermissionMode::Open,
    )
    .unwrap();
    path.canonicalize().unwrap()
}

mod context;
mod execution;
mod mcp;
mod streaming;
mod summaries;
