use super::*;

impl ToolRuntime {
    pub fn terminal_exec(&self, request: TerminalExecRequest) -> Result<TerminalExecResult> {
        self.terminal_exec_gated(request, false)
    }

    /// `preapproved` skips the interactive gate (never the hard denies); used
    /// by explore probes that already passed the read-only probe allowlist.
    pub(crate) fn terminal_exec_gated(
        &self,
        request: TerminalExecRequest,
        preapproved: bool,
    ) -> Result<TerminalExecResult> {
        let check = self.permissions.evaluate_terminal_command(&request.command);
        if request.unsandboxed {
            if self.permissions.effective_mode() == PermissionMode::Readonly {
                bail!(
                    "terminal.exec sandbox escalation refused: readonly mode never runs commands unsandboxed"
                );
            }
            // Escaping the sandbox always takes a fresh human decision, even
            // for commands that would otherwise auto-run. Hard denies stay
            // hard.
            let check = match check {
                PermissionCheck::Deny(reason) => PermissionCheck::Deny(reason),
                PermissionCheck::Allow | PermissionCheck::NeedsApproval => {
                    PermissionCheck::NeedsApproval
                }
            };
            self.authorize(check, || ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some(request.command.clone()),
                paths: Vec::new(),
                background: request.background,
                sandbox_escalation: true,
            })?;
        } else if preapproved && check == PermissionCheck::NeedsApproval {
            // probe allowlist already vetted this as read-only
        } else {
            self.authorize(check, || ApprovalRequest {
                tool: ApprovalTool::TerminalExec,
                command: Some(request.command.clone()),
                paths: Vec::new(),
                background: request.background,
                sandbox_escalation: false,
            })?;
        }
        let cwd = self.resolve_workspace_path(request.cwd.as_deref())?;
        // `preapproved` is exactly the explore-probe path: those read-only
        // probes always sandbox strictly (network denied) when available.
        let strict = preapproved;

        if request.background {
            let (command, sandboxed) =
                self.build_shell_command(&request.command, &cwd, strict, request.unsandboxed)?;
            let child = crate::proc::spawn_command(command)
                .wrap_err_with(|| format!("failed to start command: {}", request.command))?;

            let pid = child.id();
            let id = background_job_id(pid, &request.command, &cwd);
            let command = request.command.clone();
            let event_cwd = cwd.clone();
            if let Some(sender) = self.background_events.clone() {
                let _ = sender.send(BackgroundJobEvent::Started {
                    id: id.clone(),
                    pid,
                    command: command.clone(),
                    cwd: event_cwd.clone(),
                });
                let finish_id = id.clone();
                let fail_id = id.clone();
                thread::spawn(move || {
                    let event = match crate::proc::wait_for_child(child) {
                        Ok(output) => BackgroundJobEvent::Finished {
                            id: finish_id,
                            pid,
                            command,
                            cwd: event_cwd,
                            code: output.code,
                            stdout: output.stdout,
                            stderr: output.stderr,
                        },
                        Err(error) => BackgroundJobEvent::Failed {
                            id: fail_id,
                            pid,
                            command,
                            cwd: event_cwd,
                            error: error.to_string(),
                        },
                    };
                    let _ = sender.send(event);
                });
            } else {
                thread::spawn(move || {
                    let _ = crate::proc::wait_for_child(child);
                });
            }

            return Ok(TerminalExecResult {
                command: request.command,
                cwd,
                code: Some(0),
                stdout: String::new(),
                stderr: String::new(),
                background: true,
                pid: Some(pid),
                job_id: Some(id),
                sandboxed,
            });
        }

        // Foreground runs are cancellable (background jobs deliberately are
        // not: they outlive the turn by design). Never spawn after cancel.
        self.cancel.bail_if_cancelled()?;

        let (command, sandboxed) =
            self.build_shell_command(&request.command, &cwd, strict, request.unsandboxed)?;
        let outcome = crate::proc::run_command(command, None, &self.cancel)
            .wrap_err_with(|| format!("failed to run command: {}", request.command))?;
        if outcome.cancelled {
            bail!("cancelled: interrupted by user");
        }

        Ok(TerminalExecResult {
            command: request.command,
            cwd,
            code: outcome.code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            background: false,
            pid: None,
            job_id: None,
            sandboxed,
        })
    }

    /// Build `$SHELL -lc <command>` for both terminal_exec paths, wrapped in
    /// the platform sandbox when the policy (or a strict explore probe) asks
    /// for it. Sandbox-required commands fail closed when the backend is
    /// unavailable; only an explicit, approved escalation gets a plain shell.
    pub(crate) fn build_shell_command(
        &self,
        command_text: &str,
        cwd: &Path,
        strict: bool,
        unsandboxed: bool,
    ) -> Result<(Command, bool)> {
        let shell = std::env::var_os("SHELL").unwrap_or_else(|| OsStr::new("sh").to_os_string());
        if !unsandboxed && (self.sandbox.should_sandbox() || strict) {
            match crate::sandbox::sandbox_availability() {
                SandboxAvailability::Available => {
                    let spec = self.sandbox.spec(&self.workspace, strict);
                    return Ok((
                        crate::sandbox::wrap_command(&spec, &shell, command_text, cwd),
                        true,
                    ));
                }
                SandboxAvailability::Broken(reason) => {
                    bail!(
                        "sandbox-required command blocked: {reason}. Install/fix the platform sandbox, switch to open permissions, or request an explicit unsandboxed run for user approval"
                    );
                }
                SandboxAvailability::UnsupportedPlatform => {
                    bail!(
                        "sandbox-required command blocked: this platform has no supported Medusa sandbox. Switch to open permissions or request an explicit unsandboxed run for user approval"
                    );
                }
            }
        }

        let mut command = Command::new(shell);
        command.arg("-lc").arg(command_text).current_dir(cwd);
        Ok((command, false))
    }
}
