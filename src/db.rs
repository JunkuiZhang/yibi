use crate::ledger::{DEFAULT_START, Entry, Kind};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
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

    pub fn from_connection(mut conn: Connection) -> DbResult<Self> {
        initialize(&mut conn)?;
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

fn initialize(conn: &mut Connection) -> DbResult<()> {
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version > 2 {
        return Err("数据库版本较新，请使用新版“一笔”".into());
    }
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON; PRAGMA cache_size = -2048;",
    )
    .map_err(|e| e.to_string())?;
    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    // Re-read under the write lock in case another process finished upgrading
    // between the initial version check and this transaction.
    let version: i64 = tx
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version > 2 {
        return Err("数据库版本较新，请使用新版“一笔”".into());
    }
    if version == 1 {
        // SQLite cannot alter CHECK constraints in place. Preserve all stored
        // payloads and identifiers, and rebuild explicit indexes/triggers too.
        let definitions = {
            let mut stmt = tx
                .prepare("SELECT sql FROM sqlite_schema WHERE tbl_name='entries' AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type,name")
                .map_err(|e| e.to_string())?;
            stmt.query_map([], |row| row.get::<_, String>(0))
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };
        tx.execute_batch(
            "CREATE TABLE entries_v2 (
                id INTEGER PRIMARY KEY,
                date TEXT NOT NULL,
                kind TEXT NOT NULL CHECK (kind IN ('expense','income','transfer','refund')),
                amount_cents INTEGER NOT NULL CHECK(amount_cents>=0 AND (kind!='refund' OR amount_cents>0)),
                source TEXT NOT NULL,
                external_id TEXT,
                payload TEXT NOT NULL,
                UNIQUE(source, external_id)
            );
            INSERT INTO entries_v2 (id,date,kind,amount_cents,source,external_id,payload)
                SELECT id,date,kind,amount_cents,source,external_id,payload FROM entries;
            DROP TABLE entries;
            ALTER TABLE entries_v2 RENAME TO entries;",
        )
        .map_err(|e| format!("数据库升级失败，原数据已保留：{e}"))?;
        for definition in definitions {
            tx.execute_batch(&definition).map_err(|e| e.to_string())?;
        }
    }
    tx.execute_batch(include_str!("schema.sql"))
        .map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT OR IGNORE INTO settings (key,value) VALUES ('ledger_start',?1)",
        [DEFAULT_START],
    )
    .map_err(|e| e.to_string())?;
    tx.pragma_update(None, "user_version", 2)
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

pub fn insert(conn: &Connection, entry: &Entry) -> DbResult<Option<i64>> {
    entry.validate()?;
    if let Some(external_id) = &entry.external_id {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM entries WHERE source=?1 AND external_id=?2)",
                params![entry.source, external_id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if exists {
            return Ok(None);
        }
    }
    validate_refund_links(conn, entry, None)?;
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
    if self::entry(conn, entry.id)?.is_none() {
        return Err("记录不存在".into());
    }
    validate_refund_links(conn, entry, Some(entry.id))?;
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

fn validate_refund_links(
    conn: &Connection,
    candidate: &Entry,
    replacing: Option<i64>,
) -> DbResult<()> {
    if candidate.refund_of.is_none() && replacing.is_none() {
        return Ok(());
    }
    let saved = entries(conn)?;
    if let Some(parent_id) = candidate.refund_of {
        if Some(parent_id) == replacing {
            return Err("退款不能关联自身，请选择原支出记录".into());
        }
        let parent = saved
            .iter()
            .find(|e| e.id == parent_id && e.kind == Kind::Expense)
            .ok_or("关联的原支出不存在或不是支出记录")?;
        if candidate.date < parent.date {
            return Err("退款日期不能早于原支出日期".into());
        }
        let previous = saved
            .iter()
            .filter(|e| {
                e.kind == Kind::Refund && e.refund_of == Some(parent_id) && Some(e.id) != replacing
            })
            .fold(0_i64, |total, e| total.saturating_add(e.amount_cents));
        if previous.saturating_add(candidate.amount_cents) > parent.amount_cents {
            return Err("关联退款的累计金额不能超过原支出金额".into());
        }
    }
    if let Some(id) = replacing {
        let linked: Vec<_> = saved
            .iter()
            .filter(|e| e.kind == Kind::Refund && e.refund_of == Some(id))
            .collect();
        if !linked.is_empty() {
            if candidate.kind != Kind::Expense {
                return Err("这笔支出已有退款关联，请先解除退款关联再修改类型".into());
            }
            let total = linked
                .iter()
                .fold(0_i64, |total, e| total.saturating_add(e.amount_cents));
            if total > candidate.amount_cents {
                return Err("支出金额不能小于已关联退款的累计金额".into());
            }
            if linked.iter().any(|e| e.date < candidate.date) {
                return Err("支出日期不能晚于已关联退款的日期".into());
            }
        }
    }
    Ok(())
}

pub fn remove(conn: &Connection, id: i64) -> DbResult<usize> {
    let existing = entry(conn, id)?.ok_or("记录不存在")?;
    if existing.kind == Kind::Expense
        && entries(conn)?
            .iter()
            .any(|e| e.kind == Kind::Refund && e.refund_of == Some(id))
    {
        return Err("这笔支出已有退款关联，请先解除退款关联再删除".into());
    }
    conn.execute("DELETE FROM entries WHERE id=?1", [id])
        .map_err(|e| e.to_string())
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

    // Snapshot of the released v1 layout, deliberately independent of schema.sql.
    const V1_SCHEMA: &str = "
        CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE owner (id INTEGER PRIMARY KEY CHECK(id=1), username TEXT NOT NULL, password_hash TEXT NOT NULL);
        CREATE TABLE sessions (token_hash TEXT PRIMARY KEY, expires INTEGER NOT NULL);
        CREATE TABLE entries (
            id INTEGER PRIMARY KEY,
            date TEXT NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('expense','income','transfer','refund')),
            amount_cents INTEGER NOT NULL CHECK(amount_cents>0),
            source TEXT NOT NULL,
            external_id TEXT,
            payload TEXT NOT NULL,
            UNIQUE(source,external_id)
        );
        CREATE INDEX entries_date ON entries(date);
        CREATE TABLE rules (id INTEGER PRIMARY KEY, pattern TEXT NOT NULL UNIQUE COLLATE NOCASE, category TEXT NOT NULL, kind TEXT NOT NULL);
        CREATE TABLE previews (token TEXT PRIMARY KEY, created INTEGER NOT NULL, payload TEXT NOT NULL);
        PRAGMA user_version=1;";

    fn old_database() -> (Connection, String) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        let mut e = crate::ledger::sample();
        e.source = "wechat".into();
        e.external_id = Some("legacy-test-order:expense".into());
        let mut old = serde_json::to_value(&e).unwrap();
        // v0.1 exports did not yet contain refund metadata.
        for field in ["refunded_cents", "refund_of", "import_status"] {
            old.as_object_mut().unwrap().remove(field);
        }
        let payload = old.to_string();
        conn.execute(
            "INSERT INTO entries (id,date,kind,amount_cents,source,external_id,payload) VALUES (42,?1,'expense',?2,?3,?4,?5)",
            params![e.date,e.amount_cents,e.source,e.external_id,payload],
        ).unwrap();
        conn.execute_batch(
            "INSERT INTO settings VALUES ('ledger_start','2026-08-01');
             INSERT INTO owner VALUES (1,'test-owner','test-password-hash');
             INSERT INTO sessions VALUES ('test-token-hash',12345);
             INSERT INTO rules VALUES (7,'test-merchant','交通出行','expense');
             INSERT INTO previews VALUES ('test-preview',12345,'test-preview-payload');
             CREATE INDEX entries_kind_source ON entries(kind,source);
             CREATE TABLE audit (entry_id INTEGER NOT NULL);
             CREATE TRIGGER entries_test_audit AFTER UPDATE ON entries BEGIN INSERT INTO audit VALUES (NEW.id); END;",
        ).unwrap();
        (conn, payload)
    }

    fn version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    fn table_payloads(conn: &Connection) -> Vec<String> {
        [
            "SELECT value FROM settings WHERE key='ledger_start'",
            "SELECT username || '|' || password_hash FROM owner WHERE id=1",
            "SELECT token_hash || '|' || expires FROM sessions",
            "SELECT id || '|' || pattern || '|' || category || '|' || kind FROM rules",
            "SELECT token || '|' || created || '|' || payload FROM previews",
        ]
        .iter()
        .map(|sql| conn.query_row(sql, [], |row| row.get(0)).unwrap())
        .collect()
    }

    #[test]
    fn v1_upgrade_preserves_rows_legacy_payloads_indexes_triggers_and_other_tables() {
        let (mut conn, payload) = old_database();
        let before = table_payloads(&conn);
        initialize(&mut conn).unwrap();
        assert_eq!(version(&conn), 2);
        assert_eq!(table_payloads(&conn), before);
        let stored: String = conn
            .query_row("SELECT payload FROM entries WHERE id=42", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(stored, payload);
        let mut original = entry(&conn, 42).unwrap().unwrap();
        assert_eq!(original.id, 42);
        assert_eq!(original.refunded_cents, 0);
        assert!(original.refund_of.is_none());
        assert!(original.import_status.is_empty());
        assert!(insert(&conn, &original).unwrap().is_none());
        assert!(conn.execute("INSERT INTO entries (date,kind,amount_cents,source,external_id,payload) SELECT date,kind,amount_cents,source,external_id,payload FROM entries WHERE id=42", []).is_err());
        let indexes: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name IN ('entries_date','entries_kind_source')", [], |row| row.get(0),
        ).unwrap();
        assert_eq!(indexes, 2);
        original.note = "preserved trigger".into();
        update(&conn, &original).unwrap();
        let audited: i64 = conn
            .query_row("SELECT entry_id FROM audit", [], |row| row.get(0))
            .unwrap();
        assert_eq!(audited, 42);
        original.id = 0;
        original.amount_cents = 0;
        original.split_start = None;
        original.split_months = None;
        original.external_id = Some("zero-order:expense".into());
        assert_eq!(insert(&conn, &original).unwrap(), Some(43));
        initialize(&mut conn).unwrap();
        assert_eq!(entries(&conn).unwrap().len(), 2);
        assert_eq!(table_payloads(&conn), before);
    }

    #[test]
    fn failed_v1_upgrade_rolls_back_schema_rows_and_version() {
        let (mut conn, payload) = old_database();
        let before = table_payloads(&conn);
        // A malformed old record must fail the new CHECK during copying. Even
        // though the new table was created first, rollback must remove it.
        conn.execute_batch("PRAGMA ignore_check_constraints=ON; UPDATE entries SET amount_cents=-1 WHERE id=42; PRAGMA ignore_check_constraints=OFF;").unwrap();
        let old_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name='entries'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(initialize(&mut conn).unwrap_err().contains("升级失败"));
        assert_eq!(version(&conn), 1);
        assert_eq!(table_payloads(&conn), before);
        let row: (i64, String) = conn
            .query_row(
                "SELECT amount_cents,payload FROM entries WHERE id=42",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (-1, payload));
        let sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name='entries'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sql, old_sql);
        let leftover: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='entries_v2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(leftover, 0);
        // Retry succeeds after repairing the malformed value.
        conn.execute("UPDATE entries SET amount_cents=14800 WHERE id=42", [])
            .unwrap();
        initialize(&mut conn).unwrap();
        assert_eq!(version(&conn), 2);
    }

    #[test]
    fn newer_database_version_is_rejected_without_changes() {
        let (mut conn, _) = old_database();
        conn.pragma_update(None, "user_version", 3).unwrap();
        let before = table_payloads(&conn);
        assert!(initialize(&mut conn).unwrap_err().contains("版本较新"));
        assert_eq!(version(&conn), 3);
        assert_eq!(table_payloads(&conn), before);
        assert!(
            conn.execute("UPDATE entries SET amount_cents=0 WHERE id=42", [])
                .is_err()
        );
    }

    #[test]
    fn zero_records_support_crud_and_deduplication_but_not_refunds() {
        let db = Database::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let conn = db.0.lock().unwrap();
        let mut zero = crate::ledger::sample();
        zero.amount_cents = 0;
        zero.split_start = None;
        zero.split_months = None;
        zero.source = "alipay".into();
        for kind in [Kind::Expense, Kind::Income, Kind::Transfer] {
            zero.kind = kind;
            zero.external_id = Some(format!("zero-order:{}", zero.kind.as_str()));
            zero.id = insert(&conn, &zero).unwrap().unwrap();
            assert_eq!(entry(&conn, zero.id).unwrap().unwrap().amount_cents, 0);
            assert!(insert(&conn, &zero).unwrap().is_none());
            zero.note = "zero retained".into();
            update(&conn, &zero).unwrap();
            zero.amount_cents = 1;
            update(&conn, &zero).unwrap();
            zero.amount_cents = 0;
            update(&conn, &zero).unwrap();
            assert_eq!(
                entry(&conn, zero.id).unwrap().unwrap().note,
                "zero retained"
            );
            assert_eq!(remove(&conn, zero.id).unwrap(), 1);
        }
        zero.kind = Kind::Expense;
        zero.external_id = None;
        zero.id = insert(&conn, &zero).unwrap().unwrap();
        let mut positive_refund = refund(&zero, 1);
        positive_refund.date = zero.date.clone();
        assert!(
            insert(&conn, &positive_refund)
                .unwrap_err()
                .contains("累计金额")
        );
        zero.kind = Kind::Refund;
        assert!(insert(&conn, &zero).unwrap_err().contains("退款金额"));
        assert!(
            conn.execute("UPDATE entries SET kind='refund'", [])
                .is_err()
        );
        assert!(
            conn.execute("UPDATE entries SET amount_cents=-1", [])
                .is_err()
        );
    }

    fn expense(conn: &Connection) -> Entry {
        let mut e = crate::ledger::sample();
        e.date = "2026-07-05".into();
        e.amount_cents = 10000;
        e.split_start = None;
        e.split_months = None;
        e.id = insert(conn, &e).unwrap().unwrap();
        e
    }

    fn refund(parent: &Entry, amount: i64) -> Entry {
        let mut e = parent.clone();
        e.id = 0;
        e.kind = Kind::Refund;
        e.date = "2026-09-10".into();
        e.amount_cents = amount;
        e.refunded_cents = 0;
        e.refund_of = Some(parent.id);
        e
    }

    #[test]
    fn linked_refunds_require_existing_expense_valid_date_and_no_overrefund() {
        let db = Database::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let conn = db.0.lock().unwrap();
        let original = expense(&conn);
        let mut first = refund(&original, 4000);
        first.refund_of = Some(999);
        assert!(insert(&conn, &first).unwrap_err().contains("原支出"));
        first.refund_of = Some(original.id);
        first.date = "2026-07-04".into();
        assert!(insert(&conn, &first).unwrap_err().contains("日期"));
        first.date = "2026-09-10".into();
        first.source = "wechat".into();
        first.external_id = Some("first-refund".into());
        first.id = insert(&conn, &first).unwrap().unwrap();
        let mut second = refund(&original, 6001);
        assert!(insert(&conn, &second).unwrap_err().contains("累计"));
        second.amount_cents = 6000;
        second.id = insert(&conn, &second).unwrap().unwrap();
        assert_eq!(
            crate::ledger::refund_totals(&entries(&conn).unwrap())[&original.id],
            10000
        );
        // A duplicate imported refund is still a no-op after the payment is fully refunded.
        assert!(insert(&conn, &first).unwrap().is_none());
        update(&conn, &second).unwrap();
        second.amount_cents = 6001;
        assert!(update(&conn, &second).unwrap_err().contains("累计"));
        let mut invalid = refund(&original, 100);
        invalid.refund_of = Some(first.id);
        assert!(insert(&conn, &invalid).unwrap_err().contains("不是支出"));
        invalid.refund_of = None;
        insert(&conn, &invalid).unwrap().unwrap();
    }

    #[test]
    fn original_expense_edits_and_removal_preserve_refund_relationships() {
        let db = Database::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let conn = db.0.lock().unwrap();
        let mut original = expense(&conn);
        original.refunded_cents = 5000;
        update(&conn, &original).unwrap();
        let mut linked = refund(&original, 4000);
        linked.id = insert(&conn, &linked).unwrap().unwrap();

        original.refunded_cents = 0;
        update(&conn, &original).unwrap();
        assert_eq!(
            entry(&conn, original.id).unwrap().unwrap().refunded_cents,
            0
        );
        assert_eq!(
            crate::ledger::refund_totals(&entries(&conn).unwrap())[&original.id],
            4000
        );
        original.amount_cents = 3999;
        assert!(update(&conn, &original).unwrap_err().contains("不能小于"));
        original.amount_cents = 10000;
        original.date = "2026-09-11".into();
        assert!(update(&conn, &original).unwrap_err().contains("不能晚于"));
        original.date = "2026-07-05".into();
        original.kind = Kind::Income;
        assert!(update(&conn, &original).unwrap_err().contains("修改类型"));
        assert!(
            remove(&conn, original.id)
                .unwrap_err()
                .contains("解除退款关联")
        );
        linked.refund_of = None;
        update(&conn, &linked).unwrap();
        original.kind = Kind::Expense;
        update(&conn, &original).unwrap();
        remove(&conn, original.id).unwrap();
        assert!(entry(&conn, original.id).unwrap().is_none());
        assert!(entry(&conn, linked.id).unwrap().is_some());
        remove(&conn, linked.id).unwrap();
        assert!(remove(&conn, linked.id).unwrap_err().contains("不存在"));
    }

    #[test]
    fn refund_can_move_between_expenses_but_cannot_point_to_itself() {
        let db = Database::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let conn = db.0.lock().unwrap();
        let original = expense(&conn);
        let other = expense(&conn);
        let mut linked = refund(&original, 6000);
        linked.id = insert(&conn, &linked).unwrap().unwrap();
        linked.refund_of = Some(other.id);
        update(&conn, &linked).unwrap();
        remove(&conn, original.id).unwrap();
        assert!(remove(&conn, other.id).is_err());
        linked.refund_of = Some(linked.id);
        assert!(update(&conn, &linked).unwrap_err().contains("自身"));
        remove(&conn, linked.id).unwrap();
        remove(&conn, other.id).unwrap();
    }

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
