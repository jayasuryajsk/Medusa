#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnMode {
    Chat,
    Goal,
    PlanFirst,
    Workflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrchestrationRoute {
    Direct,
    ExploreFirst,
    Workflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessPolicy {
    mode: TurnMode,
    route: OrchestrationRoute,
}

impl HarnessPolicy {
    pub fn for_user_prompt(prompt: &str) -> Self {
        let mode = classify_turn_mode(prompt);
        Self {
            mode,
            route: classify_orchestration_route(prompt, mode),
        }
    }

    pub fn for_user_prompt_with_reasoning(
        prompt: &str,
        reasoning_mode: &str,
        workflows_allowed: bool,
    ) -> Self {
        let mut policy = Self::for_user_prompt(prompt);
        if !workflows_allowed && policy.route == OrchestrationRoute::Workflow {
            policy.route = OrchestrationRoute::ExploreFirst;
        }
        if workflows_allowed
            && reasoning_mode.eq_ignore_ascii_case("ultra")
            && policy.mode == TurnMode::Goal
            && policy.route == OrchestrationRoute::ExploreFirst
        {
            policy.route = OrchestrationRoute::Workflow;
        }
        policy
    }

    pub fn mode_label(self) -> &'static str {
        match self.mode {
            TurnMode::Chat => "chat",
            TurnMode::Goal => "goal",
            TurnMode::PlanFirst => "plan",
            TurnMode::Workflow => "workflow",
        }
    }

    pub fn instructions(self) -> &'static str {
        match self.mode {
            TurnMode::Chat => {
                "Current turn mode hint: chat. Answer directly and naturally. Use tools when the answer depends on the live workspace state. The full toolset stays available; if the request turns out to need workspace changes, make them."
            }
            TurnMode::Goal => {
                "Current turn mode hint: goal. Treat the user request as implementation work. Use the default Medusa loop: observe the live state, make targeted changes with file_edit or file_patch, verify with focused commands, then repeat until the request is done or you are genuinely blocked. Verification is part of the task."
            }
            TurnMode::PlanFirst => {
                "Current turn mode hint: plan. Explore and reason before changing files. Prefer concise architecture, tradeoff, or workflow guidance. The full toolset stays available; implement once the plan is settled or the user asks."
            }
            TurnMode::Workflow => {
                "Current turn mode: workflow. The user explicitly requested a dynamic workflow. Author a task-specific JavaScript orchestration and invoke workflow_run. The script, not a fixed phase template, owns loops, branching, fan-out, verification, and stopping conditions. Use only as many subagents as the task warrants, keep intermediate results in script variables, and return the final result to this turn."
            }
        }
    }

    pub fn route(self) -> OrchestrationRoute {
        self.route
    }

    pub fn route_label(self) -> &'static str {
        match self.route {
            OrchestrationRoute::Direct => "direct",
            OrchestrationRoute::ExploreFirst => "explore-first",
            OrchestrationRoute::Workflow => "workflow",
        }
    }

    pub fn requires_initial_exploration(self) -> bool {
        self.route == OrchestrationRoute::ExploreFirst
    }

    pub fn requires_workflow(self) -> bool {
        self.route == OrchestrationRoute::Workflow
    }

    pub fn route_instructions(self) -> &'static str {
        match self.route {
            OrchestrationRoute::Direct => {
                "Orchestration route: direct. Use the ordinary model/tool loop and avoid delegation overhead unless new evidence makes the task materially broader."
            }
            OrchestrationRoute::ExploreFirst => {
                "Orchestration route: explore-first. Mutation tools stay withheld until you gather live workspace evidence. Batch independent reads with explore_batch or emit independent native read calls together, synthesize what matters, then edit through one coherent writer lane."
            }
            OrchestrationRoute::Workflow => {
                "Orchestration route: workflow. Invoke workflow_run before claiming completion. Use parallel read-only specialists only where their work is independent, return structured evidence when practical, perform mutations sequentially, and finish with an independent verification phase."
            }
        }
    }

    pub fn completion_contract(self) -> &'static str {
        match self.mode {
            TurnMode::Chat | TurnMode::PlanFirst => {
                "Completion contract: answer the actual request, ground workspace claims in observed evidence, and state uncertainty instead of inventing live state."
            }
            TurnMode::Goal | TurnMode::Workflow => {
                "Completion contract: satisfy the user's requested behavior, ground changes in observed evidence, keep workspace mutations serialized, run relevant validation after editing, repair validation failures when possible, and only claim completion when the evidence supports it. If genuinely blocked, name the blocker and unfinished criterion explicitly."
            }
        }
    }
}

pub fn core_harness_contract() -> &'static str {
    "Medusa is loop-native by default. There is no separate /loop mode for coding work: implementation turns naturally cycle through observe, plan, act, check, and repeat. The harness chooses the lightest effective route: direct work for small tasks, evidence-first exploration for uncertain work, and dynamic workflows for genuinely independent lanes. There is no arbitrary tool-call cap; a progress guard only stops equivalent outcomes that repeat without durable progress. Keep terminal output compact, use file_edit/file_patch as the native mutation boundary, and stop only when the work is complete, clearly blocked, or would exceed the user's authorization."
}

fn classify_turn_mode(prompt: &str) -> TurnMode {
    let text = prompt.trim().to_ascii_lowercase();
    if text.is_empty() || is_small_talk(&text) {
        return TurnMode::Chat;
    }

    if text.starts_with("/workflow ") {
        return TurnMode::Workflow;
    }

    if contains_any(&text, GOAL_MARKERS) {
        return TurnMode::Goal;
    }

    if starts_with_any(&text, QUESTION_PREFIXES) {
        return TurnMode::Chat;
    }

    if contains_any(&text, PLAN_MARKERS) {
        return TurnMode::PlanFirst;
    }

    TurnMode::Chat
}

fn classify_orchestration_route(prompt: &str, mode: TurnMode) -> OrchestrationRoute {
    let text = prompt.trim().to_ascii_lowercase();
    if text.starts_with("/workflow ") {
        return OrchestrationRoute::Workflow;
    }

    if mode == TurnMode::Goal && contains_any(&text, WORKFLOW_MARKERS) {
        return OrchestrationRoute::Workflow;
    }

    if mode == TurnMode::Goal && !looks_like_small_direct_goal(&text) {
        return OrchestrationRoute::ExploreFirst;
    }

    if contains_any(&text, EXPLORATION_MARKERS) && contains_any(&text, WORKSPACE_SCOPE_MARKERS) {
        return OrchestrationRoute::ExploreFirst;
    }

    OrchestrationRoute::Direct
}

fn looks_like_small_direct_goal(text: &str) -> bool {
    text.len() <= 120 && contains_any(text, SMALL_DIRECT_MARKERS)
}

fn is_small_talk(text: &str) -> bool {
    matches!(
        text,
        "hi" | "hello" | "hey" | "yo" | "sup" | "cool" | "thanks" | "thank you"
    )
}

fn contains_any(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| text.contains(marker))
}

fn starts_with_any(text: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| text.starts_with(prefix))
}

const GOAL_MARKERS: &[&str] = &[
    "add ",
    "audit ",
    "build ",
    "change ",
    "clean ",
    "create ",
    "debug ",
    "delete ",
    "do it",
    "fix ",
    "implement ",
    "improve ",
    "make ",
    "migrate ",
    "patch ",
    "refactor ",
    "remove ",
    "review ",
    "run ",
    "ship ",
    "test ",
    "update ",
    "wire ",
];

const PLAN_MARKERS: &[&str] = &[
    "architecture",
    "design",
    "harness",
    "roadmap",
    "stack",
    "study",
    "workflow",
];

const QUESTION_PREFIXES: &[&str] = &[
    "can ", "could ", "does ", "explain", "how ", "tell me", "what ", "whats ", "why ", "would ",
];

const WORKFLOW_MARKERS: &[&str] = &[
    "across the codebase",
    "audit every",
    "entire codebase",
    "independent review",
    "many files",
    "migrate all",
    "multi-agent",
    "multiple agents",
    "parallel agents",
    "subagents",
    "sweeping",
    "whole repo",
];

const EXPLORATION_MARKERS: &[&str] = &[
    "analyze",
    "audit",
    "explain",
    "inspect",
    "read",
    "review",
    "study",
    "understand",
];

const WORKSPACE_SCOPE_MARKERS: &[&str] =
    &["codebase", "project", "repo", "repository", "workspace"];

const SMALL_DIRECT_MARKERS: &[&str] = &[
    "one line",
    "single line",
    "this typo",
    "typo",
    "update version",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_talk_stays_chat() {
        assert_eq!(HarnessPolicy::for_user_prompt("hi").mode, TurnMode::Chat);
    }

    #[test]
    fn direct_questions_stay_chat() {
        assert_eq!(
            HarnessPolicy::for_user_prompt("what is ratatui?").mode,
            TurnMode::Chat
        );
        assert_eq!(
            HarnessPolicy::for_user_prompt("explain the harness").mode,
            TurnMode::Chat
        );
    }

    #[test]
    fn implementation_requests_enter_goal_loop() {
        assert_eq!(
            HarnessPolicy::for_user_prompt("fix the failing tests").mode,
            TurnMode::Goal
        );
        assert_eq!(
            HarnessPolicy::for_user_prompt("implement this").mode,
            TurnMode::Goal
        );
    }

    #[test]
    fn nontrivial_goals_explore_before_mutating() {
        let policy = HarnessPolicy::for_user_prompt("fix the failing tests");
        assert_eq!(policy.route(), OrchestrationRoute::ExploreFirst);
        assert!(policy.requires_initial_exploration());

        let small = HarnessPolicy::for_user_prompt("fix this typo");
        assert_eq!(small.route(), OrchestrationRoute::Direct);
    }

    #[test]
    fn broad_parallel_work_routes_to_workflow() {
        let policy =
            HarnessPolicy::for_user_prompt("review the entire codebase with multiple agents");
        assert_eq!(policy.mode, TurnMode::Goal);
        assert_eq!(policy.route(), OrchestrationRoute::Workflow);
        assert!(policy.requires_workflow());
    }

    #[test]
    fn ultra_upgrades_nontrivial_goals_to_workflows() {
        let policy =
            HarnessPolicy::for_user_prompt_with_reasoning("fix the failing tests", "ultra", true);
        assert_eq!(policy.route(), OrchestrationRoute::Workflow);

        let chat = HarnessPolicy::for_user_prompt_with_reasoning("hi", "ultra", true);
        assert_eq!(chat.route(), OrchestrationRoute::Direct);

        let subagent = HarnessPolicy::for_user_prompt_with_reasoning(
            "review the entire codebase with multiple agents",
            "ultra",
            false,
        );
        assert_eq!(subagent.route(), OrchestrationRoute::ExploreFirst);
    }

    #[test]
    fn mode_is_advisory_and_never_forbids_edits() {
        for prompt in ["hi", "design the architecture", "implement this"] {
            let instructions = HarnessPolicy::for_user_prompt(prompt).instructions();
            assert!(instructions.contains("mode hint"));
            assert!(!instructions.contains("Do not edit"));
        }
    }

    #[test]
    fn architecture_discussion_is_plan_first() {
        assert_eq!(
            HarnessPolicy::for_user_prompt("let's talk about backend architecture").mode,
            TurnMode::PlanFirst
        );
    }

    #[test]
    fn explicit_workflow_command_uses_dynamic_workflow_mode() {
        let policy = HarnessPolicy::for_user_prompt("/workflow audit every route");

        assert_eq!(policy.mode, TurnMode::Workflow);
        assert_eq!(policy.mode_label(), "workflow");
        assert_eq!(policy.route_label(), "workflow");
        assert!(policy.instructions().contains("invoke workflow_run"));
        assert!(policy.instructions().contains("task-specific JavaScript"));
    }
}
