//! The import queue, alert state and bookkeeping in one SQLite file — the
//! same schema the Worker shell keeps in its Durable Object.

use std::collections::BTreeMap;
use std::sync::Mutex;

use genesis_rules::Address;
use genesis_rules::import::{Lane, Task};
use rusqlite::{Connection, OptionalExtension, params};

pub struct Store {
    conn: Mutex<Connection>,
}

type R<T> = rusqlite::Result<T>;

impl Store {
    pub fn open(path: &str) -> R<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS tasks (
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
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value INTEGER NOT NULL);",
        )?;
        // Tables made before the owner lane existed lack its column.
        let has: i64 =
            conn.query_row("SELECT count(*) FROM pragma_table_info('tasks') WHERE name = 'proved_by'", [], |r| r.get(0))?;
        if has == 0 {
            conn.execute("ALTER TABLE tasks ADD COLUMN proved_by TEXT", [])?;
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> R<T>) -> R<T> {
        f(&self.conn.lock().expect("store lock"))
    }

    pub fn save(&self, task: &Task) -> R<()> {
        self.with(|c| save_with(c, task))
    }

    /// Queue `task` unless an ACTIVE task for the same Safe exists (then
    /// return that one) — one locked step, so concurrent imports of one Safe
    /// cannot both queue. A done or failed task is history and is replaced.
    pub fn enqueue(&self, task: &Task) -> R<Task> {
        self.with(|c| {
            let existing: Option<String> = c
                .query_row(
                    "SELECT payload FROM tasks WHERE safe = ?1 AND status IN ('queued', 'submitted') ORDER BY updated DESC LIMIT 1",
                    params![task.safe.to_string().to_lowercase()],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(t) = existing.and_then(|p| serde_json::from_str(&p).ok()) {
                return Ok(t);
            }
            save_with(c, task)?;
            Ok(task.clone())
        })
    }

    /// Tasks created since `ms` (every task is a sponsored write) in one
    /// lane, optionally for one owner.
    pub fn sponsored_since(&self, ms: i64, lane: Lane, owner: Option<Address>) -> R<u32> {
        let lane = match lane {
            Lane::Deployed => "proved_by IS NULL",
            Lane::Owner => "proved_by IS NOT NULL",
        };
        let owner = owner.map(|o| o.to_string().to_lowercase());
        let sql = format!("SELECT count(*) FROM tasks WHERE created >= ?1 AND {lane} AND (?2 IS NULL OR proved_by = ?2)");
        self.with(|c| c.query_row(&sql, params![ms, owner], |r| r.get::<_, i64>(0))).map(|n| n as u32)
    }

    fn tasks(&self, sql: &str, args: impl rusqlite::Params) -> R<Vec<Task>> {
        self.with(|c| {
            let mut st = c.prepare(sql)?;
            let rows = st.query_map(args, |r| r.get::<_, String>(0))?;
            // An unreadable payload reads as missing rather than jamming the queue.
            Ok(rows.filter_map(|r| r.ok()).filter_map(|p| serde_json::from_str(&p).ok()).collect())
        })
    }

    pub fn by_id(&self, id: &str) -> R<Option<Task>> {
        Ok(self.tasks("SELECT payload FROM tasks WHERE id = ?1", params![id])?.into_iter().next())
    }

    pub fn active(&self, limit: u32) -> R<Vec<Task>> {
        self.tasks(
            "SELECT payload FROM tasks WHERE status IN ('queued', 'submitted') ORDER BY updated ASC LIMIT ?1",
            params![limit],
        )
    }

    pub fn counts(&self) -> R<BTreeMap<String, i64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT status, count(*) FROM tasks GROUP BY status")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            rows.collect()
        })
    }

    pub fn failed_since(&self, ms: i64) -> R<u64> {
        self.with(|c| {
            c.query_row("SELECT count(*) FROM tasks WHERE status = 'failed' AND updated > ?1", params![ms], |r| {
                r.get::<_, i64>(0)
            })
            .map(|n| n as u64)
        })
    }

    pub fn alerts(&self) -> R<BTreeMap<String, u64>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT key, sent_ms FROM alerts")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?;
            rows.collect()
        })
    }

    pub fn replace_alerts(&self, sent: &BTreeMap<String, u64>) -> R<()> {
        self.with(|c| {
            c.execute("DELETE FROM alerts", [])?;
            for (k, t) in sent {
                c.execute("INSERT INTO alerts (key, sent_ms) VALUES (?1, ?2)", params![k, *t as i64])?;
            }
            Ok(())
        })
    }

    pub fn meta(&self, key: &str) -> R<Option<i64>> {
        self.with(|c| c.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional())
    }

    pub fn set_meta(&self, key: &str, value: i64) -> R<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, value],
            )
            .map(|_| ())
        })
    }
}

fn save_with(c: &Connection, task: &Task) -> R<()> {
    let payload = serde_json::to_string(task).expect("task serializes");
    let status = serde_json::to_value(task.status).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
    c.execute(
        "INSERT INTO tasks (id, safe, status, created, updated, payload, proved_by) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(id) DO UPDATE SET status = excluded.status, updated = excluded.updated, payload = excluded.payload",
        params![
            task.id,
            task.safe.to_string().to_lowercase(),
            status,
            task.created_ms as i64,
            task.updated_ms as i64,
            payload,
            task.proved_by.map(|o| o.to_string().to_lowercase()),
        ],
    )
    .map(|_| ())
}
