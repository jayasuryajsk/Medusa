use super::*;

impl ToolRuntime {
    pub fn explore_batch(&self, request: ExploreBatchRequest) -> Result<ExploreBatchResult> {
        if request.probes.is_empty() {
            bail!("explore_batch.probes cannot be empty");
        }

        let goal = request.goal.trim().chars().take(240).collect::<String>();
        let probes = request.probes.into_iter().take(12).collect::<Vec<_>>();
        let total = probes.len();
        let batch_started = Instant::now();
        let (sender, receiver) = mpsc::channel();

        for (index, probe) in probes.into_iter().enumerate() {
            let sender = sender.clone();
            let tools = self.clone();
            thread::spawn(move || {
                let result = run_explore_probe(&tools, index, probe);
                let _ = sender.send((index, result));
            });
        }
        drop(sender);

        let mut results = vec![None; total];
        for (index, result) in receiver {
            if let Some(slot) = results.get_mut(index) {
                *slot = Some(result);
            }
        }

        let probes = results
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result.unwrap_or_else(|| ExploreProbeResult {
                    index,
                    kind: "unknown".to_string(),
                    label: format!("probe {}", index + 1),
                    failed: true,
                    output: "probe worker ended without returning a result".to_string(),
                    elapsed_ms: 0,
                })
            })
            .collect::<Vec<_>>();
        let failed = probes.iter().filter(|probe| probe.failed).count();

        Ok(ExploreBatchResult {
            goal,
            probes,
            failed,
            elapsed_ms: batch_started.elapsed().as_millis(),
        })
    }
}
