use std::time::Duration;

pub(crate) const COMPOSER_IMAGE_PREVIEW_WIDTH: u16 = 18;
pub(crate) const COMPOSER_IMAGE_PREVIEW_HEIGHT: u16 = 5;
pub(crate) const CHAT_IMAGE_PREVIEW_WIDTH: u16 = 52;
pub(crate) const CHAT_IMAGE_PREVIEW_HEIGHT: u16 = 16;
pub(crate) const IMAGE_PREVIEW_MIN_ZOOM: u16 = 25;
pub(crate) const IMAGE_PREVIEW_MAX_ZOOM: u16 = 300;
pub(crate) const IMAGE_PREVIEW_ZOOM_STEP: u16 = 25;
pub(crate) const CHAT_BOTTOM_PADDING_ROWS: usize = 1;
pub(crate) const MIN_TOOL_PULSE_VISIBLE: Duration = Duration::from_millis(650);
pub(crate) const DEFAULT_MODEL_CHOICES: &[&str] = &[
    "gpt-5.5",
    "gpt-5.3-codex",
    "gpt-5.3",
    "gpt-5.1-codex",
    "deepseek-v4-flash",
];
pub(crate) const SESSION_STATE_MAX_INTENTS: usize = 8;
pub(crate) const SESSION_STATE_MAX_OUTCOMES: usize = 8;
pub(crate) const SESSION_STATE_MAX_SYSTEM_NOTES: usize = 6;
pub(crate) const SESSION_STATE_MAX_TOOLS: usize = 12;
pub(crate) const SESSION_STATE_MAX_FILES: usize = 16;
pub(crate) const SESSION_MEMORY_MAX_PER_KIND: usize = 5;

pub(crate) const DOUBLE_ESCAPE_WINDOW: Duration = Duration::from_millis(1_500);
/// The @ mention file walk stops after this many files so giant workspaces
/// cannot stall the composer.
pub(crate) const MENTION_FILE_WALK_CAP: usize = 5_000;
/// At most this many fuzzy matches are kept for the mention popup.
pub(crate) const MENTION_MATCH_LIMIT: usize = 50;
/// Directories the @ mention file walk skips: VCS internals, caches, and
/// build output that would drown real sources.
pub(crate) const MENTION_SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".medusa",
    "target",
    "node_modules",
    "dist",
    "build",
    "out",
    ".next",
    ".cache",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
];
/// Turns shorter than this never ring the terminal bell.
pub(crate) const BELL_MIN_WORKING_DURATION: Duration = Duration::from_secs(10);
/// Heading written when quick-memory creates AGENTS.md from scratch.
pub(crate) const QUICK_MEMORY_HEADER: &str = "# Project notes";
/// Section of AGENTS.md that `# <note>` composer input appends to.
pub(crate) const QUICK_MEMORY_SECTION: &str = "## Notes";
/// Transcript note (and model-history system message) left when the user
/// interrupts a turn with Esc.
pub(crate) const TURN_INTERRUPTED_NOTE: &str = "turn interrupted by user";
/// Decision keys are ignored for this long after an approval prompt first
/// appears, so a keystroke already in flight can't blindly approve or deny.
pub(crate) const APPROVAL_KEY_GRACE: Duration = Duration::from_millis(350);

pub(crate) const PLAN_MODE_DIRECTIVE: &str = "Plan mode is active. Explore the workspace read-only to understand the task, \
then present a concise implementation plan: publish it with plan_update, raise decisions that materially change \
the approach with decision_request, and finish by asking the user to approve. Do not edit files, apply patches, \
or run mutating commands while plan mode is on; the user turns plan mode off to approve implementation.";
