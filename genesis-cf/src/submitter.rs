//! The sponsored-import queue: one Durable Object, tasks in its SQLite,
//! driven by its alarm. Each pass runs the shared `submit::step` on the
//! least recently touched active tasks.

use std::time::Duration;

use genesis_rules::import::{Status, Task};
use genesis_rules::proposal;
use genesis_rules::sentinel::{self, Facts};
use serde::Deserialize;
use serde_json::json;
// `wasm_bindgen` must be resolvable here: the #[durable_object] expansion names it.
use worker::{DurableObject, Env, Request, Response, Result, SqlStorage, SqlStorageValue, State, durable_object, wasm_bindgen};

use crate::config::Config;
use crate::edge::now_ms;
use crate::rpc::Rpc;
use crate::signer;
use crate::submit;
use crate::telegram::Telegram;

const PASS_SIZE: u32 = 10;
const NEXT_PASS: Duration = Duration::from_secs(15);

#[durable_object]
pub struct SubmitterDo {
    state: State,
    env: Env,
}

#[derive(Deserialize)]
struct PayloadRow {
    payload: String,
}

#[derive(Deserialize)]
struct CountRow {
    status: String,
    n: i64,
}

#[derive(Deserialize)]
struct NumberRow {
    n: Option<i64>,
}

#[derive(Deserialize)]
struct AlertRow {
    key: String,
    sent_ms: i64,
}

impl DurableObject for SubmitterDo {
    fn new(state: State, env: Env) -> Self {
        Self { state, env }
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        self.bootstrap()?;
        let url = req.url()?;
        let param = |k: &str| url.query_pairs().find(|(q, _)| q == k).map(|(_, v)| v.into_owned()).unwrap_or_default();
        match url.path() {
            "/enqueue" => {
                let task: Task = req.json().await?;
                // The edge enqueues only what the home chain does not hold, so
                // only an ACTIVE task for the Safe is still current; a done or
                // failed one is history (the chain is the judge) and is replaced.
                let stored = match self.by_safe(&task.safe.to_string())? {
                    Some(existing) if matches!(existing.status, Status::Queued | Status::Submitted) => existing,
                    _ => {
                        self.save(&task)?;
                        task
                    }
                };
                self.ensure_alarm().await?;
                Response::from_json(&stored)
            }
            "/task" => match self.by_id(&param("id"))? {
                Some(t) => Response::from_json(&t),
                None => Ok(Response::from_json(&json!({"error": "unknown task"}))?.with_status(404)),
            },
            "/sponsored" => {
                // Tasks created since `since` (every task is a sponsored write)
                // in one lane, optionally for one owner.
                let since: i64 = param("since").parse().unwrap_or(0);
                let lane = if param("lane") == "owner" { "proved_by IS NOT NULL" } else { "proved_by IS NULL" };
                let mut sql = format!("SELECT count(*) AS n FROM tasks WHERE created >= ?1 AND {lane}");
                let mut args = vec![SqlStorageValue::from(since)];
                let owner = param("owner").to_lowercase();
                if !owner.is_empty() {
                    sql.push_str(" AND proved_by = ?2");
                    args.push(SqlStorageValue::from(owner));
                }
                let rows: Vec<NumberRow> = self.sql().exec(&sql, args)?.to_array()?;
                Response::from_json(&json!({"count": rows.first().and_then(|r| r.n).unwrap_or(0)}))
            }
            "/stats" => {
                let rows: Vec<CountRow> =
                    self.sql().exec("SELECT status, count(*) AS n FROM tasks GROUP BY status", None)?.to_array()?;
                let mut stats = serde_json::Map::new();
                for r in rows {
                    stats.insert(r.status, json!(r.n));
                }
                stats.insert("alarmArmed".into(), json!(self.state.storage().get_alarm().await?.is_some()));
                stats.insert("alerts".into(), json!(sentinel::holding(&self.alerts()?)));
                Response::from_json(&stats)
            }
            "/maintain" => Response::from_json(&self.maintain().await?),
            "/proposal/merge" => {
                let r: crate::edge::ProposeRequest = req.json().await?;
                let hash = r.safe_tx.hash(r.chain_id, r.safe);
                if let Some(g) = &r.genesis {
                    // The edge kept it only if it derives the Safe's address.
                    self.sql().exec(
                        "INSERT INTO mailbox_geneses (safe, payload) VALUES (?1, ?2) ON CONFLICT(safe) DO NOTHING",
                        vec![
                            SqlStorageValue::from(r.safe.to_string().to_lowercase()),
                            SqlStorageValue::from(serde_json::to_string(g).map_err(|e| worker::Error::RustError(e.to_string()))?),
                        ],
                    )?;
                }
                let existing = self.proposal(r.chain_id, &hash.to_string())?;
                let answer = match proposal::merge(existing, r.chain_id, r.safe, r.safe_tx, r.approval, now_ms()) {
                    Ok((p, outcome)) => {
                        if outcome != proposal::Outcome::Unchanged {
                            self.save_proposal(&p)?;
                        }
                        crate::edge::MergeAnswer { proposal: Some(p), outcome: Some(outcome), refusal: None }
                    }
                    Err(refusal) => crate::edge::MergeAnswer { refusal: Some(refusal), ..Default::default() },
                };
                Response::from_json(&answer)
            }
            "/proposal/genesis" => {
                let rows: Vec<PayloadRow> = self
                    .sql()
                    .exec("SELECT payload FROM mailbox_geneses WHERE safe = ?1", vec![SqlStorageValue::from(param("safe").to_lowercase())])?
                    .to_array()?;
                let g: Option<genesis_rules::genesis::Genesis> = rows.into_iter().next().and_then(|r| serde_json::from_str(&r.payload).ok());
                Response::from_json(&g)
            }
            "/proposal/get" => {
                let chain_id: u64 = param("chainId").parse().unwrap_or(0);
                Response::from_json(&self.proposal(chain_id, &param("hash").to_lowercase())?)
            }
            "/proposal/list" => {
                let chain_id: u64 = param("chainId").parse().unwrap_or(0);
                let since = now_ms().saturating_sub(proposal::KEEP_MS) as i64;
                let rows: Vec<PayloadRow> = self
                    .sql()
                    .exec(
                        "SELECT payload FROM proposals WHERE chain_id = ?1 AND safe = ?2 AND updated >= ?3
                         ORDER BY nonce ASC, created ASC LIMIT 100",
                        vec![
                            SqlStorageValue::from(chain_id as i64),
                            SqlStorageValue::from(param("safe").to_lowercase()),
                            SqlStorageValue::from(since),
                        ],
                    )?
                    .to_array()?;
                let list: Vec<proposal::Proposal> = rows.into_iter().filter_map(|r| serde_json::from_str(&r.payload).ok()).collect();
                Response::from_json(&list)
            }
            _ => Ok(Response::from_json(&json!({"error": "not found"}))?.with_status(404)),
        }
    }

    async fn alarm(&self) -> Result<Response> {
        self.bootstrap()?;
        let config = Config::from_env(&self.env)?;
        let Some(key) = config.private_key.clone() else {
            return Response::ok("idle: no sponsor key");
        };
        let rpc = Rpc::new(config.home());
        for mut task in self.active(PASS_SIZE)? {
            let save = |t: &Task| self.save(t).map_err(|e| e.to_string());
            if let Err(e) = submit::step(&config, &rpc, &key, &mut task, now_ms(), &save).await {
                worker::console_warn!("task {}: {e}", task.id);
            }
        }
        if !self.active(1)?.is_empty() {
            self.state.storage().set_alarm(NEXT_PASS).await?;
        }
        Response::ok("pass done")
    }
}

impl SubmitterDo {
    /// The cron tick: evaluate and deliver alerts, then re-arm a lost alarm.
    async fn maintain(&self) -> Result<Vec<sentinel::Notice>> {
        let config = Config::from_env(&self.env)?;
        let active = self.active(1000)?;
        let now = now_ms();
        let last_tick = self.meta("lastTickMs")?.unwrap_or(now as i64);
        let newly_failed: Vec<NumberRow> = self
            .sql()
            .exec("SELECT count(*) AS n FROM tasks WHERE status = 'failed' AND updated > ?1", vec![SqlStorageValue::from(last_tick)])?
            .to_array()?;
        let balance = match config.private_key.as_deref().map(signer::address) {
            Some(Ok(sponsor)) => Some(Rpc::new(config.home()).balance(sponsor).await.map_err(|e| e.to_string())),
            Some(Err(e)) => Some(Err(e)),
            None => None,
        };
        // Measured BEFORE re-arming: an alarm lost while work waits is
        // exactly what this alert exists to report.
        let alarm_armed = self.state.storage().get_alarm().await?.is_some();
        let facts = Facts {
            now_ms: now,
            sponsor_balance_wei: balance,
            min_balance_wei: config.min_balance_wei,
            active_tasks: active.len() as u64,
            oldest_active_updated_ms: active.iter().map(|t| t.updated_ms).min(),
            newly_failed: newly_failed.first().and_then(|r| r.n).unwrap_or(0) as u64,
            alarm_armed: alarm_armed || config.private_key.is_none(),
        };
        let (notices, state) = sentinel::due(&sentinel::evaluate(&facts), &self.alerts()?, now);
        if let Some(telegram) = Telegram::new(config.telegram_bot_token.clone(), config.telegram_chat_id.clone()) {
            for n in &notices {
                telegram.send(&sentinel::message(n, "safe-genesis-registry", config.home_chain_id)).await;
            }
        }
        self.sql().exec("DELETE FROM alerts", None)?;
        for (key, sent_ms) in state {
            self.sql().exec(
                "INSERT INTO alerts (key, sent_ms) VALUES (?1, ?2)",
                vec![SqlStorageValue::from(key), SqlStorageValue::from(sent_ms as i64)],
            )?;
        }
        self.set_meta("lastTickMs", now as i64)?;
        if !active.is_empty() {
            self.ensure_alarm().await?;
        }
        Ok(notices)
    }

    fn alerts(&self) -> Result<std::collections::BTreeMap<String, u64>> {
        let rows: Vec<AlertRow> = self.sql().exec("SELECT key, sent_ms FROM alerts", None)?.to_array()?;
        Ok(rows.into_iter().map(|r| (r.key, r.sent_ms as u64)).collect())
    }

    fn meta(&self, key: &str) -> Result<Option<i64>> {
        let rows: Vec<NumberRow> = self.sql().exec("SELECT value AS n FROM meta WHERE key = ?1", vec![key.into()])?.to_array()?;
        Ok(rows.first().and_then(|r| r.n))
    }

    fn set_meta(&self, key: &str, value: i64) -> Result<()> {
        self.sql().exec(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            vec![SqlStorageValue::from(key), SqlStorageValue::from(value)],
        )?;
        Ok(())
    }

    // -- store ------------------------------------------------------------------

    fn sql(&self) -> SqlStorage {
        self.state.storage().sql()
    }

    fn bootstrap(&self) -> Result<()> {
        self.sql().exec(
            "CREATE TABLE IF NOT EXISTS tasks (
                 id      TEXT PRIMARY KEY,
                 safe    TEXT NOT NULL,
                 status  TEXT NOT NULL,
                 created INTEGER NOT NULL,
                 updated INTEGER NOT NULL,
                 payload TEXT NOT NULL,
                 proved_by TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_tasks_safe ON tasks (safe);
             CREATE INDEX IF NOT EXISTS idx_tasks_active ON tasks (status, updated);
             CREATE INDEX IF NOT EXISTS idx_tasks_created ON tasks (created);
             CREATE TABLE IF NOT EXISTS alerts (key TEXT PRIMARY KEY, sent_ms INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS proposals (
                 chain_id INTEGER NOT NULL,
                 hash     TEXT NOT NULL,
                 safe     TEXT NOT NULL,
                 nonce    INTEGER NOT NULL,
                 created  INTEGER NOT NULL,
                 updated  INTEGER NOT NULL,
                 payload  TEXT NOT NULL,
                 PRIMARY KEY (chain_id, hash)
             );
             CREATE INDEX IF NOT EXISTS idx_proposals_safe ON proposals (chain_id, safe, nonce);
             CREATE TABLE IF NOT EXISTS mailbox_geneses (safe TEXT PRIMARY KEY, payload TEXT NOT NULL);",
            None,
        )?;
        // Tables made before the owner lane existed lack its column.
        let has: Vec<NumberRow> = self
            .sql()
            .exec("SELECT count(*) AS n FROM pragma_table_info('tasks') WHERE name = 'proved_by'", None)?
            .to_array()?;
        if has.first().and_then(|r| r.n) == Some(0) {
            self.sql().exec("ALTER TABLE tasks ADD COLUMN proved_by TEXT", None)?;
        }
        Ok(())
    }

    fn save(&self, task: &Task) -> Result<()> {
        let payload = serde_json::to_string(task).map_err(|e| worker::Error::RustError(e.to_string()))?;
        let status = serde_json::to_value(task.status).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
        self.sql().exec(
            "INSERT INTO tasks (id, safe, status, created, updated, payload, proved_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET status = excluded.status, updated = excluded.updated, payload = excluded.payload",
            vec![
                SqlStorageValue::from(task.id.as_str()),
                SqlStorageValue::from(task.safe.to_string().to_lowercase()),
                SqlStorageValue::from(status),
                SqlStorageValue::from(task.created_ms as i64),
                SqlStorageValue::from(task.updated_ms as i64),
                SqlStorageValue::from(payload),
                task.proved_by.map(|o| SqlStorageValue::from(o.to_string().to_lowercase())).unwrap_or(SqlStorageValue::Null),
            ],
        )?;
        Ok(())
    }

    fn proposal(&self, chain_id: u64, hash: &str) -> Result<Option<proposal::Proposal>> {
        let rows: Vec<PayloadRow> = self
            .sql()
            .exec(
                "SELECT payload FROM proposals WHERE chain_id = ?1 AND hash = ?2",
                vec![SqlStorageValue::from(chain_id as i64), SqlStorageValue::from(hash.to_lowercase())],
            )?
            .to_array()?;
        Ok(rows.into_iter().next().and_then(|r| serde_json::from_str(&r.payload).ok()))
    }

    fn save_proposal(&self, p: &proposal::Proposal) -> Result<()> {
        let payload = serde_json::to_string(p).map_err(|e| worker::Error::RustError(e.to_string()))?;
        self.sql().exec(
            "INSERT INTO proposals (chain_id, hash, safe, nonce, created, updated, payload) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(chain_id, hash) DO UPDATE SET updated = excluded.updated, payload = excluded.payload",
            vec![
                SqlStorageValue::from(p.chain_id as i64),
                SqlStorageValue::from(p.safe_tx_hash.to_string().to_lowercase()),
                SqlStorageValue::from(p.safe.to_string().to_lowercase()),
                SqlStorageValue::from(proposal::nonce_key(p.safe_tx.nonce)),
                SqlStorageValue::from(p.created_ms as i64),
                SqlStorageValue::from(p.updated_ms as i64),
                SqlStorageValue::from(payload),
            ],
        )?;
        Ok(())
    }

    fn query(&self, sql: &str, args: Vec<SqlStorageValue>) -> Result<Vec<Task>> {
        let rows: Vec<PayloadRow> = self.sql().exec(sql, args)?.to_array()?;
        // An unreadable payload reads as missing rather than jamming the queue.
        Ok(rows.into_iter().filter_map(|r| serde_json::from_str(&r.payload).ok()).collect())
    }

    fn by_id(&self, id: &str) -> Result<Option<Task>> {
        Ok(self.query("SELECT payload FROM tasks WHERE id = ?1", vec![id.into()])?.into_iter().next())
    }

    fn by_safe(&self, safe: &str) -> Result<Option<Task>> {
        Ok(self
            .query("SELECT payload FROM tasks WHERE safe = ?1 ORDER BY updated DESC LIMIT 1", vec![safe.to_lowercase().into()])?
            .into_iter()
            .next())
    }

    fn active(&self, limit: u32) -> Result<Vec<Task>> {
        self.query(
            "SELECT payload FROM tasks WHERE status IN ('queued', 'submitted') ORDER BY updated ASC LIMIT ?1",
            vec![SqlStorageValue::from(limit as i64)],
        )
    }

    async fn ensure_alarm(&self) -> Result<()> {
        if self.state.storage().get_alarm().await?.is_none() {
            self.state.storage().set_alarm(Duration::from_millis(100)).await?;
        }
        Ok(())
    }
}
