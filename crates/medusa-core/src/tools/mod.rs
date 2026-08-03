//! Tool runtime: filesystem, terminal, explore, patch, and approval surfaces.

mod approval;
mod runtime;
mod support;
mod types;

#[cfg(test)]
mod tests;

pub use approval::{ApprovalDecision, ApprovalHandler, ApprovalRequest, ApprovalTool};
pub use runtime::ToolRuntime;
pub(crate) use support::{command_paths_outside_workspace, validate_read_only_terminal_command};
pub use types::{
    BackgroundJobEvent, DecisionQuestion, DecisionQuestionRequest, DecisionRequest, DecisionResult,
    ExploreBatchRequest, ExploreBatchResult, ExploreProbe, ExploreProbeKind, ExploreProbeResult,
    FileEditRequest, FileEditResult, FileGlobRequest, FileGlobResult, FilePatchRequest,
    FilePatchResult, FileReadRequest, FileReadResult, FileSearchRequest, FileSearchResult, FsEntry,
    FsListRequest, FsListResult, NumberedLine, PlanUpdateItem, PlanUpdateRequest, PlanUpdateResult,
    QuestionRequest, QuestionResult, ReadFile, SearchMatch, SemanticMatch, SemanticSearchRequest,
    SemanticSearchResult, TaskUpdateRequest, TaskUpdateResult, TerminalExecRequest,
    TerminalExecResult,
};
