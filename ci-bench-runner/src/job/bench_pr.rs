use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::fs::File;
use std::ops::Deref;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context};
use octocrab::models::pulls::PullRequest;
use octocrab::models::webhook_events::payload::PullRequestWebhookEventAction;
use octocrab::models::webhook_events::{WebhookEvent, WebhookEventPayload};
use octocrab::models::StatusState;
use tempfile::TempDir;
use time::{Duration, OffsetDateTime};
use tracing::{debug, error, info, trace};

use crate::db::{BenchResult, CompareResult, ScenarioDiff, ScenarioKind};
use crate::event_queue::JobContext;
use crate::github::{self, update_commit_status, CommentEvent, PullRequestReviewEvent};
use crate::job::read_results;
use crate::runner::{BenchRunner, Log};
use crate::RepoAndSha;

static ALLOWED_AUTHOR_ASSOCIATIONS: &[&str] = &[
    // The owner of the repository
    "OWNER",
    // A member of the organization that owns the repository
    "MEMBER",
    // Someone invited to collaborate on the repository
    "COLLABORATOR",
];

/// Handle an "issue comment"
///
/// Runs the PR benchmarks if the comment:
/// - Has just been created (edits are ignored);
/// - Has been posted to a PR (not to an issue);
/// - Has been posted by an authorized user; and
/// - Addresses the bot with the right command (`@rustls-bench bench`).
pub async fn handle_issue_comment(ctx: JobContext<'_>) -> anyhow::Result<()> {
    // Ideally, we'd use WebhookEvent::try_from_header_and_body from `octocrab`, but it doesn't have
    // the `author_association` field on the comment, which we need.
    let Ok(payload) = serde_json::from_slice::<CommentEvent>(ctx.event_payload) else {
        error!("Invalid JSON payload, ignoring event");
        return Ok(());
    };

    if payload.issue.pull_request.is_none() {
        trace!("The comment was to a plain issue (not to a PR), ignoring event");
        return Ok(());
    };

    let body = &payload.comment.body;
    if payload.action != "created" {
        trace!("ignoring event for `{}` action", payload.action);
        return Ok(());
    }
    if payload.comment.user.login == "rustls-bench" {
        trace!("ignoring comment from rustls-bench");
        return Ok(());
    }
    if !ALLOWED_AUTHOR_ASSOCIATIONS.contains(&payload.comment.author_association.as_str()) {
        trace!(
            "ignoring comment from unauthorized user (author association = {})",
            payload.comment.author_association
        );
        return Ok(());
    }

    let octocrab = ctx.octocrab.cached();
    if body.contains("@rustls-bench bench") {
        let pr = octocrab
            .pulls(&ctx.config.github_repo_owner, &ctx.config.github_repo_name)
            .get(payload.issue.number)
            .await
            .context("unable to get PR details")?;

        let branches = pr_branches(&pr).ok_or(anyhow!("unable to get PR branch details"))?;
        bench_pr(ctx, pr.number, branches).await
    } else if body.contains("@rustls-bench") {
        debug!("The comment was addressed at rustls-bench, but it is an unknown command!");
        let comment = "Unrecognized command. Available commands are:\n\
        * `@rustls-bench bench`: runs the instruction count benchmarks and reports the results";
        octocrab
            .issues(&ctx.config.github_repo_owner, &ctx.config.github_repo_name)
            .create_comment(payload.issue.number, comment)
            .await?;
        Ok(())
    } else {
        trace!("The comment was not addressed at rustls-bench");
        Ok(())
    }
}

/// Handle a "PR review"
///
/// Runs the PR benchmarks if the review:
/// - Is an approval; and
/// - Has just been submitted by an authorized user.
pub async fn handle_pr_review(ctx: JobContext<'_>) -> anyhow::Result<()> {
    // Ideally, we'd use WebhookEvent::try_from_header_and_body from `octocrab`, but it doesn't have
    // the `author_association` field on the review (which we need) and it requires the `head` field
    // on the PR (which is not provided).
    let Ok(payload) = serde_json::from_slice::<PullRequestReviewEvent>(ctx.event_payload) else {
        error!(
            event = ctx.event,
            body = String::from_utf8_lossy(ctx.event_payload).to_string(),
            "invalid JSON payload, ignoring event"
        );
        return Ok(());
    };

    if payload.action != "submitted" {
        trace!("ignoring pull request event with action {}", payload.action);
        return Ok(());
    }

    if !ALLOWED_AUTHOR_ASSOCIATIONS.contains(&payload.review.author_association.as_str()) {
        trace!("ignoring review from untrusted author");
        return Ok(());
    }

    if payload.review.state != "approved" {
        trace!("ignoring review with non-approved status");
        return Ok(());
    }

    let pr = payload.pull_request;
    let Some(mut branches) = pr_branches(&pr) else {
        error!("unable to obtain branches from payload, ignoring event");
        return Ok(());
    };

    // Ensure we bench the commit that was reviewed, and not something else
    branches.candidate.commit_sha = payload.review.commit_id;

    bench_pr(ctx, pr.number, branches).await
}

/// Handle a "PR update"
///
/// Runs the PR benchmarks if:
/// - The PR originates from a trusted branch (i.e. branches from the repository, not from forks); and
/// - The PR was just created (action is `opened`), or its branches were updated (action is `synchronize`).
pub async fn handle_pr_update(ctx: JobContext<'_>) -> anyhow::Result<()> {
    let Ok(event) = WebhookEvent::try_from_header_and_body(ctx.event, ctx.event_payload) else {
        error!(
            event = ctx.event,
            body = String::from_utf8_lossy(ctx.event_payload).to_string(),
            "invalid JSON payload, ignoring event"
        );
        return Ok(());
    };

    let WebhookEventPayload::PullRequest(payload) = event.specific else {
        error!("invalid JSON payload, ignoring event");
        return Ok(());
    };

    if payload.action != PullRequestWebhookEventAction::Opened
        && payload.action != PullRequestWebhookEventAction::Synchronize
    {
        trace!(
            "ignoring pull request event with action {:?}",
            payload.action
        );
        return Ok(());
    }

    let Some(branches) = pr_branches(&payload.pull_request) else {
        error!("unable to obtain branches from payload, ignoring event");
        return Ok(());
    };

    if branches.baseline.clone_url != branches.candidate.clone_url {
        trace!(
            "ignoring pull request update for forked repo (base repo = {}, head repo = {})",
            branches.baseline.clone_url,
            branches.candidate.clone_url
        );
        return Ok(());
    }

    bench_pr(ctx, payload.pull_request.number, branches).await
}

pub async fn bench_pr(
    ctx: JobContext<'_>,
    pr_number: u64,
    branches: PrBranches,
) -> anyhow::Result<()> {
    if branches.baseline.branch_name != "main" {
        trace!("ignoring bench request for PR with non-main base");
        return Ok(());
    }

    let octocrab = ctx.octocrab.cached();
    update_commit_status(
        branches.candidate.commit_sha.clone(),
        StatusState::Pending,
        ctx.config,
        &octocrab,
    )
    .await;

    let cached_result = ctx
        .db
        .comparison_result(
            &branches.baseline.commit_sha,
            &branches.candidate.commit_sha,
        )
        .await?;
    let result = match cached_result {
        Some(result) => Ok(result),
        None => {
            let mut logs = BenchPrLogs::default();
            bench_pr_and_cache_results(&ctx, branches.clone(), &mut logs)
                .await
                .map_err(|error| BenchPrError { error, logs })
        }
    };

    let cachegrind_diff_url = format!(
        "{}/comparisons/{}:{}/cachegrind-diff",
        ctx.config.app_base_url, branches.baseline.commit_sha, branches.candidate.commit_sha
    );
    let mut comment = BenchPrResult {
        result,
        branches: branches.clone(),
    }
    .into_markdown_comment(&cachegrind_diff_url);
    github::maybe_truncate_comment(&mut comment);

    let issues = octocrab.issues(&ctx.config.github_repo_owner, &ctx.config.github_repo_name);
    if let Some(comment_id) = ctx.db.result_comment_id(pr_number).await? {
        issues.update_comment(comment_id, comment).await?;
    } else {
        let comment = issues.create_comment(pr_number, comment).await?;
        ctx.db
            .store_result_comment_id(pr_number, comment.id)
            .await?;
    }

    update_commit_status(
        branches.candidate.commit_sha.clone(),
        StatusState::Success,
        ctx.config,
        &octocrab,
    )
    .await;

    Ok(())
}

async fn bench_pr_and_cache_results(
    ctx: &JobContext<'_>,
    branches: PrBranches,
    logs: &mut BenchPrLogs,
) -> anyhow::Result<CompareResult> {
    let cutoff_date = OffsetDateTime::now_utc() - Duration::days(30);
    let historical_results = ctx
        .db
        .result_history(cutoff_date)
        .await
        .context("could not obtain result history")?;
    let significance_thresholds = calculate_significance_thresholds(historical_results);

    let job_output_dir = ctx.job_output_dir.clone();
    let runner = ctx.bench_runner.clone();
    let branches_cloned = branches.clone();
    let (result, task_logs) = tokio::task::spawn_blocking(move || {
        let mut logs = BenchPrLogs::default();

        let result = compare_refs(
            &branches_cloned,
            &job_output_dir,
            &mut logs,
            runner.deref(),
            &significance_thresholds,
        );

        if let Err(e) = &result {
            error!(cause = e.to_string(), "unable to compare refs");
        }

        (result, logs)
    })
    .await
    .context("benchmarking task crashed")?;

    *logs = task_logs;

    // Write the task logs so they are available even if commenting to GitHub fails
    let mut s = String::new();
    writeln!(s, "### Candidate").ok();
    BenchPrResult::write_logs_for_run(&mut s, &logs.candidate);
    writeln!(s, "### Base").ok();
    BenchPrResult::write_logs_for_run(&mut s, &logs.base);
    fs::write(ctx.job_output_dir.join("logs.md"), s).context("unable to write job logs")?;

    if let Ok(result) = &result {
        ctx.db
            .store_comparison_result(
                branches.baseline.commit_sha.clone(),
                branches.candidate.commit_sha.clone(),
                result.scenarios_missing_in_baseline.clone(),
                result.diffs.clone(),
            )
            .await
            .context("could not store comparison results")?;
    }

    result
}

fn pr_branches(pr: &PullRequest) -> Option<PrBranches> {
    Some(PrBranches {
        candidate: RepoAndSha {
            branch_name: pr.head.ref_field.clone(),
            commit_sha: pr.head.sha.clone(),
            clone_url: pr.head.repo.as_ref()?.clone_url.as_ref()?.to_string(),
        },
        baseline: RepoAndSha {
            branch_name: pr.base.ref_field.clone(),
            commit_sha: pr.base.sha.clone(),
            clone_url: pr.base.repo.as_ref()?.clone_url.as_ref()?.to_string(),
        },
    })
}

fn compare_refs(
    pr_branches: &PrBranches,
    job_output_path: &Path,
    logs: &mut BenchPrLogs,
    runner: &dyn BenchRunner,
    significance_thresholds: &HashMap<String, f64>,
) -> anyhow::Result<CompareResult> {
    let candidate_repo = TempDir::new().context("Unable to create temp dir")?;
    let candidate_repo_path = candidate_repo.path().to_owned();

    let base_repo = TempDir::new().context("Unable to create temp dir")?;
    let base_repo_path = base_repo.path().to_owned();

    runner.checkout_and_run_benchmarks(
        &pr_branches.candidate,
        &candidate_repo_path,
        &job_output_path.join("candidate"),
        &mut logs.candidate,
    )?;

    runner.checkout_and_run_benchmarks(
        &pr_branches.baseline,
        &base_repo_path,
        &job_output_path.join("base"),
        &mut logs.base,
    )?;

    info!("comparing results");
    let baseline = read_results(&job_output_path.join("base/results/icounts.csv"))?;
    let candidate = read_results(&job_output_path.join("candidate/results/icounts.csv"))?;
    let (diffs, missing) = compare_results(
        &job_output_path.join("base/results/cachegrind"),
        &job_output_path.join("candidate/results/cachegrind"),
        &baseline,
        &candidate,
        significance_thresholds,
    )?;

    Ok(CompareResult {
        diffs,
        scenarios_missing_in_baseline: missing,
    })
}

fn calculate_significance_thresholds(historical_results: Vec<BenchResult>) -> HashMap<String, f64> {
    let mut results_by_name = HashMap::new();
    for result in historical_results {
        results_by_name
            .entry(result.name)
            .or_insert(Vec::new())
            .push(result.result as u64);
    }

    let mut outlier_bounds = HashMap::with_capacity(results_by_name.len());
    for (name, results) in results_by_name {
        // Ensure we have at least 10 results available
        if results.len() < 10 {
            continue;
        }

        // A bench result is significant if the change percentage exceeds a threshold derived
        // from historic change percentages. We use inter-quartile range fencing by a factor of 3.0,
        // similar to the Rust compiler's benchmarks.
        // (see https://github.com/rust-lang/rustc-perf/blob/4f313add609f43e928e98132358e8426ed3969ae/site/src/comparison.rs#L1219)
        let mut historic_changes = results
            .windows(2)
            .map(|window| (window[0] as f64 - window[1] as f64).abs() / window[0] as f64)
            .collect::<Vec<_>>();
        historic_changes.sort_unstable_by(|x, y| x.partial_cmp(y).unwrap_or(Ordering::Equal));

        let q1 = historic_changes[historic_changes.len() / 4];
        let q3 = historic_changes[(historic_changes.len() * 3) / 4];
        let iqr = q3 - q1;
        let iqr_multiplier = 3.0;
        let significance_threshold = f64::max(q3 + iqr * iqr_multiplier, DEFAULT_NOISE_THRESHOLD);
        outlier_bounds.insert(name, significance_threshold);
    }

    outlier_bounds
}

#[derive(Clone)]
pub struct PrBranches {
    pub baseline: RepoAndSha,
    pub candidate: RepoAndSha,
}

struct BenchPrResult {
    branches: PrBranches,
    result: Result<CompareResult, BenchPrError>,
}

struct BenchPrError {
    error: anyhow::Error,
    logs: BenchPrLogs,
}

#[derive(Default)]
struct BenchPrLogs {
    base: Vec<Log>,
    candidate: Vec<Log>,
}

impl BenchPrResult {
    fn into_markdown_comment(self, diff_url: &str) -> String {
        let checkout_details = self.checkout_details();

        let mut s = String::new();
        match self.result {
            Ok(bench_results) => {
                s = print_report(bench_results, diff_url);
                writeln!(s, "### Checkout details").ok();
                write!(s, "{checkout_details}").ok();
            }
            Err(error) => {
                writeln!(s, "# Error running benchmarks").ok();
                writeln!(s, "Cause:").ok();
                writeln!(s, "```\n{:?}\n```", error.error).ok();
                writeln!(s, "Checkout details:").ok();
                write!(s, "{checkout_details}").ok();
                writeln!(s, "## Logs").ok();
                writeln!(s, "### Candidate").ok();
                Self::write_logs_for_run(&mut s, &error.logs.candidate);
                writeln!(s, "### Base").ok();
                Self::write_logs_for_run(&mut s, &error.logs.base);
            }
        }

        s
    }

    fn write_logs_for_run(s: &mut String, logs: &[Log]) {
        if logs.is_empty() {
            writeln!(s, "_Not available_").ok();
        }

        for log in logs {
            Self::write_log(s, log);
        }
    }

    fn write_log(s: &mut String, log: &Log) {
        Self::write_log_part(s, "command", &log.command);
        Self::write_log_part(s, "cwd", &log.cwd);
        Self::write_log_part(s, "stdout", &String::from_utf8_lossy(&log.stdout));
        Self::write_log_part(s, "stderr", &String::from_utf8_lossy(&log.stderr));
    }

    fn write_log_part(s: &mut String, part_name: &str, part: &str) {
        write!(s, "{part_name}:").ok();
        if part.trim().is_empty() {
            writeln!(s, " _empty_.\n").ok();
        } else {
            writeln!(s, "\n```\n{}\n```\n", part.trim_end()).ok();
        }
    }

    fn checkout_details(&self) -> String {
        let mut s = String::new();
        writeln!(s, "- Base repo: {}", self.branches.baseline.clone_url).ok();
        writeln!(
            s,
            "- Base branch: {} ({})",
            self.branches.baseline.branch_name, self.branches.baseline.commit_sha,
        )
        .ok();
        writeln!(s, "- Candidate repo: {}", self.branches.candidate.clone_url).ok();
        writeln!(
            s,
            "- Candidate branch: {} ({})",
            self.branches.candidate.branch_name, self.branches.candidate.commit_sha,
        )
        .ok();
        s
    }
}

/// Returns an internal representation of the comparison between the baseline and the candidate
/// measurements
fn compare_results(
    baseline_cachegrind_dir: &Path,
    candidate_cachegrind_dir: &Path,
    baseline: &HashMap<String, f64>,
    candidate: &HashMap<String, f64>,
    significance_thresholds: &HashMap<String, f64>,
) -> anyhow::Result<(Vec<ScenarioDiff>, Vec<String>)> {
    let mut diffs = Vec::new();
    let mut missing = Vec::new();
    for (scenario, &instr_count) in candidate {
        let Some(&baseline_instr_count) = baseline.get(scenario) else {
            missing.push(scenario.clone());
            continue;
        };

        let cachegrind_diff =
            cachegrind_diff(baseline_cachegrind_dir, candidate_cachegrind_dir, scenario)?;

        diffs.push(ScenarioDiff {
            scenario_name: scenario.clone(),
            scenario_kind: ScenarioKind::Icount,
            baseline_result: baseline_instr_count,
            candidate_result: instr_count,
            significance_threshold: significance_thresholds
                .get(scenario)
                .cloned()
                .unwrap_or(DEFAULT_NOISE_THRESHOLD),
            cachegrind_diff,
        });
    }

    Ok((diffs, missing))
}

/// Prints a report of the comparison to stdout, using GitHub-flavored markdown
fn print_report(result: CompareResult, cachegrind_diff_url: &str) -> String {
    let (significant, negligible) = split_on_threshold(result.diffs);

    let mut s = String::new();
    writeln!(s, "# Benchmark results").ok();

    if !result.scenarios_missing_in_baseline.is_empty() {
        writeln!(s, "### ⚠️ Warning: missing benchmarks").ok();
        writeln!(s,).ok();
        writeln!(s, "The following benchmark scenarios are present in the candidate but not in the baseline:").ok();
        writeln!(s,).ok();
        for scenario in &result.scenarios_missing_in_baseline {
            writeln!(s, "* {scenario}").ok();
        }
    }

    writeln!(s, "## Significant instruction count differences").ok();
    if significant.is_empty() {
        writeln!(
            s,
            "_There are no significant instruction count differences_",
        )
        .ok();
    } else {
        table(&mut s, &significant, cachegrind_diff_url, true);
    }

    writeln!(s, "## Other instruction count differences").ok();
    if negligible.is_empty() {
        writeln!(s, "_There are no other instruction count differences_").ok();
    } else {
        writeln!(s, "<details>").ok();
        writeln!(s, "<summary>Click to expand</summary>\n").ok();
        table(&mut s, &negligible, cachegrind_diff_url, false);
        writeln!(s, "</details>\n").ok();
    }

    s
}

/// Splits the diffs into two `Vec`s, the first one containing the diffs that exceed the threshold,
/// the second one containing the rest
fn split_on_threshold(diffs: Vec<ScenarioDiff>) -> (Vec<ScenarioDiff>, Vec<ScenarioDiff>) {
    let mut significant = Vec::new();
    let mut negligible = Vec::new();

    for diff in diffs {
        if diff.diff_ratio().abs() < diff.significance_threshold {
            negligible.push(diff);
        } else {
            significant.push(diff);
        }
    }

    significant.sort_by(|s1, s2| {
        f64::partial_cmp(&s2.diff_ratio(), &s1.diff_ratio()).unwrap_or(Ordering::Equal)
    });
    negligible.sort_by(|s1, s2| {
        f64::partial_cmp(&s2.diff_ratio(), &s1.diff_ratio()).unwrap_or(Ordering::Equal)
    });

    (significant, negligible)
}

/// Renders the diffs as a markdown table
fn table(s: &mut String, diffs: &[ScenarioDiff], cachegrind_diff_url: &str, emoji_feedback: bool) {
    writeln!(s, "| Scenario | Baseline | Candidate | Diff | Threshold |").ok();
    writeln!(s, "| --- | ---: | ---: | ---: | ---: |").ok();
    for diff in diffs {
        let emoji = match emoji_feedback {
            true if diff.diff() > 0.0 => "⚠️ ",
            true if diff.diff() < 0.0 => "✅ ",
            _ => "",
        };

        let cachegrind_diff_url = format!("{cachegrind_diff_url}/{}", diff.scenario_name);

        writeln!(
            s,
            "| {} | {} | {} | {emoji}[{}]({cachegrind_diff_url}) ({:.2}%) | {:.2}% |",
            diff.scenario_name,
            diff.baseline_result,
            diff.candidate_result,
            diff.diff(),
            diff.diff_ratio() * 100.0,
            diff.significance_threshold * 100.0
        )
        .ok();
    }
}

/// Returns the detailed instruction diff between the baseline and the candidate
pub fn cachegrind_diff(
    baseline: &Path,
    candidate: &Path,
    scenario: &str,
) -> anyhow::Result<String> {
    // The latest version of valgrind has deprecated cg_diff, which has been superseded by
    // cg_annotate. Many systems are running older versions, though, so we are sticking with cg_diff
    // for the time being.

    let tmp_path = Path::new("ci-bench-tmp");
    let tmp = File::create(tmp_path).context("cannot create temp file for cg_diff")?;

    // cg_diff generates a diff between two cachegrind output files in a custom format that is not
    // user-friendly
    let cg_diff = Command::new("cg_diff")
        // remove per-compilation uniqueness in symbols, eg
        // _ZN9hashbrown3raw21RawTable$LT$T$C$A$GT$14reserve_rehash17hc60392f3f3eac4b2E.llvm.9716880419886440089 ->
        // _ZN9hashbrown3raw21RawTable$LT$T$C$A$GT$14reserve_rehashE
        .arg("--mod-funcname=s/17h[0-9a-f]+E\\.llvm\\.\\d+/E/")
        .arg(baseline.join(scenario))
        .arg(candidate.join(scenario))
        .stdout(Stdio::from(tmp))
        .spawn()
        .context("cannot spawn cg_diff subprocess")?
        .wait()
        .context("error waiting for cg_diff to finish")?;

    if !cg_diff.success() {
        anyhow::bail!(
            "cg_diff finished with an error (code = {:?})",
            cg_diff.code()
        )
    }

    // cg_annotate transforms the output of cg_diff into something a user can understand
    let cg_annotate = Command::new("cg_annotate")
        .arg(tmp_path)
        .arg("--auto=no")
        .output()
        .context("error waiting for cg_annotate to finish")?;

    if !cg_annotate.status.success() {
        anyhow::bail!(
            "cg_annotate finished with an error (code = {:?})",
            cg_annotate.status.code()
        )
    }

    let annotated =
        String::from_utf8(cg_annotate.stdout).context("cg_annotate produced invalid UTF8")?;

    fs::remove_file(tmp_path).ok();

    Ok(annotated)
}

static DEFAULT_NOISE_THRESHOLD: f64 = 0.002;

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn calculate_outlier_bounds_not_enough_results() {
        let thresholds = calculate_significance_thresholds(Vec::new());
        assert_eq!(thresholds.len(), 0);
    }

    #[test]
    fn calculate_outlier_bounds_many_results() {
        let historical_results = vec![
            100.0, 97.0, 98.0, 101.0, 100.0, 99.0, 97.0, 102.0, 99.0, 98.0,
        ];

        let bench_results = historical_results
            .into_iter()
            .map(|result| BenchResult {
                name: "foo".to_string(),
                result,
            })
            .collect();
        let thresholds = calculate_significance_thresholds(bench_results);

        assert_eq!(thresholds.len(), 1);
        assert_eq!((thresholds["foo"] * 100.0).round(), 9.0);
    }

    #[test]
    fn calculate_outlier_bounds_minimal() {
        let bench_results = std::iter::repeat(1000.0)
            .take(10)
            .map(|result| BenchResult {
                name: "foo".to_string(),
                result,
            })
            .collect();
        let thresholds = calculate_significance_thresholds(bench_results);

        assert_eq!(thresholds.len(), 1);
        assert_eq!(thresholds["foo"], DEFAULT_NOISE_THRESHOLD);
    }
}