//! SQLite-backed messages. Each message keeps its MIME and a JSON record of how it arrived;
//! recipients get their own table so filtering by address is an index lookup. Message ids come
//! from AUTOINCREMENT, so they only ever grow and work as cursors even after deletes.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use tokio::sync::watch;

use crate::message::{Meta, NewMessage, Provider, Stored, Summary};

pub struct Store {
    conn: Mutex<Connection>,
    max_messages: usize,
    /// The newest message id, so `wait` can sleep until something arrives.
    latest: watch::Sender<i64>,
}

/// Which messages to list, wait for or delete. Strings match case-insensitively.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Any recipient: To, Cc or Bcc.
    pub to: Option<String>,
    pub from: Option<String>,
    /// The whole subject.
    pub subject: Option<String>,
    pub subject_contains: Option<String>,
    pub provider: Option<Provider>,
    /// Free text over subject, sender and recipients (the inbox search box).
    pub q: Option<String>,
    /// Only messages with a greater id.
    pub after: Option<i64>,
}

fn log<T>(result: rusqlite::Result<T>, op: &str) -> Option<T> {
    result.map_err(|e| tracing::error!("sqlite {op}: {e}")).ok()
}

fn like(s: &str) -> String {
    format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"))
}

impl Filter {
    /// A WHERE clause (on `messages m`) and its parameters.
    fn sql(&self) -> (String, Vec<SqlValue>) {
        let mut clauses = vec!["1 = 1".to_string()];
        let mut args: Vec<SqlValue> = vec![];
        if let Some(to) = &self.to {
            clauses.push("EXISTS (SELECT 1 FROM recipients r WHERE r.message_id = m.id AND r.address = ?)".into());
            args.push(to.trim().to_ascii_lowercase().into());
        }
        if let Some(from) = &self.from {
            clauses.push("m.mail_from = ?".into());
            args.push(from.trim().to_ascii_lowercase().into());
        }
        if let Some(subject) = &self.subject {
            clauses.push("m.subject = ? COLLATE NOCASE".into());
            args.push(subject.clone().into());
        }
        if let Some(s) = &self.subject_contains {
            clauses.push("m.subject LIKE ? ESCAPE '\\'".into());
            args.push(like(s).into());
        }
        if let Some(p) = self.provider {
            clauses.push("m.provider = ?".into());
            args.push(p.id().to_string().into());
        }
        if let Some(q) = self.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
            clauses.push(
                "(m.subject LIKE ? ESCAPE '\\' OR m.mail_from LIKE ? ESCAPE '\\' OR EXISTS (SELECT 1 FROM recipients r WHERE r.message_id = m.id AND r.address LIKE ? ESCAPE '\\'))"
                    .into(),
            );
            let pattern = like(q);
            args.extend([pattern.clone().into(), pattern.clone().into(), pattern.into()]);
        }
        if let Some(after) = self.after {
            clauses.push("m.id > ?".into());
            args.push(after.into());
        }
        (clauses.join(" AND "), args)
    }
}

const SUMMARY_COLUMNS: &str =
    "m.id, m.received_at, m.provider, m.endpoint, m.provider_message_id, m.subject, m.size, m.read, m.meta";

fn summary_row(r: &rusqlite::Row) -> rusqlite::Result<(Summary, Meta)> {
    let meta: Meta = serde_json::from_str(&r.get::<_, String>(8)?).unwrap_or_default();
    let received_at: i64 = r.get(1)?;
    let summary = Summary {
        id: r.get(0)?,
        received_at,
        date: crate::util::iso8601(received_at / 1000),
        provider: Provider::parse(&r.get::<_, String>(2)?).unwrap_or(Provider::Smtp),
        endpoint: r.get(3)?,
        provider_message_id: r.get(4)?,
        from: meta.from.clone(),
        to: meta.to.clone(),
        cc: meta.cc.clone(),
        bcc: meta.bcc.clone(),
        subject: r.get(5)?,
        preview: meta.preview.clone(),
        size: r.get::<_, i64>(6)? as usize,
        has_attachments: meta.has_attachments,
        read: r.get(7)?,
        scenario: meta.scenario.clone(),
        template: meta.template.clone(),
        tags: meta.tags.clone(),
    };
    Ok((summary, meta))
}

impl Store {
    /// Open (creating if needed) the database at `path`, or an in-memory database when `None`.
    pub fn open(path: Option<&Path>, max_messages: usize) -> anyhow::Result<Self> {
        let conn = match path {
            Some(p) => {
                if let Some(dir) = p.parent() {
                    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
                }
                let conn = Connection::open(p).with_context(|| format!("opening {}", p.display()))?;
                // WAL lets readers and the writer work concurrently.
                conn.pragma_update(None, "journal_mode", "WAL")?;
                conn
            }
            None => Connection::open_in_memory()?,
        };
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                received_at         INTEGER NOT NULL,
                provider            TEXT NOT NULL,
                endpoint            TEXT NOT NULL,
                provider_message_id TEXT NOT NULL,
                mail_from           TEXT NOT NULL,
                subject             TEXT NOT NULL,
                size                INTEGER NOT NULL,
                read                INTEGER NOT NULL DEFAULT 0,
                meta                TEXT NOT NULL,
                raw                 BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS recipients (
                message_id INTEGER NOT NULL REFERENCES messages (id) ON DELETE CASCADE,
                address    TEXT NOT NULL,
                kind       TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS recipients_address ON recipients (address, message_id);
            CREATE INDEX IF NOT EXISTS recipients_message ON recipients (message_id);",
        )?;
        let latest: i64 = conn
            .query_row("SELECT seq FROM sqlite_sequence WHERE name = 'messages'", [], |r| r.get(0))
            .optional()?
            .unwrap_or(0);
        Ok(Self { conn: Mutex::new(conn), max_messages, latest: watch::channel(latest).0 })
    }

    /// The newest message id ever stored (0 if none). Deleting messages doesn't lower it.
    pub fn cursor(&self) -> i64 {
        *self.latest.borrow()
    }

    /// Wakes up whenever a message is stored.
    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.latest.subscribe()
    }

    /// Store a message and return its id.
    pub fn insert(&self, msg: &NewMessage) -> Option<i64> {
        let meta = serde_json::to_string(&msg.meta).ok()?;
        let mail_from = msg.meta.from.as_ref().map(|a| a.key()).unwrap_or_default();
        let id = {
            let mut conn = self.conn.lock().unwrap();
            let tx = log(conn.transaction(), "begin")?;
            log(
                tx.execute(
                    "INSERT INTO messages (received_at, provider, endpoint, provider_message_id, mail_from, subject, size, meta, raw)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        msg.received_at,
                        msg.provider.id(),
                        msg.endpoint,
                        msg.provider_message_id,
                        mail_from,
                        msg.subject,
                        msg.raw.len() as i64,
                        meta,
                        msg.raw
                    ],
                ),
                "insert message",
            )?;
            let id = tx.last_insert_rowid();
            let kinds = [("to", &msg.meta.to), ("cc", &msg.meta.cc), ("bcc", &msg.meta.bcc)];
            for (kind, list) in kinds {
                for a in list.iter() {
                    log(
                        tx.execute(
                            "INSERT INTO recipients (message_id, address, kind) VALUES (?1, ?2, ?3)",
                            params![id, a.key(), kind],
                        ),
                        "insert recipient",
                    )?;
                }
            }
            // Keep the newest `max_messages`.
            log(
                tx.execute(
                    "DELETE FROM messages WHERE id <= (SELECT id FROM messages ORDER BY id DESC LIMIT 1 OFFSET ?1)",
                    params![self.max_messages as i64],
                ),
                "prune",
            )?;
            log(tx.commit(), "commit")?;
            id
        };
        self.latest.send_replace(id);
        Some(id)
    }

    /// Matching messages, newest first.
    pub fn list(&self, filter: &Filter, limit: usize, offset: usize) -> Vec<Summary> {
        let (clause, mut args) = filter.sql();
        args.extend([SqlValue::from(limit as i64), SqlValue::from(offset as i64)]);
        let conn = self.conn.lock().unwrap();
        let rows = conn
            .prepare(&format!(
                "SELECT {SUMMARY_COLUMNS} FROM messages m WHERE {clause} ORDER BY m.id DESC LIMIT ? OFFSET ?"
            ))
            .and_then(|mut stmt| {
                stmt.query_map(params_from_iter(args), summary_row)?.map(|r| r.map(|(s, _)| s)).collect()
            });
        log(rows, "list").unwrap_or_default()
    }

    pub fn count(&self, filter: &Filter) -> usize {
        let (clause, args) = filter.sql();
        let conn = self.conn.lock().unwrap();
        log(
            conn.query_row(&format!("SELECT COUNT(*) FROM messages m WHERE {clause}"), params_from_iter(args), |r| {
                r.get::<_, i64>(0)
            }),
            "count",
        )
        .unwrap_or(0) as usize
    }

    pub fn unread(&self) -> usize {
        let conn = self.conn.lock().unwrap();
        log(conn.query_row("SELECT COUNT(*) FROM messages WHERE read = 0", [], |r| r.get::<_, i64>(0)), "count")
            .unwrap_or(0) as usize
    }

    /// The oldest matching message (the first to arrive after the filter's cursor).
    pub fn first(&self, filter: &Filter) -> Option<Stored> {
        let (clause, args) = filter.sql();
        let id: i64 = {
            let conn = self.conn.lock().unwrap();
            log(
                conn.query_row(
                    &format!("SELECT m.id FROM messages m WHERE {clause} ORDER BY m.id ASC LIMIT 1"),
                    params_from_iter(args),
                    |r| r.get(0),
                )
                .optional(),
                "first",
            )??
        };
        self.get(id)
    }

    pub fn get(&self, id: i64) -> Option<Stored> {
        let conn = self.conn.lock().unwrap();
        log(
            conn.query_row(&format!("SELECT {SUMMARY_COLUMNS}, m.raw FROM messages m WHERE m.id = ?1"), [id], |r| {
                let (summary, meta) = summary_row(r)?;
                Ok(Stored { summary, raw: r.get(9)?, meta })
            })
            .optional(),
            "get",
        )
        .flatten()
    }

    pub fn set_read(&self, id: Option<i64>, read: bool) -> usize {
        let conn = self.conn.lock().unwrap();
        let result = match id {
            Some(id) => conn.execute("UPDATE messages SET read = ?1 WHERE id = ?2", params![read, id]),
            None => conn.execute("UPDATE messages SET read = ?1", params![read]),
        };
        log(result, "update").unwrap_or(0)
    }

    pub fn delete(&self, id: i64) -> bool {
        let conn = self.conn.lock().unwrap();
        log(conn.execute("DELETE FROM messages WHERE id = ?1", [id]), "delete").is_some_and(|n| n > 0)
    }

    /// Delete every matching message; returns how many.
    pub fn delete_matching(&self, filter: &Filter) -> usize {
        let (clause, args) = filter.sql();
        let conn = self.conn.lock().unwrap();
        log(conn.execute(&format!("DELETE FROM messages AS m WHERE {clause}"), params_from_iter(args)), "delete")
            .unwrap_or(0)
    }
}
