//! The single writer. One blocking task owns the read-write connection and applies commands in batches:
//! every command waiting in the channel (up to 64) goes into one transaction, so a burst of completions
//! costs one fsync. Replies are sent after the commit, so a reply means the row is durable. Retention and
//! checkpoints manage their own transactions and run between batches.

use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, TransactionBehavior, params};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::retention::{self, RetentionPolicy, RetentionReport};
use super::rows;
use crate::types::{Alert, Body, Finding, HardwareSample, ModelInfo, RequestRecord};

pub const CHANNEL_CAPACITY: usize = 1024;
const BATCH_MAX: usize = 64;

pub enum StoreCommand {
    PersistRequest {
        record: Box<RequestRecord>,
        bodies: Vec<Body>,
        findings: Vec<Finding>,
    },
    PersistAlert {
        alert: Alert,
        reply: oneshot::Sender<Result<i64>>,
    },
    AckAlert {
        id: i64,
        reply: oneshot::Sender<Result<bool>>,
    },
    AckAllAlerts {
        reply: oneshot::Sender<Result<usize>>,
    },
    PersistHardware(HardwareSample),
    UpsertModelsSeen(Vec<ModelInfo>),
    StartSession {
        app_version: String,
        reply: oneshot::Sender<Result<i64>>,
    },
    EndSession {
        id: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    DeleteBodies {
        request_id: Uuid,
        reply: oneshot::Sender<Result<usize>>,
    },
    RunRetention {
        policy: RetentionPolicy,
        now: DateTime<Utc>,
        reply: oneshot::Sender<Result<RetentionReport>>,
    },
    /// `PRAGMA optimize` and a truncating WAL checkpoint, for shutdown.
    Checkpoint {
        reply: oneshot::Sender<Result<()>>,
    },
}

impl StoreCommand {
    /// Commands that manage their own transactions and must not sit inside a batch.
    fn is_standalone(&self) -> bool {
        matches!(
            self,
            StoreCommand::RunRetention { .. } | StoreCommand::Checkpoint { .. }
        )
    }
}

/// The cloneable sender side. Every method is async because the channel is bounded: a flood of
/// completions backpressures the proxy's driver tasks rather than growing memory.
#[derive(Clone)]
pub struct StoreHandle {
    tx: mpsc::Sender<StoreCommand>,
}

impl StoreHandle {
    async fn send(&self, cmd: StoreCommand) -> Result<()> {
        self.tx
            .send(cmd)
            .await
            .map_err(|_| anyhow!("store writer has stopped"))
    }

    async fn ask<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> StoreCommand,
    ) -> Result<T> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.send(make(reply_tx)).await?;
        reply_rx
            .await
            .map_err(|_| anyhow!("store writer dropped the reply"))?
    }

    pub async fn persist_request(
        &self,
        record: RequestRecord,
        bodies: Vec<Body>,
        findings: Vec<Finding>,
    ) -> Result<()> {
        self.send(StoreCommand::PersistRequest {
            record: Box::new(record),
            bodies,
            findings,
        })
        .await
    }

    pub async fn persist_alert(&self, alert: Alert) -> Result<i64> {
        self.ask(|reply| StoreCommand::PersistAlert { alert, reply })
            .await
    }

    pub async fn ack_alert(&self, id: i64) -> Result<bool> {
        self.ask(|reply| StoreCommand::AckAlert { id, reply }).await
    }

    pub async fn ack_all_alerts(&self) -> Result<usize> {
        self.ask(|reply| StoreCommand::AckAllAlerts { reply }).await
    }

    pub async fn persist_hardware(&self, sample: HardwareSample) -> Result<()> {
        self.send(StoreCommand::PersistHardware(sample)).await
    }

    pub async fn upsert_models_seen(&self, models: Vec<ModelInfo>) -> Result<()> {
        self.send(StoreCommand::UpsertModelsSeen(models)).await
    }

    pub async fn start_session(&self, app_version: &str) -> Result<i64> {
        let app_version = app_version.to_string();
        self.ask(|reply| StoreCommand::StartSession { app_version, reply })
            .await
    }

    pub async fn end_session(&self, id: i64) -> Result<()> {
        self.ask(|reply| StoreCommand::EndSession { id, reply })
            .await
    }

    pub async fn delete_bodies(&self, request_id: Uuid) -> Result<usize> {
        self.ask(|reply| StoreCommand::DeleteBodies { request_id, reply })
            .await
    }

    pub async fn run_retention(
        &self,
        policy: RetentionPolicy,
        now: DateTime<Utc>,
    ) -> Result<RetentionReport> {
        self.ask(|reply| StoreCommand::RunRetention { policy, now, reply })
            .await
    }

    pub async fn checkpoint(&self) -> Result<()> {
        self.ask(|reply| StoreCommand::Checkpoint { reply }).await
    }
}

/// Starts the writer task over an opened, migrated connection.
pub fn spawn(conn: Connection, path: PathBuf) -> StoreHandle {
    let (tx, mut rx) = mpsc::channel::<StoreCommand>(CHANNEL_CAPACITY);
    tokio::task::spawn_blocking(move || {
        let mut conn = conn;
        info!(db = %path.display(), "store writer started");
        while let Some(first) = rx.blocking_recv() {
            let mut batch = VecDeque::with_capacity(BATCH_MAX);
            batch.push_back(first);
            while batch.len() < BATCH_MAX {
                match rx.try_recv() {
                    Ok(cmd) => batch.push_back(cmd),
                    Err(_) => break,
                }
            }
            apply_batch(&mut conn, batch);
        }
        info!("store writer stopped");
    });
    StoreHandle { tx }
}

type Reply = Box<dyn FnOnce(&Result<()>) + Send>;

fn apply_batch(conn: &mut Connection, mut queue: VecDeque<StoreCommand>) {
    while let Some(cmd) = queue.pop_front() {
        if cmd.is_standalone() {
            run_standalone(conn, cmd);
            continue;
        }
        let mut group = vec![cmd];
        while queue.front().is_some_and(|next| !next.is_standalone()) {
            if let Some(cmd) = queue.pop_front() {
                group.push(cmd);
            }
        }
        run_transaction(conn, group);
    }
}

fn run_transaction(conn: &mut Connection, group: Vec<StoreCommand>) {
    let mut replies: Vec<Reply> = Vec::new();
    let committed = (|| -> Result<()> {
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .context("begin write transaction")?;
        for cmd in group {
            apply_command(&tx, cmd, &mut replies);
        }
        tx.commit().context("commit write transaction")
    })();
    if let Err(err) = &committed {
        warn!(error = %format!("{err:#}"), "store write batch failed");
    }
    for reply in replies {
        reply(&committed);
    }
}

/// Runs one command inside the open transaction; a failing statement is logged and does not abort the
/// batch, since every command writes independent rows.
fn apply_command(tx: &Connection, cmd: StoreCommand, replies: &mut Vec<Reply>) {
    match cmd {
        StoreCommand::PersistRequest {
            record,
            bodies,
            findings,
        } => {
            if let Err(err) = persist_request(tx, &record, &bodies, &findings) {
                warn!(request = %record.id, error = %format!("{err:#}"), "failed to persist request");
            }
        }
        StoreCommand::PersistAlert { alert, reply } => {
            let result = rows::insert_alert(tx, &alert);
            replies.push(Box::new(move |commit| {
                let _ = reply.send(after_commit(commit, result));
            }));
        }
        StoreCommand::AckAlert { id, reply } => {
            let result = tx
                .execute(
                    "UPDATE alerts SET acknowledged_at = ?1 WHERE id = ?2 AND acknowledged_at IS NULL",
                    params![Utc::now().timestamp_millis(), id],
                )
                .map(|n| n > 0)
                .context("ack alert");
            replies.push(Box::new(move |commit| {
                let _ = reply.send(after_commit(commit, result));
            }));
        }
        StoreCommand::AckAllAlerts { reply } => {
            let result = tx
                .execute(
                    "UPDATE alerts SET acknowledged_at = ?1 WHERE acknowledged_at IS NULL",
                    params![Utc::now().timestamp_millis()],
                )
                .context("ack all alerts");
            replies.push(Box::new(move |commit| {
                let _ = reply.send(after_commit(commit, result));
            }));
        }
        StoreCommand::PersistHardware(sample) => {
            if let Err(err) = rows::insert_hardware(tx, &sample) {
                warn!(error = %format!("{err:#}"), "failed to persist hardware sample");
            }
        }
        StoreCommand::UpsertModelsSeen(models) => {
            let now = Utc::now();
            for model in &models {
                if let Err(err) = rows::upsert_model_seen(tx, model, now) {
                    warn!(model = %model.id, error = %format!("{err:#}"), "failed to upsert model");
                }
            }
        }
        StoreCommand::StartSession { app_version, reply } => {
            let result = tx
                .execute(
                    "INSERT INTO sessions (started_at, app_version) VALUES (?1, ?2)",
                    params![Utc::now().timestamp_millis(), app_version],
                )
                .map(|_| tx.last_insert_rowid())
                .context("insert session");
            replies.push(Box::new(move |commit| {
                let _ = reply.send(after_commit(commit, result));
            }));
        }
        StoreCommand::EndSession { id, reply } => {
            let result = tx
                .execute(
                    "UPDATE sessions SET ended_at = ?1 WHERE id = ?2 AND ended_at IS NULL",
                    params![Utc::now().timestamp_millis(), id],
                )
                .map(|updated| {
                    if updated == 0 {
                        warn!(session_id = id, "no open session row to close");
                    }
                })
                .context("end session");
            replies.push(Box::new(move |commit| {
                let _ = reply.send(after_commit(commit, result));
            }));
        }
        StoreCommand::DeleteBodies { request_id, reply } => {
            let result = tx
                .execute(
                    "DELETE FROM bodies WHERE request_id = ?1",
                    params![request_id.to_string()],
                )
                .context("delete bodies");
            replies.push(Box::new(move |commit| {
                let _ = reply.send(after_commit(commit, result));
            }));
        }
        StoreCommand::RunRetention { .. } | StoreCommand::Checkpoint { .. } => {
            debug!("standalone command reached the batch path; ignored");
        }
    }
}

fn after_commit<T>(commit: &Result<()>, result: Result<T>) -> Result<T> {
    match commit {
        Ok(()) => result,
        Err(err) => Err(anyhow!("{err:#}")),
    }
}

fn persist_request(
    tx: &Connection,
    record: &RequestRecord,
    bodies: &[Body],
    findings: &[Finding],
) -> Result<()> {
    rows::insert_request(tx, record)?;
    for body in bodies {
        rows::insert_body(tx, body, record.ended_at)?;
    }
    for finding in findings {
        rows::insert_finding(tx, finding)?;
    }
    rows::upsert_request_rollup(tx, record)?;
    Ok(())
}

fn run_standalone(conn: &mut Connection, cmd: StoreCommand) {
    match cmd {
        StoreCommand::RunRetention { policy, now, reply } => {
            let result = retention::run(conn, &policy, now);
            if let Err(err) = &result {
                warn!(error = %format!("{err:#}"), "retention failed");
            }
            let _ = reply.send(result);
        }
        StoreCommand::Checkpoint { reply } => {
            let result = conn
                .execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);")
                .context("checkpoint");
            let _ = reply.send(result);
        }
        _ => {}
    }
}
