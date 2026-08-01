use super::*;
#[cfg(test)]
use std::sync::atomic::Ordering;

impl App {
    pub(crate) fn new(startup_session: SessionOpenMode) -> Result<Self> {
        Self::with_model_backend_and_session(true, startup_session)
    }

    #[cfg(test)]
    pub(crate) fn with_model_backend(model_enabled: bool) -> Self {
        // Each test app gets its own workspace: parallel tests sharing the
        // real cwd raced on .medusa/permissions.json and flaked.
        use std::sync::atomic::AtomicU64;
        static NEXT_TEST_WORKSPACE: AtomicU64 = AtomicU64::new(0);
        let dir = env::temp_dir().join(format!(
            "medusa-test-{}-{}",
            std::process::id(),
            NEXT_TEST_WORKSPACE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("test workspace should be creatable");
        Self::build_in(model_enabled, None, Some(dir))
    }

    pub(crate) fn with_model_backend_and_session(
        model_enabled: bool,
        startup_session: SessionOpenMode,
    ) -> Result<Self> {
        let cwd = env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
        let session = SessionStore::open(&cwd, startup_session)?;
        let transcript = session.load_transcript_with_legacy(TranscriptItem::Message)?;
        let mut app = Self::try_build_in(model_enabled, Some(session), None)?;
        if !transcript.is_empty() {
            app.transcript = transcript;
            app.touch_transcript();
            app.status_line = "session restored".to_string();
        } else {
            app.persist_session();
        }
        Ok(app)
    }

    #[cfg(test)]
    pub(crate) fn build(model_enabled: bool, session: Option<SessionStore>) -> Self {
        Self::build_in(model_enabled, session, None)
    }

    #[cfg(test)]
    pub(crate) fn build_in(
        model_enabled: bool,
        session: Option<SessionStore>,
        workspace: Option<PathBuf>,
    ) -> Self {
        Self::try_build_in(model_enabled, session, workspace)
            .expect("test app model gateway should initialize")
    }

    fn try_build_in(
        model_enabled: bool,
        session: Option<SessionStore>,
        workspace: Option<PathBuf>,
    ) -> Result<Self> {
        let cwd = workspace
            .or_else(|| env::current_dir().ok())
            .unwrap_or_else(|| Path::new(".").to_path_buf());
        let tools = ToolRuntime::new(&cwd).expect("current directory should be usable");
        let (mcp, mcp_load_error) = match McpRegistry::load(tools.workspace()) {
            Ok(registry) => (registry, None),
            Err(error) => (McpRegistry::empty(), Some(error.to_string())),
        };
        let tools = tools.with_mcp(mcp.clone());
        // Deliberately do NOT prewarm MCP servers here: spawning a server runs
        // an arbitrary command from `.medusa/mcp.json`, and a freshly-cloned
        // untrusted repo must not execute those without a human click. Servers
        // start lazily on first use, gated by a launch approval in every
        // confined mode (Open mode trusts the workspace config). See
        // ToolRuntime::mcp_tool_schemas / authorize_mcp_launch.
        let app_settings = load_app_settings(tools.workspace()).unwrap_or_default();
        let mut model = ModelGateway::new(tools.workspace().to_path_buf())
            .wrap_err("failed to initialize model providers")?;
        if env::var_os("MEDUSA_MODEL").is_none()
            && let Some(model_name) = app_settings.model()
        {
            let _ = model.try_set_model_name(model_name);
        }
        // Env override wins for one-off launches; else the saved preference.
        if env::var_os("MEDUSA_REASONING_EFFORT").is_none()
            && let Some(effort) = app_settings.reasoning_effort()
        {
            model.set_reasoning_effort(effort);
        }
        let cwd_display = abbreviate_home(&tools.workspace().to_string_lossy());
        let inside_git_repo = Path::new(".git").exists();
        let theme = ThemeKind::from_workspace_settings(tools.workspace());
        let permission_mode = app_settings.permission_mode();
        set_active_theme(theme);
        let (background_job_sender, background_job_events) = mpsc::channel();
        let (approval_sender, approval_events) = mpsc::channel::<PendingApproval>();
        let approval_handler: medusa_core::tools::ApprovalHandler =
            Arc::new(move |request: ApprovalRequest| {
                let (respond, decision) = mpsc::channel();
                if approval_sender
                    .send(PendingApproval { request, respond })
                    .is_err()
                {
                    return ApprovalDecision::Deny;
                }
                decision.recv().unwrap_or(ApprovalDecision::Deny)
            });

        let mut app = Self {
            input: String::new(),
            input_cursor: 0,
            pending_attachments: Vec::new(),
            attachment_previews: HashMap::new(),
            image_renderer: TerminalImageRenderer::detect(),
            transcript: Vec::new(),
            transcript_version: 0,
            transcript_rows_cache: None,
            status_line: "Ready.".to_string(),
            last_chat_viewport: None,
            last_transcript_rows: Arc::new(Vec::new()),
            should_quit: false,
            restart_requested: false,
            cwd_display,
            inside_git_repo,
            theme,
            permission_mode,
            tools,
            mcp,
            mcp_statuses: Vec::new(),
            agent_registry: AgentRegistry::default(),
            context_engine: ContextEngine::new(),
            plan_mode: false,
            last_escape_at: None,
            model,
            model_enabled,
            model_events: None,
            workflow_events: Vec::new(),
            background_job_sender,
            approval_handler,
            approval_events,
            approval_queue: VecDeque::new(),
            session_terminal_grants: Vec::new(),
            session_edit_grants: Vec::new(),
            session_web_egress_allowed: false,
            denied_this_turn: Vec::new(),
            approval_shown_at: None,
            denied_edits_this_turn: Vec::new(),
            background_job_events,
            background_jobs: BTreeMap::new(),
            streaming_message: None,
            queued_turns: VecDeque::new(),
            turn_cancel: None,
            cancel_requested_at: None,
            last_stream_save: Instant::now(),
            chat_scroll: 0,
            chat_scroll_target: 0,
            selected_tool: None,
            decision_selection: 0,
            workflows: Vec::new(),
            animation_tick: 0,
            started_at: Instant::now(),
            turn_started_at: None,
            session,
            active_modal: None,
            slash_selection: 0,
            mention_selection: 0,
            mention_files: None,
            mention_dismissed: false,
            bell_setting: app_settings.bell.unwrap_or(true),
            settings_selection: 0,
            model_selection: 0,
            reasoning_selection: 0,
            model_picker_pane: ModelPickerPane::Models,
            permission_selection: permission_mode_index(permission_mode),
            theme_selection: theme_index(theme),
            image_preview_index: 0,
            image_preview_zoom: 100,
            theme_preview_original: None,
            toast: None,
            session_usage: TokenUsage::default(),
            session_requests: 0,
            turn_usage: TokenUsage::default(),
            turn_requests: 0,
            last_turn_usage: TokenUsage::default(),
            last_turn_requests: 0,
            context_report: None,
            compact_events: None,
            active_checkpoint: None,
            #[cfg(test)]
            last_turn_runtime: None,
            #[cfg(test)]
            last_workflow_runtime: None,
            rewind_entries: Vec::new(),
            rewind_selection: 0,
            rewind_stage: RewindStage::Pick,
            rewind_confirm_selection: 0,
            edit_picker_entries: Vec::new(),
            edit_picker_selection: 0,
            review_diff_check: workspace_has_reviewable_diff,
        };
        if let Some(error) = mcp_load_error {
            app.toast(format!("MCP config ignored: {error}"), ToastKind::Warning);
        }
        Ok(app)
    }

    pub(crate) fn run(&mut self, terminal: &mut Tui) -> Result<()> {
        let mut needs_draw = true;
        let mut terminal_changed = true;
        let mut last_draw = Instant::now() - Duration::from_secs(1);

        while !self.should_quit {
            terminal_changed |= self.drain_terminal_events(Duration::ZERO)?;
            if self.should_quit {
                break;
            }

            let previous_animation_tick = self.animation_tick;
            self.animation_tick = self.animation_frame();
            let animated = self.has_active_animation();
            let animation_changed = animated && self.animation_tick != previous_animation_tick;
            let toast_changed = self.expire_toast();
            let model_changed = self.drain_model_events();
            let workflow_changed = self.drain_workflow_events();
            let background_changed = self.drain_background_job_events();
            let approval_changed = self.drain_approval_requests();
            let pending_tool_changed = self.drain_pending_tool_results();
            let compact_changed = self.drain_compact_events();

            needs_draw |= terminal_changed
                || toast_changed
                || model_changed
                || workflow_changed
                || background_changed
                || approval_changed
                || pending_tool_changed
                || compact_changed
                || animation_changed;

            let frame_cadence = if animated {
                Duration::from_millis(16)
            } else {
                Duration::from_millis(50)
            };

            if needs_draw && (terminal_changed || last_draw.elapsed() >= frame_cadence) {
                crate::terminal::draw_synchronized(terminal, |terminal| {
                    terminal.draw(|frame| self.draw(frame)).map(|_| ())
                })?;
                last_draw = Instant::now();
                needs_draw = false;
                terminal_changed = false;
            }

            self.clamp_chat_scroll_to_viewport();
            let poll_interval = if needs_draw {
                frame_cadence
                    .saturating_sub(last_draw.elapsed())
                    .min(Duration::from_millis(16))
            } else if animated {
                Duration::from_millis(16)
            } else if self.toast.is_some() {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(250)
            };

            terminal_changed |= self.drain_terminal_events(poll_interval)?;
        }

        Ok(())
    }

    pub(super) fn drain_terminal_events(&mut self, initial_timeout: Duration) -> Result<bool> {
        if !event::poll(initial_timeout)? {
            return Ok(false);
        }

        let event = event::read()?;
        let should_draw = event_requests_immediate_draw(&event);
        self.handle_terminal_event(event);
        if should_draw {
            return Ok(true);
        }

        for _ in 0..128 {
            if self.should_quit || !event::poll(Duration::ZERO)? {
                break;
            }
            let event = event::read()?;
            let should_draw = event_requests_immediate_draw(&event);
            self.handle_terminal_event(event);
            if should_draw {
                break;
            }
        }

        Ok(true)
    }

    pub(super) fn handle_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key),
            Event::Paste(text) => self.handle_paste(text),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            _ => {}
        }
    }
}
