use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::types::SkillRequirements;

const DEFAULT_ROUNDS: u32 = 5;
const MAX_STORED_RUNS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalInput {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub compatibility: Option<String>,
    #[serde(default)]
    pub requires: SkillRequirements,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalRun {
    pub id: String,
    pub skill_name: String,
    pub source: String,
    pub created_at_ms: u64,
    pub status: String,
    pub benchmark: SkillEvalBenchmark,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalBenchmark {
    pub metadata: SkillEvalMetadata,
    pub configurations: Vec<SkillEvalConfigurationSummary>,
    pub assertions: Vec<SkillEvalAssertionComparison>,
    pub run_summary: SkillEvalRunSummary,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalMetadata {
    pub timestamp_ms: u64,
    pub rounds: u32,
    pub skill_name: String,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalConfigurationSummary {
    pub configuration: String,
    pub passed: u64,
    pub total: u64,
    pub pass_rate: f64,
    pub avg_duration_ms: f64,
    pub stddev_duration_ms: f64,
    pub avg_tokens: f64,
    pub stddev_tokens: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalAssertionComparison {
    pub id: String,
    pub label: String,
    pub with_skill: SkillEvalAssertionResult,
    pub without_skill: SkillEvalAssertionResult,
    pub delta_passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalAssertionResult {
    pub passed: bool,
    #[serde(default)]
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalRunSummary {
    pub with_skill_pass_rate: f64,
    pub without_skill_pass_rate: f64,
    pub pass_rate_delta: f64,
    pub with_skill_avg_duration_ms: f64,
    pub without_skill_avg_duration_ms: f64,
    pub with_skill_avg_tokens: f64,
    pub without_skill_avg_tokens: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillEvalLog {
    pub version: u32,
    #[serde(default)]
    pub runs: Vec<SkillEvalRun>,
}

impl Default for SkillEvalLog {
    fn default() -> Self {
        Self {
            version: 1,
            runs: Vec::new(),
        }
    }
}

pub struct SkillEvalStore {
    path: PathBuf,
}

impl SkillEvalStore {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn default_path() -> anyhow::Result<PathBuf> {
        Ok(moltis_config::data_dir().join("skills-evals.json"))
    }

    pub fn load(&self) -> anyhow::Result<SkillEvalLog> {
        if !self.path.exists() {
            return Ok(SkillEvalLog::default());
        }
        let data = std::fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read skill eval log '{}'", self.path.display()))?;
        let log = serde_json::from_str::<SkillEvalLog>(&data)
            .with_context(|| format!("failed to parse skill eval log '{}'", self.path.display()))?;
        Ok(log)
    }

    pub fn save(&self, log: &SkillEvalLog) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent dir '{}'", parent.display()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(log).context("failed to serialize eval log")?;
        std::fs::write(&tmp, data)
            .with_context(|| format!("failed to write temp eval log '{}'", tmp.display()))?;
        std::fs::rename(&tmp, &self.path).with_context(|| {
            format!(
                "failed to replace eval log '{}' with '{}'",
                self.path.display(),
                tmp.display()
            )
        })?;
        Ok(())
    }

    pub fn append(&self, run: SkillEvalRun) -> anyhow::Result<()> {
        let mut log = self.load()?;
        log.runs.retain(|existing| existing.id != run.id);
        log.runs.insert(0, run);
        if log.runs.len() > MAX_STORED_RUNS {
            log.runs.truncate(MAX_STORED_RUNS);
        }
        self.save(&log)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

pub fn run_skill_eval(input: &SkillEvalInput, rounds: Option<u32>) -> SkillEvalRun {
    let rounds = rounds.unwrap_or(DEFAULT_ROUNDS).max(1);
    let assertions = evaluate_assertions(input);

    let with_passed = assertions.iter().filter(|a| a.with_skill.passed).count() as u64;
    let without_passed = assertions.iter().filter(|a| a.without_skill.passed).count() as u64;
    let total = assertions.len() as u64;
    let with_pass_rate = ratio(with_passed, total);
    let without_pass_rate = ratio(without_passed, total);

    let with_metrics = simulate_metrics(input, "with_skill", rounds, true);
    let without_metrics = simulate_metrics(input, "without_skill", rounds, false);

    let run_summary = SkillEvalRunSummary {
        with_skill_pass_rate: with_pass_rate,
        without_skill_pass_rate: without_pass_rate,
        pass_rate_delta: with_pass_rate - without_pass_rate,
        with_skill_avg_duration_ms: with_metrics.avg_duration_ms,
        without_skill_avg_duration_ms: without_metrics.avg_duration_ms,
        with_skill_avg_tokens: with_metrics.avg_tokens,
        without_skill_avg_tokens: without_metrics.avg_tokens,
    };

    let notes = build_notes(&assertions, &run_summary);
    let timestamp_ms = now_ms();
    let benchmark = SkillEvalBenchmark {
        metadata: SkillEvalMetadata {
            timestamp_ms,
            rounds,
            skill_name: input.name.clone(),
            source: input.source.clone(),
        },
        configurations: vec![
            SkillEvalConfigurationSummary {
                configuration: "with_skill".to_string(),
                passed: with_passed,
                total,
                pass_rate: with_pass_rate,
                avg_duration_ms: with_metrics.avg_duration_ms,
                stddev_duration_ms: with_metrics.stddev_duration_ms,
                avg_tokens: with_metrics.avg_tokens,
                stddev_tokens: with_metrics.stddev_tokens,
            },
            SkillEvalConfigurationSummary {
                configuration: "without_skill".to_string(),
                passed: without_passed,
                total,
                pass_rate: without_pass_rate,
                avg_duration_ms: without_metrics.avg_duration_ms,
                stddev_duration_ms: without_metrics.stddev_duration_ms,
                avg_tokens: without_metrics.avg_tokens,
                stddev_tokens: without_metrics.stddev_tokens,
            },
        ],
        assertions,
        run_summary,
        notes,
    };

    SkillEvalRun {
        id: eval_id(&input.name, timestamp_ms),
        skill_name: input.name.clone(),
        source: input.source.clone(),
        created_at_ms: timestamp_ms,
        status: "completed".to_string(),
        benchmark,
    }
}

#[derive(Debug, Clone)]
struct EvalAssertionDef {
    id: &'static str,
    label: &'static str,
}

fn eval_assertion_defs() -> [EvalAssertionDef; 7] {
    [
        EvalAssertionDef {
            id: "trigger_quality",
            label: "Trigger intent is explicit",
        },
        EvalAssertionDef {
            id: "workflow_steps",
            label: "Workflow is stepwise and executable",
        },
        EvalAssertionDef {
            id: "tool_specificity",
            label: "Tool usage is explicit",
        },
        EvalAssertionDef {
            id: "safety_guardrails",
            label: "Risk guardrails are present",
        },
        EvalAssertionDef {
            id: "validation_loop",
            label: "Validation/check loop is defined",
        },
        EvalAssertionDef {
            id: "examples_present",
            label: "Examples are included",
        },
        EvalAssertionDef {
            id: "dependency_clarity",
            label: "Dependencies are declared",
        },
    ]
}

fn evaluate_assertions(input: &SkillEvalInput) -> Vec<SkillEvalAssertionComparison> {
    let mut out = Vec::new();
    for def in eval_assertion_defs() {
        let with_skill = evaluate_assertion(def.id, input, true);
        let without_skill = evaluate_assertion(def.id, input, false);
        out.push(SkillEvalAssertionComparison {
            id: def.id.to_string(),
            label: def.label.to_string(),
            delta_passed: with_skill.passed && !without_skill.passed,
            with_skill,
            without_skill,
        });
    }
    out
}

fn evaluate_assertion(
    id: &str,
    input: &SkillEvalInput,
    with_skill: bool,
) -> SkillEvalAssertionResult {
    let baseline_requires = SkillRequirements::default();
    let body = if with_skill {
        &input.body
    } else {
        ""
    };
    let allowed_tools: &[String] = if with_skill {
        &input.allowed_tools
    } else {
        &[]
    };
    let compatibility = if with_skill {
        input.compatibility.as_deref()
    } else {
        None
    };
    let requires = if with_skill {
        &input.requires
    } else {
        &baseline_requires
    };

    match id {
        "trigger_quality" => trigger_quality_result(&input.description),
        "workflow_steps" => workflow_steps_result(body),
        "tool_specificity" => tool_specificity_result(body, allowed_tools),
        "safety_guardrails" => safety_guardrails_result(body, compatibility),
        "validation_loop" => validation_loop_result(body),
        "examples_present" => examples_present_result(body),
        "dependency_clarity" => dependency_clarity_result(requires),
        _ => SkillEvalAssertionResult {
            passed: false,
            evidence: vec!["unknown assertion".to_string()],
        },
    }
}

fn trigger_quality_result(description: &str) -> SkillEvalAssertionResult {
    let lc = description.to_lowercase();
    let words = word_count(description);
    let has_verb = [
        "create",
        "build",
        "update",
        "optimize",
        "run",
        "benchmark",
        "evaluate",
        "improve",
        "generate",
        "review",
    ]
    .iter()
    .any(|needle| lc.contains(needle));
    let passed = words >= 8 && has_verb;
    let mut evidence = Vec::new();
    evidence.push(format!("description_words={words}"));
    evidence.push(if has_verb {
        "contains_action_verb".to_string()
    } else {
        "missing_action_verb".to_string()
    });
    SkillEvalAssertionResult { passed, evidence }
}

fn workflow_steps_result(body: &str) -> SkillEvalAssertionResult {
    let lc = body.to_lowercase();
    let has_numbered = lc.contains("1.") && (lc.contains("2.") || lc.contains("step 2"));
    let has_step_language = lc.contains("step ") || lc.contains("workflow");
    let passed = has_numbered || has_step_language;
    let mut evidence = Vec::new();
    if has_numbered {
        evidence.push("found_numbered_sequence".to_string());
    }
    if has_step_language {
        evidence.push("found_step_language".to_string());
    }
    if evidence.is_empty() {
        evidence.push("no_explicit_execution_flow".to_string());
    }
    SkillEvalAssertionResult { passed, evidence }
}

fn tool_specificity_result(body: &str, allowed_tools: &[String]) -> SkillEvalAssertionResult {
    let body_lc = body.to_lowercase();
    let has_inline_command = body.contains('`') && body_lc.contains('(') && body_lc.contains(')');
    let has_allowed_tools = !allowed_tools.is_empty();
    let passed = has_allowed_tools || has_inline_command;
    let mut evidence = Vec::new();
    if has_allowed_tools {
        evidence.push(format!("allowed_tools={}", allowed_tools.len()));
    }
    if has_inline_command {
        evidence.push("contains_inline_tool_invocations".to_string());
    }
    if evidence.is_empty() {
        evidence.push("no_tool_constraints_or_invocations".to_string());
    }
    SkillEvalAssertionResult { passed, evidence }
}

fn safety_guardrails_result(body: &str, compatibility: Option<&str>) -> SkillEvalAssertionResult {
    let joined = format!(
        "{} {}",
        body.to_lowercase(),
        compatibility.unwrap_or("").to_lowercase()
    );
    let phrases = [
        "confirm",
        "double-check",
        "verify",
        "before running",
        "do not",
        "never",
        "must not",
        "danger",
        "sandbox",
    ];
    let hits: Vec<&str> = phrases
        .iter()
        .copied()
        .filter(|phrase| joined.contains(phrase))
        .collect();
    let passed = !hits.is_empty();
    let evidence = if passed {
        hits.into_iter().map(str::to_string).collect()
    } else {
        vec!["no_guardrail_language_detected".to_string()]
    };
    SkillEvalAssertionResult { passed, evidence }
}

fn validation_loop_result(body: &str) -> SkillEvalAssertionResult {
    let lc = body.to_lowercase();
    let phrases = [
        "test",
        "validate",
        "assert",
        "check",
        "verify output",
        "benchmark",
    ];
    let hits: Vec<&str> = phrases
        .iter()
        .copied()
        .filter(|phrase| lc.contains(phrase))
        .collect();
    let passed = !hits.is_empty();
    let evidence = if passed {
        hits.into_iter().map(str::to_string).collect()
    } else {
        vec!["no_validation_loop_detected".to_string()]
    };
    SkillEvalAssertionResult { passed, evidence }
}

fn examples_present_result(body: &str) -> SkillEvalAssertionResult {
    let lc = body.to_lowercase();
    let has_example_word = lc.contains("example");
    let has_fenced_code = body.contains("```");
    let passed = has_example_word || has_fenced_code;
    let mut evidence = Vec::new();
    if has_example_word {
        evidence.push("contains_example_language".to_string());
    }
    if has_fenced_code {
        evidence.push("contains_fenced_code".to_string());
    }
    if evidence.is_empty() {
        evidence.push("no_examples_detected".to_string());
    }
    SkillEvalAssertionResult { passed, evidence }
}

fn dependency_clarity_result(requires: &SkillRequirements) -> SkillEvalAssertionResult {
    let passed =
        !requires.bins.is_empty() || !requires.any_bins.is_empty() || !requires.install.is_empty();
    let mut evidence = Vec::new();
    if !requires.bins.is_empty() {
        evidence.push(format!("bins={}", requires.bins.len()));
    }
    if !requires.any_bins.is_empty() {
        evidence.push(format!("any_bins={}", requires.any_bins.len()));
    }
    if !requires.install.is_empty() {
        evidence.push(format!("install_options={}", requires.install.len()));
    }
    if evidence.is_empty() {
        evidence.push("no_dependencies_declared".to_string());
    }
    SkillEvalAssertionResult { passed, evidence }
}

#[derive(Debug, Clone, Copy)]
struct ConfigMetrics {
    avg_duration_ms: f64,
    stddev_duration_ms: f64,
    avg_tokens: f64,
    stddev_tokens: f64,
}

fn simulate_metrics(
    input: &SkillEvalInput,
    config_name: &str,
    rounds: u32,
    with_skill: bool,
) -> ConfigMetrics {
    let description_words = word_count(&input.description) as f64;
    let body_words = word_count(&input.body) as f64;
    let tool_factor = input.allowed_tools.len() as f64;
    let dependency_factor = (input.requires.bins.len()
        + input.requires.any_bins.len()
        + input.requires.install.len()) as f64;

    let base_duration = if with_skill {
        420.0 + (body_words * 2.4) + (tool_factor * 18.0) + (dependency_factor * 10.0)
    } else {
        300.0 + (description_words * 1.1)
    };
    let base_tokens = if with_skill {
        180.0 + (body_words * 1.8) + (tool_factor * 6.0)
    } else {
        130.0 + (description_words * 1.2)
    };

    let mut duration_samples = Vec::new();
    let mut token_samples = Vec::new();
    for round in 0..rounds {
        let round_str = round.to_string();
        let jitter_seed = stable_hash(&[input.name.as_str(), config_name, round_str.as_str()]);
        let duration_jitter = (jitter_seed % 101) as f64 - 50.0;
        let token_jitter = ((jitter_seed / 7) % 31) as f64 - 15.0;
        duration_samples.push((base_duration + duration_jitter).max(40.0));
        token_samples.push((base_tokens + token_jitter).max(20.0));
    }

    let (avg_duration_ms, stddev_duration_ms) = mean_stddev(&duration_samples);
    let (avg_tokens, stddev_tokens) = mean_stddev(&token_samples);
    ConfigMetrics {
        avg_duration_ms,
        stddev_duration_ms,
        avg_tokens,
        stddev_tokens,
    }
}

fn build_notes(
    assertions: &[SkillEvalAssertionComparison],
    summary: &SkillEvalRunSummary,
) -> Vec<String> {
    let mut notes = Vec::new();

    if summary.pass_rate_delta > 0.0 {
        notes.push(format!(
            "With-skill pass rate is +{:.1}% higher than baseline.",
            summary.pass_rate_delta * 100.0
        ));
    } else if summary.pass_rate_delta < 0.0 {
        notes.push(format!(
            "With-skill pass rate is {:.1}% lower than baseline.",
            summary.pass_rate_delta * 100.0
        ));
    } else {
        notes.push("With-skill and baseline pass rates are equal.".to_string());
    }

    let missing_with_skill: Vec<String> = assertions
        .iter()
        .filter(|a| !a.with_skill.passed)
        .map(|a| a.label.clone())
        .collect();
    if !missing_with_skill.is_empty() {
        notes.push(format!(
            "Assertions still failing with skill: {}.",
            missing_with_skill.join(", ")
        ));
    }

    let non_discriminating: Vec<String> = assertions
        .iter()
        .filter(|a| a.with_skill.passed == a.without_skill.passed)
        .map(|a| a.label.clone())
        .collect();
    if !non_discriminating.is_empty() {
        notes.push(format!(
            "Non-discriminating assertions (same result both configs): {}.",
            non_discriminating.join(", ")
        ));
    }

    if summary.with_skill_avg_tokens > summary.without_skill_avg_tokens * 1.45 {
        notes.push("With-skill token cost is materially higher than baseline.".to_string());
    }

    notes
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn mean_stddev(samples: &[f64]) -> (f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let variance = samples
        .iter()
        .map(|sample| {
            let delta = sample - mean;
            delta * delta
        })
        .sum::<f64>()
        / samples.len() as f64;
    (mean, variance.sqrt())
}

fn eval_id(skill_name: &str, timestamp_ms: u64) -> String {
    let timestamp = timestamp_ms.to_string();
    let suffix = stable_hash(&[skill_name, timestamp.as_str()]);
    format!("eval-{timestamp_ms}-{:08x}", suffix as u32)
}

fn stable_hash(parts: &[&str]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    hasher.finish()
}

fn now_ms() -> u64 {
    let nanos = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
    if nanos <= 0 {
        return 0;
    }
    (nanos / 1_000_000) as u64
}

fn word_count(text: &str) -> usize {
    text.split_whitespace().count()
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use super::*;

    fn strong_skill() -> SkillEvalInput {
        SkillEvalInput {
            name: "skill-creator".to_string(),
            source: "anthropics/skills".to_string(),
            description:
                "Create, update, and benchmark skills with a structured evaluation workflow."
                    .to_string(),
            body: r#"
1. Read the target skill and identify gaps.
2. Draft updated instructions and include examples.
3. Validate with benchmark cases and check outputs.

Always confirm risky operations before running commands.

Example:
```bash
cargo test -p moltis-tools skill_tools
```
"#
            .to_string(),
            allowed_tools: vec!["Read".to_string(), "Bash(cargo test:*)".to_string()],
            compatibility: Some("Use sandbox mode for risky commands.".to_string()),
            requires: SkillRequirements {
                bins: vec!["cargo".to_string()],
                any_bins: vec!["python3".to_string()],
                install: vec![],
            },
        }
    }

    #[test]
    fn run_skill_eval_generates_completed_run() {
        let run = run_skill_eval(&strong_skill(), Some(4));
        assert_eq!(run.status, "completed");
        assert_eq!(run.benchmark.metadata.rounds, 4);
        assert_eq!(run.benchmark.configurations.len(), 2);
        assert!(!run.benchmark.assertions.is_empty());
    }

    #[test]
    fn strong_skill_beats_baseline() {
        let run = run_skill_eval(&strong_skill(), Some(5));
        let summary = run.benchmark.run_summary;
        assert!(summary.pass_rate_delta > 0.0);
        assert!(summary.with_skill_pass_rate > summary.without_skill_pass_rate);
    }

    #[test]
    fn weak_skill_has_limited_signal() {
        let mut weak = strong_skill();
        weak.body = "Do stuff.".to_string();
        weak.allowed_tools.clear();
        weak.requires = SkillRequirements::default();

        let run = run_skill_eval(&weak, Some(3));
        assert!(
            run.benchmark.run_summary.with_skill_pass_rate < 0.75,
            "unexpectedly high pass rate: {}",
            run.benchmark.run_summary.with_skill_pass_rate
        );
    }

    #[test]
    fn store_append_and_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("skills-evals.json");
        let store = SkillEvalStore::new(path.clone());
        let run = run_skill_eval(&strong_skill(), Some(2));
        let id = run.id.clone();

        store.append(run).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.runs.len(), 1);
        assert_eq!(loaded.runs[0].id, id);

        let on_disk = std::fs::read_to_string(path).unwrap();
        assert!(on_disk.contains("\"version\": 1"));
    }
}
