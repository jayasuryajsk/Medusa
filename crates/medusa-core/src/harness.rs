#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnMode {
    Chat,
    Goal,
    PlanFirst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarnessPolicy {
    mode: TurnMode,
}

impl HarnessPolicy {
    pub fn for_user_prompt(prompt: &str) -> Self {
        Self {
            mode: classify_turn_mode(prompt),
        }
    }

    pub fn mode_label(self) -> &'static str {
        match self.mode {
            TurnMode::Chat => "chat",
            TurnMode::Goal => "goal",
            TurnMode::PlanFirst => "plan",
        }
    }

    pub fn allows_patch(self) -> bool {
        self.mode == TurnMode::Goal
    }

    pub fn instructions(self) -> &'static str {
        match self.mode {
            TurnMode::Chat => {
                "Current turn mode: chat. Answer directly and naturally. Use tools only when the answer depends on the live workspace state. Do not edit files or run broad inspection commands unless the user asked for workspace work."
            }
            TurnMode::Goal => {
                "Current turn mode: goal. Treat the user request as implementation work. Use the default Medusa loop: observe the live state, make targeted changes with file_patch, verify with focused commands, then repeat until the request is done or you are genuinely blocked. Verification is part of the task."
            }
            TurnMode::PlanFirst => {
                "Current turn mode: plan. Explore and reason before changing files. Prefer concise architecture, tradeoff, or workflow guidance. If the user explicitly asks to implement after planning, switch into the goal loop."
            }
        }
    }
}

pub fn core_harness_contract() -> &'static str {
    "Medusa is loop-native by default. There is no separate /loop mode for coding work: implementation turns should naturally cycle through observe, plan, act, check, and repeat. Keep the tool surface small, keep terminal output compact, use file_patch as the mutation boundary, and stop only when the work is complete, clearly blocked, or would exceed the user's authorization."
}

fn classify_turn_mode(prompt: &str) -> TurnMode {
    let text = prompt.trim().to_ascii_lowercase();
    if text.is_empty() || is_small_talk(&text) {
        return TurnMode::Chat;
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
    "patch ",
    "refactor ",
    "remove ",
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
    fn only_goal_turns_allow_patches() {
        assert!(!HarnessPolicy::for_user_prompt("hi").allows_patch());
        assert!(!HarnessPolicy::for_user_prompt("design the architecture").allows_patch());
        assert!(HarnessPolicy::for_user_prompt("implement this").allows_patch());
    }

    #[test]
    fn architecture_discussion_is_plan_first() {
        assert_eq!(
            HarnessPolicy::for_user_prompt("let's talk about backend architecture").mode,
            TurnMode::PlanFirst
        );
    }
}
