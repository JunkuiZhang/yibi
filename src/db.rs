use crate::ledger::{DEFAULT_START, Entry};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

pub type DbResult<T> = Result<T, String>;

#[derive(Clone)]
pub struct Database(pub Arc<Mutex<Connection>>);

impl Database {
    pub fn open(path: &Path) -> DbResult<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path).map_err(|e| e.to_string())?;
        Self::from_connection(Connection::open(path).map_err(|e| e.to_string())?)
    }

    pub fn from_connection(conn: Connection) -> DbResult<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if version > 1 {
            return Err("数据库版本较新，请使用新版“一笔”".into());
        }
        conn.execute_batch(include_str!("schema.sql"))
            .map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT OR IGNORE INTO settings (key,value) VALUES ('ledger_start',?1)",
            [DEFAULT_START],
        )
        .map_err(|e| e.to_string())?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    pub async fn run<T, F>(&self, operation: F) -> DbResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> DbResult<T> + Send + 'static,
    {
        let db = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = db.0.lock().map_err(|_| "数据库锁异常")?;
            operation(&mut conn)
        })
        .await
        .map_err(|_| "数据库任务异常")?
    }
}

pub fn insert(conn: &Connection, entry: &Entry) -> DbResult<Option<i64>> {
    entry.validate()?;
    let payload = serde_json::to_string(entry).map_err(|e| e.to_string())?;
    let changed = conn.execute(
        "INSERT INTO entries (date,kind,amount_cents,source,external_id,payload) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(source,external_id) DO NOTHING",
        params![entry.date, entry.kind.as_str(), entry.amount_cents, entry.source, entry.external_id, payload],
    ).map_err(|e| e.to_string())?;
    Ok((changed > 0).then(|| conn.last_insert_rowid()))
}

pub fn entries(conn: &Connection) -> DbResult<Vec<Entry>> {
    let mut stmt = conn
        .prepare("SELECT id,payload FROM entries ORDER BY date DESC,id DESC")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    rows.map(|row| {
        let (id, text) = row.map_err(|e| e.to_string())?;
        let mut entry: Entry = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        entry.id = id;
        Ok(entry)
    })
    .collect()
}

pub fn entry(conn: &Connection, id: i64) -> DbResult<Option<Entry>> {
    let text: Option<String> = conn
        .query_row("SELECT payload FROM entries WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()
        .map_err(|e| e.to_string())?;
    text.map(|t| {
        let mut e: Entry = serde_json::from_str(&t).map_err(|e| e.to_string())?;
        e.id = id;
        Ok(e)
    })
    .transpose()
}

pub fn update(conn: &Connection, entry: &Entry) -> DbResult<()> {
    entry.validate()?;
    let changed = conn
        .execute(
            "UPDATE entries SET date=?1,kind=?2,amount_cents=?3,payload=?4 WHERE id=?5",
            params![
                entry.date,
                entry.kind.as_str(),
                entry.amount_cents,
                serde_json::to_string(entry).map_err(|e| e.to_string())?,
                entry.id
            ],
        )
        .map_err(|e| e.to_string())?;
    if changed != 1 {
        return Err("记录不存在".into());
    }
    Ok(())
}

pub fn setting(conn: &Connection, key: &str) -> DbResult<String> {
    conn.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
        r.get(0)
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn import_identity_survives_edits_and_manual_entries_are_distinct() {
        let db = Database::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        db.run(|conn| {
            let mut e = crate::ledger::sample();
            e.source = "wechat".into();
            e.external_id = Some("123:expense".into());
            e.id = insert(conn, &e)?.unwrap();
            assert!(insert(conn, &e)?.is_none());
            e.category = "娱乐".into();
            update(conn, &e)?;
            assert!(insert(conn, &e)?.is_none());
            assert_eq!(entry(conn, e.id)?.unwrap().category, "娱乐");
            e.source = "manual".into();
            e.external_id = None;
            assert!(insert(conn, &e)?.is_some());
            assert!(insert(conn, &e)?.is_some());
            assert_eq!(entries(conn)?.len(), 3);
            Ok(())
        })
        .await
        .unwrap();
    }
}
