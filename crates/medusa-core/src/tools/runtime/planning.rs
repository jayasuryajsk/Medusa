use super::*;

impl ToolRuntime {
    pub fn task_update(&self, request: TaskUpdateRequest) -> Result<TaskUpdateResult> {
        let status = request.status.trim();
        if status.is_empty() {
            bail!("status cannot be empty");
        }

        Ok(TaskUpdateResult {
            status: status.chars().take(160).collect(),
        })
    }

    pub fn plan_update(&self, request: PlanUpdateRequest) -> Result<PlanUpdateResult> {
        if request.items.is_empty() {
            bail!("plan items cannot be empty");
        }

        let mut active_count = 0usize;
        let mut items = Vec::new();
        for item in request.items.into_iter().take(24) {
            let text = item.text.trim();
            if text.is_empty() {
                continue;
            }
            let status = normalize_plan_status(&item.status)
                .ok_or_else(|| color_eyre::eyre::eyre!("unknown plan status: {}", item.status))?;
            if status == "active" {
                active_count += 1;
            }
            let evidence = item
                .evidence
                .into_iter()
                .map(|value| value.trim().chars().take(180).collect::<String>())
                .filter(|value| !value.is_empty())
                .take(6)
                .collect::<Vec<_>>();
            items.push(PlanUpdateItem {
                text: text.chars().take(220).collect(),
                status: status.to_string(),
                evidence,
            });
        }

        if items.is_empty() {
            bail!("plan items cannot all be empty");
        }
        if active_count > 1 {
            bail!("at most one plan item can be active");
        }

        Ok(PlanUpdateResult {
            summary: request
                .summary
                .unwrap_or_default()
                .trim()
                .chars()
                .take(180)
                .collect(),
            items,
        })
    }

    pub fn question(&self, request: QuestionRequest) -> Result<QuestionResult> {
        let question = request.question.trim();
        if question.is_empty() {
            bail!("question cannot be empty");
        }

        Ok(QuestionResult {
            question: question.chars().take(600).collect(),
        })
    }

    pub fn decision_request(&self, request: DecisionRequest) -> Result<DecisionResult> {
        if request.questions.is_empty() {
            bail!("decision_request.questions cannot be empty");
        }

        let mut seen_ids = BTreeSet::new();
        let mut questions = Vec::new();
        for (index, question) in request.questions.into_iter().take(8).enumerate() {
            let prompt = question.prompt.trim();
            if prompt.is_empty() {
                continue;
            }

            let kind = normalize_decision_kind(&question.kind);
            let options = question
                .options
                .into_iter()
                .map(|option| option.trim().chars().take(120).collect::<String>())
                .filter(|option| !option.is_empty())
                .take(8)
                .collect::<Vec<_>>();
            if kind == "choice" && options.is_empty() {
                bail!(
                    "decision_request.questions[{index}].options is required for choice questions"
                );
            }

            let base_id = sanitize_decision_id(&question.id)
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("q{}", index + 1));
            let mut id = base_id.clone();
            let mut suffix = 2usize;
            while !seen_ids.insert(id.clone()) {
                id = format!("{base_id}_{suffix}");
                suffix += 1;
            }

            let recommended = question
                .recommended
                .map(|value| value.trim().chars().take(120).collect::<String>())
                .filter(|value| !value.is_empty());

            questions.push(DecisionQuestion {
                id,
                prompt: prompt.chars().take(280).collect(),
                kind: kind.to_string(),
                options,
                recommended,
                required: question.required,
            });
        }

        if questions.is_empty() {
            bail!("decision_request.questions cannot all be empty");
        }

        let assumptions = request
            .assumptions
            .into_iter()
            .map(|assumption| assumption.trim().chars().take(220).collect::<String>())
            .filter(|assumption| !assumption.is_empty())
            .take(6)
            .collect::<Vec<_>>();

        Ok(DecisionResult {
            title: request
                .title
                .unwrap_or_else(|| "Planning decision".to_string())
                .trim()
                .chars()
                .take(120)
                .collect(),
            reason: request
                .reason
                .unwrap_or_default()
                .trim()
                .chars()
                .take(280)
                .collect(),
            questions,
            assumptions,
        })
    }
}
