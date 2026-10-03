use crate::db::DbResult;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use rand::{RngCore, rngs::OsRng};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

pub const SESSION_SECONDS: i64 = 7 * 24 * 3600;

pub fn token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub fn set_owner(conn: &mut Connection, username: &str, password: &str) -> DbResult<()> {
    if username.trim().is_empty() || username.chars().count() > 64 {
        return Err("用户名需为 1–64 个字符".into());
    }
    if password.chars().count() < 12 || password.len() > 1024 {
        return Err("密码至少 12 个字符，最多 1024 字节".into());
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| e.to_string())?
        .to_string();
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    tx.execute("INSERT INTO owner (id,username,password_hash) VALUES (1,?1,?2) ON CONFLICT(id) DO UPDATE SET username=excluded.username,password_hash=excluded.password_hash",params![username.trim(),hash]).map_err(|e|e.to_string())?;
    tx.execute("DELETE FROM sessions", [])
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

pub fn verify(conn: &Connection, username: &str, password: &str) -> DbResult<bool> {
    let owner: Option<(String, String)> = conn
        .query_row(
            "SELECT username,password_hash FROM owner WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((name, hash)) = owner else {
        return Ok(false);
    };
    let parsed = PasswordHash::new(&hash).map_err(|_| "密码存储异常")?;
    // Always do password verification, including for an incorrect username.
    let valid = Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok();
    Ok(valid && username == name)
}

pub fn session(conn: &Connection, raw: &str) -> DbResult<bool> {
    if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(false);
    }
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE token_hash=?1 AND expires>?2)",
        params![digest(raw), chrono::Utc::now().timestamp()],
        |r| r.get(0),
    )
    .map_err(|e| e.to_string())
}

pub fn new_session(conn: &Connection) -> DbResult<String> {
    let now = chrono::Utc::now().timestamp();
    conn.execute("DELETE FROM sessions WHERE expires<=?1", [now])
        .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM sessions WHERE token_hash IN (SELECT token_hash FROM sessions ORDER BY expires DESC LIMIT -1 OFFSET 15)",[]).map_err(|e|e.to_string())?;
    let raw = token();
    conn.execute(
        "INSERT INTO sessions(token_hash,expires) VALUES (?1,?2)",
        params![digest(&raw), now + SESSION_SECONDS],
    )
    .map_err(|e| e.to_string())?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_sessions_and_password_reset() {
        let db =
            crate::db::Database::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        let mut conn = db.0.lock().unwrap();
        set_owner(&mut conn, "owner", "a-good-password-123").unwrap();
        assert!(verify(&conn, "owner", "a-good-password-123").unwrap());
        assert!(!verify(&conn, "other", "a-good-password-123").unwrap());
        assert!(!verify(&conn, "owner", "wrong").unwrap());
        let raw = new_session(&conn).unwrap();
        assert!(session(&conn, &raw).unwrap());
        assert!(!session(&conn, &digest(&raw)).unwrap());
        set_owner(&mut conn, "owner", "a-new-password-456").unwrap();
        assert!(!session(&conn, &raw).unwrap());
    }
}
