use std::ops::DerefMut;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context};
use octocrab::models::CommentId;
use serde::Serialize;
use serde_json::json;
use sqlx::sqlite::SqliteRow;
use sqlx::{Connection, Error, FromRow, Row, SqliteConnection};
use time::OffsetDateTime;
use tokio::sync::Mutex;
use uuid::Uuid;

pub struct QueuedEvent {
    pub id: Uuid,
    pub job_id: Option<Uuid>,
    pub event: String,
    pub payload: Vec<u8>,
    pub created_utc: OffsetDateTime,
}

impl FromRow<'_, SqliteRow> for QueuedEvent {
    fn from_row(row: &SqliteRow) -> Result<Self, Error> {
        let id = row.try_get::<Vec<u8>, _>("id")?;
        let id = Uuid::from_slice(&id).map_err(|e| Error::Decode(Box::new(e)))?;

        let job_id = row.try_get::<Option<Vec<u8>>, _>("job_id")?;
        let job_id = match job_id {
            None => None,
            Some(id) => Some(Uuid::from_slice(&id).map_err(|e| Error::Decode(Box::new(e)))?),
        };

        Ok(Self {
            id,
            job_id,
            event: row.try_get("event")?,
            payload: row.try_get("payload")?,
            created_utc: row.try_get("created_utc")?,
        })
    }
}

#[derive(Debug, PartialEq, sqlx::FromRow, Serialize)]
pub struct BenchJob {
    #[sqlx(try_from = "Vec<u8>")]
    pub id: Uuid,
    pub event_queued_utc: OffsetDateTime,
    pub created_utc: OffsetDateTime,
    pub finished_utc: Option<OffsetDateTime>,
}

#[derive(sqlx::FromRow)]
pub struct BenchResult {
    pub name: String,
    pub result: f64,
}

/// The results of a comparison between two branches of Rustls
pub struct CompareResult {
    /// The diffs, per scenario
    pub diffs: Vec<ScenarioDiff>,
    /// Benchmark scenarios present in the candidate but missing in the baseline
    pub scenarios_missing_in_baseline: Vec<String>,
}

/// A diff for a particular scenario, obtained by comparing benchmark results between two versions
/// of rustls
#[derive(Clone, Debug, PartialEq, sqlx::FromRow)]
pub struct ScenarioDiff {
    pub scenario_name: String,
    #[sqlx(try_from = "i64")]
    pub scenario_kind: ScenarioKind,
    pub baseline_result: f64,
    pub candidate_result: f64,
    pub significance_threshold: f64,
    pub cachegrind_diff: String,
}

impl ScenarioDiff {
    pub fn diff(&self) -> f64 {
        self.candidate_result - self.baseline_result
    }

    pub fn diff_ratio(&self) -> f64 {
        self.diff() / self.baseline_result
    }
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ScenarioKind {
    Icount = 0,
}

impl TryFrom<i64> for ScenarioKind {
    type Error = anyhow::Error;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ScenarioKind::Icount),
            kind => bail!("invalid scenario kind: {kind}"),
        }
    }
}

#[derive(Clone)]
pub struct Db {
    sqlite: Arc<Mutex<SqliteConnection>>,
}

impl Db {
    pub fn with_connection(sqlite: Arc<Mutex<SqliteConnection>>) -> Self {
        Self { sqlite }
    }

    /// Store an incoming webhook to the database
    #[tracing::instrument(skip(self, payload), ret)]
    pub async fn enqueue_event(&self, event: &str, payload: &[u8]) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();
        let now = OffsetDateTime::now_utc();

        let mut conn = self.sqlite.lock().await;
        sqlx::query(
            "INSERT INTO event_queue (id, created_utc, event, payload) VALUES (?, ?, ?, ?)",
        )
        .bind(id.as_bytes().as_slice())
        .bind(now)
        .bind(event)
        .bind(payload)
        .execute(conn.deref_mut())
        .await?;

        Ok(id)
    }

    /// Retrieve the next incoming webhook we should handle
    #[tracing::instrument(skip(self))]
    pub async fn next_queued_event(&self) -> anyhow::Result<QueuedEvent> {
        let mut conn = self.sqlite.lock().await;
        let event = sqlx::query_as(
            r"
            SELECT *
            FROM event_queue
            ORDER BY created_utc
            LIMIT 1",
        )
        .fetch_one(conn.deref_mut())
        .await?;

        Ok(event)
    }

    #[tracing::instrument(skip(self), ret)]
    pub async fn queued_event_count(&self) -> anyhow::Result<i64> {
        let mut conn = self.sqlite.lock().await;
        let row = sqlx::query(
            r"
            SELECT COUNT(*) as count
            FROM event_queue",
        )
        .fetch_one(conn.deref_mut())
        .await?;

        Ok(row.try_get("count")?)
    }

    /// Create a new job and associate it to the provided event
    #[tracing::instrument(skip(self), ret)]
    pub async fn new_job_for_event(
        &self,
        event_id: Uuid,
        event_created_utc: OffsetDateTime,
    ) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();

        let mut conn = self.sqlite.lock().await;
        conn.transaction(|t| {
            Box::pin(async move {
                // Create new job
                let now = OffsetDateTime::now_utc();
                sqlx::query(
                    "INSERT INTO jobs (id, event_queued_utc, created_utc) VALUES (?, ?, ?)",
                )
                .bind(id.as_bytes().as_slice())
                .bind(event_created_utc)
                .bind(now)
                .execute(t.deref_mut())
                .await?;

                // Associate the event to this job
                sqlx::query("UPDATE event_queue SET job_id = ? WHERE id = ?")
                    .bind(id.as_bytes().as_slice())
                    .bind(event_id.as_bytes().as_slice())
                    .execute(t.deref_mut())
                    .await?;

                Ok::<_, Error>(())
            })
        })
        .await?;

        Ok(id)
    }

    /// Mark the job as finished
    #[tracing::instrument(skip(self))]
    pub async fn job_finished(&self, id: Uuid) -> anyhow::Result<()> {
        let finished_utc = OffsetDateTime::now_utc();

        let mut conn = self.sqlite.lock().await;
        sqlx::query("UPDATE jobs SET finished_utc = ? WHERE id = ?")
            .bind(Some(finished_utc))
            .bind(id.as_bytes().as_slice())
            .execute(conn.deref_mut())
            .await?;

        Ok(())
    }

    /// Retrieve a job by its id
    pub async fn job(&self, id: Uuid) -> anyhow::Result<BenchJob> {
        let job = self
            .maybe_job(id)
            .await?
            .ok_or(anyhow!("job not found: {id}"))?;
        Ok(job)
    }

    #[tracing::instrument(skip(self), ret)]
    pub async fn maybe_job(&self, id: Uuid) -> anyhow::Result<Option<BenchJob>> {
        let mut conn = self.sqlite.lock().await;
        let job = sqlx::query_as(
            r"
            SELECT *
            FROM jobs
            WHERE id = ?",
        )
        .bind(id.as_bytes().as_slice())
        .fetch_optional(conn.deref_mut())
        .await?;

        Ok(job)
    }

    /// Delete handled webhook from the database
    #[tracing::instrument(skip(self))]
    pub async fn delete_event(&self, id: Uuid) -> anyhow::Result<()> {
        let mut conn = self.sqlite.lock().await;
        sqlx::query("DELETE FROM event_queue WHERE id = ?")
            .bind(id.as_bytes().as_slice())
            .execute(conn.deref_mut())
            .await?;

        Ok(())
    }

    /// Store the results of a bench run to the database
    #[tracing::instrument(skip(self, icount_results), ret)]
    pub async fn store_run_results(
        &self,
        icount_results: Vec<(String, f64)>,
    ) -> anyhow::Result<Uuid> {
        let bench_run_id = Uuid::new_v4();

        let mut conn = self.sqlite.lock().await;
        conn.transaction(|t| {
            Box::pin(async move {
                // Create new bench run
                let now = OffsetDateTime::now_utc();
                sqlx::query("INSERT INTO bench_runs (id, created_utc) VALUES (?, ?)")
                    .bind(bench_run_id.as_bytes().as_slice())
                    .bind(now)
                    .execute(t.deref_mut())
                    .await?;

                // Add benchmark results
                for (name, result) in icount_results {
                    sqlx::query(
                        "INSERT INTO bench_results (bench_run_id, name, result) VALUES (?, ?, ?)",
                    )
                    .bind(bench_run_id.as_bytes().as_slice())
                    .bind(name)
                    .bind(result)
                    .execute(t.deref_mut())
                    .await?;
                }

                Ok::<_, Error>(())
            })
        })
        .await?;

        Ok(bench_run_id)
    }

    /// Retrieve the results since the provided cutoff date
    #[tracing::instrument(skip(self))]
    pub async fn result_history(
        &self,
        cutoff_date: OffsetDateTime,
    ) -> anyhow::Result<Vec<BenchResult>> {
        let mut conn = self.sqlite.lock().await;
        let results = sqlx::query_as(
            r"
            SELECT name, result
            FROM bench_results JOIN
                (SELECT id FROM bench_runs WHERE created_utc > ? ORDER BY created_utc)
            ON id = bench_run_id",
        )
        .bind(cutoff_date)
        .fetch_all(conn.deref_mut())
        .await?;

        Ok(results)
    }

    #[tracing::instrument(skip(self, diffs))]
    pub async fn store_comparison_result(
        &self,
        baseline_commit: String,
        candidate_commit: String,
        scenarios_missing: Vec<String>,
        diffs: Vec<ScenarioDiff>,
    ) -> anyhow::Result<Uuid> {
        let scenarios_missing = if scenarios_missing.is_empty() {
            None
        } else {
            Some(json!(scenarios_missing).to_string())
        };

        let mut conn = self.sqlite.lock().await;
        let id = conn.transaction(|t| {
            Box::pin(async move {
                // Create new comparison run
                let id = Uuid::new_v4();
                let now = OffsetDateTime::now_utc();
                sqlx::query(
                    "INSERT INTO comparison_runs (id, created_utc, baseline_commit, candidate_commit, scenarios_missing_in_baseline) VALUES (?, ?, ?, ?, ?)",
                )
                    .bind(id.as_bytes().as_slice())
                    .bind(now)
                    .bind(baseline_commit)
                    .bind(candidate_commit)
                    .bind(scenarios_missing)
                    .execute(t.deref_mut())
                    .await?;

                // Insert the associated diffs
                for diff in diffs {
                    sqlx::query(
                        "INSERT INTO scenario_diffs (comparison_run_id, scenario_name, scenario_kind, baseline_result, candidate_result, significance_threshold, cachegrind_diff) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    )
                        .bind(id.as_bytes().as_slice())
                        .bind(diff.scenario_name)
                        .bind(diff.scenario_kind as i64)
                        .bind(diff.baseline_result)
                        .bind(diff.candidate_result)
                        .bind(diff.significance_threshold)
                        .bind(diff.cachegrind_diff)
                        .execute(t.deref_mut())
                        .await?;
                }

                Ok::<_, Error>(id)
            })
        })
            .await?;

        Ok(id)
    }

    #[tracing::instrument(skip(self))]
    pub async fn comparison_result(
        &self,
        baseline_commit: &str,
        candidate_commit: &str,
    ) -> anyhow::Result<Option<CompareResult>> {
        let mut conn = self.sqlite.lock().await;
        let row = sqlx::query(
            r"
            SELECT id, created_utc, scenarios_missing_in_baseline
            FROM comparison_runs
            WHERE baseline_commit = ? AND candidate_commit = ?",
        )
        .bind(baseline_commit)
        .bind(candidate_commit)
        .fetch_optional(conn.deref_mut())
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let id: Vec<u8> = row.try_get("id")?;
        let scenarios_missing_in_baseline: Option<String> =
            row.try_get("scenarios_missing_in_baseline")?;
        let scenarios_missing_in_baseline = match scenarios_missing_in_baseline {
            None => Vec::new(),
            Some(missing) => serde_json::from_str(&missing)
                .context("invalid JSON in db for `scenarios_missing_in_baseline` column")?,
        };

        let diffs = sqlx::query_as(
            r"
            SELECT *
            FROM scenario_diffs
            WHERE comparison_run_id = ?",
        )
        .bind(id)
        .fetch_all(conn.deref_mut())
        .await?;

        Ok(Some(CompareResult {
            diffs,
            scenarios_missing_in_baseline,
        }))
    }

    #[tracing::instrument(skip(self))]
    pub async fn cachegrind_diff(
        &self,
        baseline_commit: &str,
        candidate_commit: &str,
        scenario_name: &str,
    ) -> anyhow::Result<Option<String>> {
        let mut conn = self.sqlite.lock().await;
        let row = sqlx::query(
            r"
            SELECT cachegrind_diff
            FROM comparison_runs JOIN scenario_diffs ON comparison_runs.id = scenario_diffs.comparison_run_id
            WHERE baseline_commit = ? AND candidate_commit = ? AND scenario_name = ?",
        )
            .bind(baseline_commit)
            .bind(candidate_commit)
            .bind(scenario_name)
            .fetch_optional(conn.deref_mut())
            .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        Ok(Some(row.try_get("cachegrind_diff")?))
    }

    #[tracing::instrument(skip(self))]
    pub async fn store_result_comment_id(
        &self,
        pr_number: u64,
        comment_id: CommentId,
    ) -> anyhow::Result<()> {
        let mut conn = self.sqlite.lock().await;
        sqlx::query("INSERT INTO result_comments (pr_number, comment_id) VALUES (?, ?)")
            .bind(pr_number as i64)
            .bind(comment_id.into_inner() as i64)
            .execute(conn.deref_mut())
            .await?;

        Ok(())
    }

    #[tracing::instrument(skip(self), ret)]
    pub async fn result_comment_id(&self, pr_number: u64) -> anyhow::Result<Option<CommentId>> {
        let mut conn = self.sqlite.lock().await;
        let row = sqlx::query(
            r"
            SELECT comment_id
            FROM result_comments
            WHERE pr_number = ?",
        )
        .bind(pr_number as i64)
        .fetch_optional(conn.deref_mut())
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let comment_id: i64 = row.try_get("comment_id")?;
        Ok(Some((comment_id as u64).into()))
    }

    #[cfg(test)]
    pub async fn jobs(&self) -> anyhow::Result<Vec<BenchJob>> {
        let mut conn = self.sqlite.lock().await;
        let jobs = sqlx::query_as(
            r"
            SELECT *
            FROM jobs
            ORDER BY created_utc",
        )
        .fetch_all(conn.deref_mut())
        .await?;

        Ok(jobs)
    }

    #[cfg(test)]
    pub async fn queued_events(&self) -> anyhow::Result<Vec<QueuedEvent>> {
        let mut conn = self.sqlite.lock().await;
        let jobs = sqlx::query_as(
            r"
            SELECT *
            FROM event_queue
            ORDER BY created_utc",
        )
        .fetch_all(conn.deref_mut())
        .await?;

        Ok(jobs)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::MIGRATOR;
    use time::Duration;

    #[tokio::test]
    async fn test_store_load_results_round_trips_and_orders_by_time() -> anyhow::Result<()> {
        let db = empty_db().await;

        db.store_run_results(vec![("foo".to_string(), 42.0)])
            .await?;
        db.store_run_results(vec![("foo".to_string(), 41.0)])
            .await?;
        db.store_run_results(vec![("foo".to_string(), 43.0)])
            .await?;

        let history = db
            .result_history(OffsetDateTime::now_utc() - Duration::minutes(1))
            .await?;

        assert_eq!(history.len(), 3);
        assert_eq!(history[0].result, 42.0);
        assert_eq!(history[1].result, 41.0);
        assert_eq!(history[2].result, 43.0);

        Ok(())
    }

    #[tokio::test]
    async fn test_store_load_event_round_trips_and_orders_by_time() -> anyhow::Result<()> {
        let db = empty_db().await;

        let id1 = db.enqueue_event("foo", &[1, 2, 3, 4]).await?;
        let id2 = db.enqueue_event("bar", &[1, 2, 3, 4]).await?;

        let event = db.next_queued_event().await?;
        assert_eq!(id1, event.id);
        assert_eq!(event.payload, [1, 2, 3, 4]);

        db.delete_event(id1).await?;

        let event = db.next_queued_event().await?;
        assert_eq!(id2, event.id);

        let job_id = db.new_job_for_event(event.id, event.created_utc).await?;
        let job = db.job(job_id).await?;
        assert_eq!(job.id, job_id);
        assert_eq!(job.finished_utc, None);

        db.job_finished(job_id).await?;
        let job = db.job(job_id).await?;

        assert!(job.finished_utc.is_some());

        Ok(())
    }

    #[tokio::test]
    async fn test_store_load_comparison_diff_round_trips() -> anyhow::Result<()> {
        let db = empty_db().await;

        let baseline_commit = "c609978130843652696e748bb9c9f73703d79089";
        let candidate_commit = "7faf240afbdbb4e76c47ff5f3f049c7a78c9c843";
        let diffs = vec![
            ScenarioDiff {
                scenario_name: "foo".to_string(),
                scenario_kind: ScenarioKind::Icount,
                candidate_result: 42.0,
                baseline_result: 42.5,
                significance_threshold: 0.3,
                cachegrind_diff: "fake cachegrind diff".to_string(),
            },
            ScenarioDiff {
                scenario_name: "bar".to_string(),
                scenario_kind: ScenarioKind::Icount,
                candidate_result: 100.0,
                baseline_result: 104.0,
                significance_threshold: 5.0,
                cachegrind_diff: "fake cachegrind diff 2".to_string(),
            },
        ];

        db.store_comparison_result(
            baseline_commit.to_string(),
            candidate_commit.to_string(),
            Vec::new(),
            diffs.clone(),
        )
        .await?;
        let comparison = db
            .comparison_result(baseline_commit, candidate_commit)
            .await?;

        let Some(mut comparison) = comparison else {
            bail!("no comparison results found for the provided commits");
        };

        assert!(comparison.scenarios_missing_in_baseline.is_empty());
        assert_eq!(comparison.diffs.len(), 2);

        comparison
            .diffs
            .sort_by(|d1, d2| d1.scenario_name.cmp(&d2.scenario_name));
        assert_eq!(comparison.diffs[0], diffs[1]);

        let cachegrind_diff = db
            .cachegrind_diff(baseline_commit, candidate_commit, "foo")
            .await?;
        assert_eq!(cachegrind_diff, Some("fake cachegrind diff".to_string()));

        let cachegrind_diff = db
            .cachegrind_diff(baseline_commit, candidate_commit, "non-existent")
            .await?;
        assert_eq!(cachegrind_diff, None);

        Ok(())
    }

    #[tokio::test]
    async fn test_store_load_comparison_diff_with_missing_scenarios_round_trips(
    ) -> anyhow::Result<()> {
        let db = empty_db().await;

        let baseline_commit = "c609978130843652696e748bb9c9f73703d79089";
        let candidate_commit = "7faf240afbdbb4e76c47ff5f3f049c7a78c9c843";
        let diffs = vec![ScenarioDiff {
            scenario_name: "foo".to_string(),
            scenario_kind: ScenarioKind::Icount,
            candidate_result: 42.0,
            baseline_result: 42.5,
            significance_threshold: 0.3,
            cachegrind_diff: "fake cachegrind diff".to_string(),
        }];

        db.store_comparison_result(
            baseline_commit.to_string(),
            candidate_commit.to_string(),
            vec!["bar".to_string()],
            diffs.clone(),
        )
        .await?;
        let comparison = db
            .comparison_result(baseline_commit, candidate_commit)
            .await?;

        let Some(comparison) = comparison else {
            bail!("no comparison results found for the provided commits");
        };

        assert_eq!(
            comparison.scenarios_missing_in_baseline,
            ["bar".to_string()]
        );
        assert_eq!(comparison.diffs.len(), 1);
        assert_eq!(comparison.diffs[0], diffs[0]);

        Ok(())
    }

    #[tokio::test]
    async fn test_store_load_result_comment_id_round_trips() -> anyhow::Result<()> {
        let db = empty_db().await;

        let original_comment_id = 100.into();
        db.store_result_comment_id(42, original_comment_id).await?;
        let comment_id = db.result_comment_id(42).await?;

        assert_eq!(comment_id, Some(original_comment_id));

        let comment_id = db.result_comment_id(43).await?;
        assert_eq!(comment_id, None);

        Ok(())
    }

    async fn empty_db() -> Db {
        let mut sqlite = SqliteConnection::connect("sqlite::memory:").await.unwrap();
        MIGRATOR.run(&mut sqlite).await.unwrap();
        Db::with_connection(Arc::new(Mutex::new(sqlite)))
    }
}