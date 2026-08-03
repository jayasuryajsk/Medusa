use super::*;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PlanProgress {
    pub(crate) pending: usize,
    pub(crate) active: usize,
    pub(crate) done: usize,
    pub(crate) blocked: usize,
}

pub(crate) fn plan_progress(plan: &PlanView) -> PlanProgress {
    let mut progress = PlanProgress::default();
    for item in &plan.items {
        match item.status {
            PlanItemStatus::Pending => progress.pending += 1,
            PlanItemStatus::Active => progress.active += 1,
            PlanItemStatus::Done => progress.done += 1,
            PlanItemStatus::Blocked => progress.blocked += 1,
        }
    }
    progress
}

/// Items shown in the plan strip before folding the tail behind "+N more".
pub(crate) const PLAN_STRIP_MAX_ITEMS: usize = 6;

pub(crate) fn plan_strip_lines(plan: &PlanView) -> Vec<Line<'static>> {
    let progress = plan_progress(plan);
    let mut lines = Vec::new();

    let mut header = vec![
        Span::styled("plan", tool_label_style().add_modifier(Modifier::BOLD)),
        Span::styled(" · ", muted()),
        Span::styled(
            format!("{}/{}", progress.done, plan.items.len()),
            success_style(),
        ),
    ];
    if progress.blocked > 0 {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled(format!("{} blocked", progress.blocked), error_style()),
        ]);
    }
    if !plan.summary.trim().is_empty() {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled(truncate(&plan.summary, 72), muted()),
        ]);
    }
    lines.push(Line::from(header));

    // Long plans fold the completed prefix into one line so the strip always
    // centers on what's happening now.
    let mut start = 0;
    if plan.items.len() > PLAN_STRIP_MAX_ITEMS {
        let leading_done = plan
            .items
            .iter()
            .take_while(|item| item.status == PlanItemStatus::Done)
            .count();
        if leading_done > 1 {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("✓", success_style()),
                Span::styled(format!(" {leading_done} done"), success_style()),
            ]));
            start = leading_done;
        }
    }

    let remaining = &plan.items[start..];
    let shown = remaining.len().min(PLAN_STRIP_MAX_ITEMS);
    for item in remaining.iter().take(shown) {
        lines.push(Line::from(vec![
            Span::raw("  "),
            plan_status_marker_span(item.status),
            Span::raw(" "),
            Span::styled(truncate(&item.text, 110), plan_status_style(item.status)),
        ]));
    }
    if remaining.len() > shown {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("… +{} more", remaining.len() - shown), muted()),
        ]));
    }

    lines
}

pub(crate) fn plan_status_marker_span(status: PlanItemStatus) -> Span<'static> {
    match status {
        PlanItemStatus::Pending => Span::styled("·", muted()),
        PlanItemStatus::Active => Span::styled("●", prompt_style()),
        PlanItemStatus::Done => Span::styled("✓", success_style()),
        PlanItemStatus::Blocked => Span::styled("×", error_style()),
    }
}

pub(crate) fn plan_status_style(status: PlanItemStatus) -> Style {
    match status {
        PlanItemStatus::Pending => muted(),
        PlanItemStatus::Active => prompt_style(),
        PlanItemStatus::Done => success_style(),
        PlanItemStatus::Blocked => error_style(),
    }
}

pub(crate) fn append_decision_rows(
    rows: &mut Vec<TranscriptRow>,
    decision: &DecisionView,
    selected_question: usize,
) {
    let state = if decision.answered {
        "answered"
    } else {
        "waiting"
    };
    let mut header = vec![
        Span::styled("decision", prompt_style().add_modifier(Modifier::BOLD)),
        Span::styled(" · ", muted()),
        Span::styled(
            format!("{} question(s)", decision.questions.len()),
            value_style(),
        ),
        Span::styled(" · ", muted()),
        Span::styled(
            state,
            if decision.answered {
                success_style()
            } else {
                prompt_style()
            },
        ),
    ];
    if !decision.title.trim().is_empty() {
        header.extend([
            Span::styled(" · ", muted()),
            Span::styled(truncate(&decision.title, 72), muted()),
        ]);
    }
    rows.push(TranscriptRow::text(Line::from(header)));

    let last = decision.questions.len().saturating_sub(1);
    for (index, question) in decision.questions.iter().enumerate() {
        let answered = decision_question_answered(decision, question);
        let selected = !decision.answered && index == selected_question;
        let (marker, marker_style) = if selected {
            ("› ", prompt_style())
        } else if answered {
            ("✓ ", success_style())
        } else {
            ("? ", prompt_style())
        };
        let branch = if crate::terminal::ascii_ui() {
            if index == last { "  `- " } else { "  +- " }
        } else if index == last {
            "  └─ "
        } else {
            "  ├─ "
        };
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled(branch, muted()),
            Span::styled(marker, marker_style),
            Span::styled(
                truncate(&question.prompt, 120),
                if selected {
                    value_style().add_modifier(Modifier::BOLD)
                } else {
                    value_style()
                },
            ),
        ])));
        let continuation = if index == last {
            "     "
        } else if crate::terminal::ascii_ui() {
            "  |  "
        } else {
            "  │  "
        };
        if question.kind == DecisionQuestionKind::Choice && !decision.answered {
            for (option_index, option) in question.options.iter().take(4).enumerate() {
                let recommended = question.recommended.as_deref() == Some(option.as_str());
                let picked = decision.answers.get(&question.id) == Some(option);
                let mut spans = vec![
                    Span::styled(continuation, muted()),
                    Span::styled(
                        format!("{} {}. ", if picked { "●" } else { "○" }, option_index + 1),
                        if picked { success_style() } else { muted() },
                    ),
                    Span::styled(
                        truncate(option, 100),
                        if picked {
                            success_style()
                        } else {
                            value_style()
                        },
                    ),
                ];
                if recommended {
                    spans.push(Span::styled(" · recommended", muted()));
                }
                rows.push(TranscriptRow::text(Line::from(spans)));
            }
        } else if let Some(answer) = decision.answers.get(&question.id) {
            rows.push(TranscriptRow::text(Line::from(vec![
                Span::styled(continuation, muted()),
                Span::styled(truncate(answer, 120), success_style()),
            ])));
        }
    }
    if let Some(answer) = &decision.answer {
        rows.push(TranscriptRow::text(Line::from(vec![
            Span::styled("  answer ", success_style()),
            Span::styled(truncate(answer, 140), value_style()),
        ])));
    }
}

pub(crate) fn decision_question_answered(
    decision: &DecisionView,
    question: &DecisionQuestionView,
) -> bool {
    decision
        .answers
        .get(&question.id)
        .is_some_and(|answer| !answer.trim().is_empty())
}

pub(crate) fn decision_ready(decision: &DecisionView) -> bool {
    decision
        .questions
        .iter()
        .filter(|question| question.required)
        .all(|question| decision_question_answered(decision, question))
}

pub(crate) fn match_choice_option(options: &[String], value: &str) -> Option<String> {
    let value = value.trim();
    options
        .iter()
        .find(|option| option.eq_ignore_ascii_case(value))
        .cloned()
        .or_else(|| {
            let lower = value.to_ascii_lowercase();
            options
                .iter()
                .find(|option| option.to_ascii_lowercase().starts_with(&lower))
                .cloned()
        })
}

pub(crate) fn decision_answer_text(decision: &DecisionView) -> String {
    let title = if decision.title.trim().is_empty() {
        "planning decision"
    } else {
        decision.title.trim()
    };
    let mut lines = vec![format!("Decision answer: {title}")];
    if !decision.reason.trim().is_empty() {
        lines.push(format!(
            "Reason: {}",
            compact_one_line(&decision.reason, 220)
        ));
    }
    lines.push("Answers:".to_string());
    for question in &decision.questions {
        let answer = decision
            .answers
            .get(&question.id)
            .map(|answer| compact_one_line(answer, 220))
            .unwrap_or_else(|| "(skipped)".to_string());
        lines.push(format!(
            "- {}: {}",
            compact_one_line(&question.id, 48),
            answer
        ));
    }
    if !decision.assumptions.is_empty() {
        lines.push("Assumptions shown:".to_string());
        for assumption in decision.assumptions.iter().take(6) {
            lines.push(format!("- {}", compact_one_line(assumption, 220)));
        }
    }
    lines.join("\n")
}
