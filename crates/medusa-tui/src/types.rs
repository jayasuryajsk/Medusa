use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{Receiver, Sender},
    },
    time::Instant,
};

use crate::config::ThemeKind;
use crate::constants::MIN_TOOL_PULSE_VISIBLE;
use medusa_core::cancel::CancelToken;
use medusa_core::checkpoint::CheckpointRecorder;
use medusa_core::tools::{ApprovalDecision, ApprovalRequest};
use medusa_core::workflow::{SubagentToolPolicy, WorkflowEvent};
use ratatui::{Frame, layout::Rect, text::Line};
use ratatui_image::{
    Resize,
    picker::Picker,
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};
use serde::{Deserialize, Serialize};

pub(crate) struct BackgroundJobView {
    pub(crate) id: String,
    pub(crate) pid: u32,
    pub(crate) command: String,
    pub(crate) cwd: PathBuf,
    pub(crate) state: ToolRunState,
    pub(crate) started_at: Instant,
    pub(crate) finished_at: Option<Instant>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) last_output: String,
}

#[derive(Debug, Clone)]
pub(crate) struct SettingsItem {
    pub(crate) key: &'static str,
    pub(crate) value: String,
    pub(crate) description: &'static str,
    pub(crate) action: &'static str,
    pub(crate) editable: bool,
}

pub(crate) struct BackgroundWorkflow {
    pub(crate) events: Receiver<WorkflowEvent>,
    pub(crate) checkpoint: CheckpointRecorder,
    pub(crate) cancel: CancelToken,
}

pub(crate) struct PendingApproval {
    pub(crate) request: ApprovalRequest,
    pub(crate) respond: Sender<ApprovalDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChatViewportMetrics {
    pub(crate) text_area: Rect,
    pub(crate) has_scrollbar: bool,
    pub(crate) total_visual_lines: usize,
    pub(crate) max_scroll: usize,
    pub(crate) scroll: usize,
    pub(crate) top_offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Modal {
    Commands,
    Settings,
    Help,
    ImagePreview,
    Workflows,
    Jobs,
    Sessions,
    SessionTree,
    Models,
    Reasoning,
    Permissions,
    Themes,
    Rewind,
    EditMessage,
    Mcp,
    Agents,
    Cost,
    Context,
}

/// Which screen of the /rewind modal is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RewindStage {
    Pick,
    Confirm,
}

/// One row in the /edit backtrack picker: a previous user message.
#[derive(Debug, Clone)]
pub(crate) struct EditPickerEntry {
    pub(crate) transcript_index: usize,
    pub(crate) preview: String,
}

/// The /edit picker shows at most this many previous user messages.
pub(crate) const EDIT_PICKER_LIMIT: usize = 20;

#[derive(Debug, Clone)]
pub(crate) struct Toast {
    pub(crate) message: String,
    pub(crate) kind: ToastKind,
    pub(crate) created_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UiFocus {
    Composer,
    Activity,
    Modal,
    Transcript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelPickerPane {
    Models,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChatMessage {
    pub(crate) role: ChatRole,
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) attachments: Vec<ImageAttachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ImageAttachment {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) mime: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) size_bytes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptRow {
    pub(crate) line: Line<'static>,
    pub(crate) image: Option<ImageAttachment>,
}

impl TranscriptRow {
    pub(crate) fn text(line: Line<'static>) -> Self {
        Self { line, image: None }
    }

    pub(crate) fn image(line: Line<'static>, attachment: ImageAttachment) -> Self {
        Self {
            line,
            image: Some(attachment),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RenderContext {
    pub(crate) animation_tick: u64,
    /// Index of the keyboard-selected question in the pending decision.
    pub(crate) decision_selection: usize,
    pub(crate) show_reasoning: bool,
}

impl RenderContext {
    #[cfg(test)]
    pub(crate) fn static_view() -> Self {
        Self::default()
    }
}

pub(crate) struct TerminalImageRenderer {
    pub(crate) picker: Option<Picker>,
    pub(crate) protocols: HashMap<String, SlicedProtocol>,
}

impl std::fmt::Debug for TerminalImageRenderer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminalImageRenderer")
            .field("enabled", &self.is_enabled())
            .field("cached", &self.protocols.len())
            .finish()
    }
}

impl TerminalImageRenderer {
    pub(crate) fn detect() -> Self {
        if env::var("MEDUSA_DISABLE_IMAGES").is_ok_and(|value| value == "1" || value == "true") {
            return Self::disabled();
        }

        #[cfg(test)]
        {
            Self::disabled()
        }

        #[cfg(not(test))]
        {
            let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
            Self {
                picker: Some(picker),
                protocols: HashMap::new(),
            }
        }
    }

    pub(crate) fn disabled() -> Self {
        Self {
            picker: None,
            protocols: HashMap::new(),
        }
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.picker.is_some()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn render(
        &mut self,
        frame: &mut Frame<'_>,
        attachment: &ImageAttachment,
        area: Rect,
        width: u16,
        height: u16,
        x_offset: u16,
        y_offset: i16,
    ) -> bool {
        if area.width == 0 || area.height == 0 || width == 0 || height == 0 {
            return false;
        }

        let Some(protocol) = self.protocol_for(attachment, width, height) else {
            return false;
        };

        frame.render_widget(
            SlicedImage::new(
                protocol,
                SignedPosition::from((x_offset.min(i16::MAX as u16) as i16, y_offset)),
            ),
            area,
        );
        true
    }

    pub(crate) fn protocol_for(
        &mut self,
        attachment: &ImageAttachment,
        width: u16,
        height: u16,
    ) -> Option<&SlicedProtocol> {
        let picker = self.picker.as_ref()?;
        let key = format!("{}:{width}x{height}", attachment.id);
        if !self.protocols.contains_key(&key) {
            let bytes = fs::read(&attachment.path).ok()?;
            let image = image::load_from_memory(&bytes).ok()?;
            let protocol = SlicedProtocol::new_with_resize(
                picker,
                image,
                (width, height).into(),
                Resize::Fit(None),
            )
            .ok()?;
            self.protocols.insert(key.clone(), protocol);
        }
        self.protocols.get(&key)
    }

    pub(crate) fn forget(&mut self, attachment_id: &str) {
        let prefix = format!("{attachment_id}:");
        self.protocols.retain(|key, _| !key.starts_with(&prefix));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ChatRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TranscriptItem {
    Message(ChatMessage),
    Tool(ToolRun),
    Reasoning(ReasoningTrace),
    Plan(PlanView),
    Decision(DecisionView),
    Workflow(WorkflowRunView),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ToolRun {
    #[serde(default)]
    pub(crate) id: Option<String>,
    #[serde(skip, default = "Instant::now")]
    pub(crate) started_at: Instant,
    #[serde(skip, default)]
    pub(crate) pending_result: Option<ToolRunPendingResult>,
    pub(crate) name: String,
    pub(crate) summary: String,
    pub(crate) state: ToolRunState,
    pub(crate) detail: String,
    #[serde(default)]
    pub(crate) expanded: bool,
    /// Set on the first tool of a finished group to re-open a collapsed group.
    #[serde(default)]
    pub(crate) group_expanded: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct TranscriptRowsCache {
    pub(crate) version: u64,
    pub(crate) theme: ThemeKind,
    pub(crate) streaming_message: Option<usize>,
    pub(crate) selected_tool: Option<usize>,
    pub(crate) animation_tick: Option<u64>,
    pub(crate) decision_selection: usize,
    pub(crate) show_reasoning: bool,
    /// Shared so cache hits are an Arc bump, not a deep clone of every row.
    pub(crate) rows: Arc<Vec<TranscriptRow>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReasoningTrace {
    pub(crate) content: String,
    #[serde(default)]
    pub(crate) expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanView {
    #[serde(default)]
    pub(crate) summary: String,
    pub(crate) items: Vec<PlanItemView>,
    #[serde(default)]
    pub(crate) expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlanItemView {
    pub(crate) text: String,
    pub(crate) status: PlanItemStatus,
    #[serde(default)]
    pub(crate) evidence: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanItemStatus {
    Pending,
    Active,
    Done,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DecisionView {
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) reason: String,
    pub(crate) questions: Vec<DecisionQuestionView>,
    #[serde(default)]
    pub(crate) assumptions: Vec<String>,
    #[serde(default)]
    pub(crate) answered: bool,
    #[serde(default)]
    pub(crate) answer: Option<String>,
    #[serde(default)]
    pub(crate) answers: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DecisionQuestionView {
    pub(crate) id: String,
    pub(crate) prompt: String,
    pub(crate) kind: DecisionQuestionKind,
    #[serde(default)]
    pub(crate) options: Vec<String>,
    #[serde(default)]
    pub(crate) recommended: Option<String>,
    #[serde(default = "default_required_decision")]
    pub(crate) required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DecisionQuestionKind {
    Choice,
    Text,
}

pub(crate) fn default_required_decision() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkflowRunView {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) task: String,
    pub(crate) status: WorkflowViewState,
    pub(crate) phases: Vec<WorkflowPhaseView>,
    pub(crate) summary: String,
    #[serde(default)]
    pub(crate) expanded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkflowPhaseView {
    pub(crate) name: String,
    pub(crate) objective: String,
    pub(crate) status: WorkflowViewState,
    pub(crate) agents: Vec<WorkflowAgentView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkflowAgentView {
    pub(crate) name: String,
    pub(crate) role: String,
    #[serde(default)]
    pub(crate) tool_policy: SubagentToolPolicy,
    pub(crate) status: WorkflowViewState,
    pub(crate) output: String,
    #[serde(default)]
    pub(crate) tool_counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum WorkflowViewState {
    Pending,
    Running,
    Succeeded,
    PartiallySucceeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ToolRunState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ToolRunPendingResult {
    pub(crate) state: ToolRunState,
    pub(crate) detail: String,
    pub(crate) received_at: Instant,
}

pub(crate) fn queue_or_apply_tool_result(
    run: &mut ToolRun,
    state: ToolRunState,
    detail: String,
    expand_on_failure: bool,
) {
    if run.started_at.elapsed() < MIN_TOOL_PULSE_VISIBLE {
        run.pending_result = Some(ToolRunPendingResult {
            state,
            detail,
            received_at: Instant::now(),
        });
        return;
    }
    apply_tool_result_now(run, state, detail, expand_on_failure);
}

pub(crate) fn apply_tool_result_now(
    run: &mut ToolRun,
    state: ToolRunState,
    detail: String,
    expand_on_failure: bool,
) {
    run.state = state;
    run.detail = detail;
    run.pending_result = None;
    run.expanded = expand_on_failure && state == ToolRunState::Failed;
}

/// Sweep an interrupted workflow row: everything still running (the run, its
/// phases, their agents) resolves to Failed so no spinner survives the turn.
pub(crate) fn mark_workflow_view_cancelled(view: &mut WorkflowRunView) {
    if view.status != WorkflowViewState::Running {
        return;
    }
    view.status = WorkflowViewState::Failed;
    if view.summary.is_empty() {
        view.summary = "cancelled".to_string();
    }
    for phase in &mut view.phases {
        if phase.status == WorkflowViewState::Running {
            phase.status = WorkflowViewState::Failed;
        }
        for agent in &mut phase.agents {
            if agent.status == WorkflowViewState::Running {
                agent.status = WorkflowViewState::Failed;
            }
        }
    }
}

impl ChatMessage {
    pub(crate) fn user(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            attachments: Vec::new(),
        }
    }

    pub(crate) fn user_with_attachments(
        content: impl Into<String>,
        attachments: Vec<ImageAttachment>,
    ) -> Self {
        Self {
            role: ChatRole::User,
            content: content.into(),
            attachments,
        }
    }

    pub(crate) fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::Assistant,
            content: content.into(),
            attachments: Vec::new(),
        }
    }

    pub(crate) fn system(content: impl Into<String>) -> Self {
        Self {
            role: ChatRole::System,
            content: content.into(),
            attachments: Vec::new(),
        }
    }
}
