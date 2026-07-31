use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use color_eyre::eyre::{Result, WrapErr, bail};
use serde::Serialize;

use super::{WorkflowEvent, WorkflowRunReport};
use crate::persistence::{atomic_write_private, ensure_private_dir};

const WORKFLOW_RUNS_DIR: &str = ".medusa/workflow-runs";

#[derive(Debug, Serialize)]
struct JournalRecord<'a> {
    sequence: u64,
    timestamp_ms: u128,
    event: &'a WorkflowEvent,
}

/// Append-only event log plus an atomic final report for one workflow run.
pub(crate) struct WorkflowJournal {
    directory: PathBuf,
    events: File,
    sequence: u64,
}

impl WorkflowJournal {
    pub(crate) fn start(workspace: &Path, run_id: &str) -> Result<Self> {
        validate_run_id(run_id)?;
        let directory = workspace.join(WORKFLOW_RUNS_DIR).join(run_id);
        ensure_private_dir(&directory)?;
        let event_path = directory.join("events.jsonl");
        let events = private_append_file(&event_path)?;

        Ok(Self {
            directory,
            events,
            sequence: 0,
        })
    }

    pub(crate) fn append(&mut self, event: &WorkflowEvent) -> Result<()> {
        self.sequence = self.sequence.saturating_add(1);
        let record = JournalRecord {
            sequence: self.sequence,
            timestamp_ms: now_ms(),
            event,
        };
        serde_json::to_writer(&mut self.events, &record)
            .wrap_err("failed to encode workflow journal event")?;
        self.events
            .write_all(b"\n")
            .wrap_err("failed to terminate workflow journal event")?;
        self.events
            .flush()
            .wrap_err("failed to flush workflow journal")?;
        self.events
            .sync_data()
            .wrap_err("failed to sync workflow journal")?;
        Ok(())
    }

    pub(crate) fn finish(&mut self, report: &WorkflowRunReport) -> Result<()> {
        let json =
            serde_json::to_vec_pretty(report).wrap_err("failed to encode workflow report")?;
        atomic_write_private(&self.directory.join("report.json"), json)
            .wrap_err("failed to persist workflow report")
    }
}

fn private_append_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .wrap_err_with(|| format!("failed to open workflow journal {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .wrap_err_with(|| format!("failed to secure workflow journal {}", path.display()))?;
    }
    Ok(file)
}

fn validate_run_id(run_id: &str) -> Result<()> {
    if run_id.is_empty()
        || !run_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        bail!("invalid workflow run id {run_id:?}");
    }
    Ok(())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{WorkflowStatus, workflow_run_id};

    #[test]
    fn journal_is_append_only_and_report_is_atomic() {
        let workspace = std::env::temp_dir().join(format!(
            "medusa-workflow-journal-{}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&workspace).unwrap();
        let run_id = workflow_run_id();
        let mut journal = WorkflowJournal::start(&workspace, &run_id).unwrap();
        let started = WorkflowEvent::RunStarted {
            run_id: run_id.clone(),
            title: "test".into(),
            task: "test durability".into(),
        };
        let finished = WorkflowEvent::RunFinished {
            run_id: run_id.clone(),
            status: WorkflowStatus::Succeeded,
            summary: "done".into(),
        };
        journal.append(&started).unwrap();
        journal.append(&finished).unwrap();
        let report = WorkflowRunReport {
            run_id: run_id.clone(),
            title: "test".into(),
            task: "test durability".into(),
            phases: Vec::new(),
            summary: "done".into(),
            status: WorkflowStatus::Succeeded,
        };
        journal.finish(&report).unwrap();

        let directory = workspace.join(WORKFLOW_RUNS_DIR).join(run_id);
        let events = fs::read_to_string(directory.join("events.jsonl")).unwrap();
        assert_eq!(events.lines().count(), 2);
        assert!(events.contains("\"sequence\":1"));
        assert!(events.contains("\"sequence\":2"));
        let persisted: WorkflowRunReport =
            serde_json::from_str(&fs::read_to_string(directory.join("report.json")).unwrap())
                .unwrap();
        assert_eq!(persisted, report);
    }

    #[test]
    fn run_ids_cannot_escape_the_state_directory() {
        assert!(validate_run_id("../escape").is_err());
        assert!(validate_run_id("nested/run").is_err());
    }
}
