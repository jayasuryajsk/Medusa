use super::*;

impl App {
    pub(super) fn kill_background_job(&mut self, id: &str) {
        let Some(job) = self.background_jobs.get(id) else {
            self.toast("Unknown background job", ToastKind::Error);
            self.status_line = "unknown job".to_string();
            return;
        };
        if job.state != ToolRunState::Running {
            self.toast("Job is not running", ToastKind::Warning);
            self.status_line = "job is not running".to_string();
            return;
        }
        let pid = job.pid;
        #[cfg(unix)]
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
        #[cfg(not(unix))]
        let status = Command::new("kill").arg(pid.to_string()).status();
        match status {
            Ok(status) if status.success() => {
                self.status_line = format!("kill sent · {id}");
                self.toast(self.status_line.clone(), ToastKind::Success);
            }
            Ok(status) => {
                self.status_line = format!("kill failed · exit {}", status.code().unwrap_or(-1));
                self.toast(self.status_line.clone(), ToastKind::Error);
            }
            Err(error) => {
                self.status_line = format!("kill failed: {error}");
                self.toast(self.status_line.clone(), ToastKind::Error);
            }
        }
    }

    pub(super) fn tail_background_job(&mut self, id: &str) {
        let Some(job) = self.background_jobs.get(id) else {
            self.toast("Unknown background job", ToastKind::Error);
            return;
        };
        let text = if job.last_output.trim().is_empty() {
            format!(
                "job {id}\npid: {}\ncommand: {}\noutput: <not available yet>",
                job.pid, job.command
            )
        } else {
            format!(
                "job {id}\npid: {}\ncommand: {}\n\n{}",
                job.pid, job.command, job.last_output
            )
        };
        self.transcript
            .push(TranscriptItem::Message(ChatMessage::system(text)));
        self.touch_transcript();
        self.status_line = format!("tailed job {id}");
    }

    pub(super) fn restart_background_job(&mut self, id: &str) {
        let Some(job) = self.background_jobs.get(id) else {
            self.toast("Unknown background job", ToastKind::Error);
            return;
        };
        let command = job.command.clone();
        self.start_exec_command(&command, true);
    }

    /// A runtime for tools the user invokes directly (/exec, /patch). The
    /// user typing the command IS the approval, so NeedsApproval auto-allows;
    /// hard denies still block. Uses an immediate closure (no channel) so it
    /// can run on the UI thread without deadlocking.
    pub(super) fn user_tools(&self) -> ToolRuntime {
        self.tools
            .clone()
            .with_approval_handler(Arc::new(|_request| ApprovalDecision::AllowOnce))
    }

    pub(super) fn start_exec_command(&mut self, command: &str, background: bool) {
        // A foreground /exec blocks the UI thread until the child exits, which
        // would also stall approval servicing for any running turn/workflow.
        if !background && (self.is_working() || self.has_active_workflows()) {
            self.status_line =
                "finish the current turn before running a foreground /exec".to_string();
            self.toast(
                "Busy — use /exec … & for background, or wait",
                ToastKind::Warning,
            );
            return;
        }
        self.push_tool_start("terminal.exec".to_string(), format!("$ {command}"));
        let request = TerminalExecRequest {
            command: command.to_string(),
            cwd: None,
            background,
            unsandboxed: false,
        };
        match self
            .user_tools()
            .with_background_events(self.background_job_sender.clone())
            .terminal_exec(request)
        {
            Ok(result) => {
                if result.background {
                    if let Some(id) = result.job_id.as_deref() {
                        self.attach_or_push_background_tool_start(id, command);
                        self.update_tool_result_by_id(
                            id,
                            ToolRunState::Running,
                            &terminal_result_output(&result),
                        );
                    } else {
                        self.push_tool_result("terminal.exec", terminal_result_output(&result));
                    }
                } else {
                    self.push_tool_result("terminal.exec", terminal_result_output(&result));
                }
                self.status_line = if result.background {
                    format!("terminal.exec background · pid {}", result.pid.unwrap_or(0))
                } else {
                    format!("terminal.exec exit {}", result.code.unwrap_or(-1))
                };
                self.toast(self.status_line.clone(), ToastKind::Success);
            }
            Err(error) => {
                self.push_tool_result("terminal.exec", format!("error: {error}"));
                self.status_line = "terminal.exec failed".to_string();
                self.toast("Command failed", ToastKind::Error);
            }
        }
    }
}
