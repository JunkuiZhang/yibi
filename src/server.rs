use crate::{
    auth,
    db::{self, Database},
    import::{self, Preview, Rule},
    ledger::{self, Entry, Kind},
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post},
};
use chrono::{FixedOffset, Utc};
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

type Result<T> = std::result::Result<T, ApiError>;
pub struct ApiError(StatusCode, String);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({"error":self.1}))).into_response()
    }
}
fn bad(s: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, s.into())
}
fn internal(s: String) -> ApiError {
    eprintln!("database error: {s}");
    ApiError(
        StatusCode::INTERNAL_SERVER_ERROR,
        "操作失败，请稍后重试".into(),
    )
}

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub secure_cookie: bool,
    attempts: Arc<Mutex<Vec<Instant>>>,
    login_gate: Arc<tokio::sync::Semaphore>,
    import_gate: Arc<tokio::sync::Semaphore>,
}
impl AppState {
    pub fn new(db: Database, secure_cookie: bool) -> Self {
        Self {
            db,
            secure_cookie,
            attempts: Arc::new(Mutex::new(Vec::new())),
            login_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            import_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }
}

pub fn router(state: AppState) -> Router {
    let private = Router::new()
        .route("/api/state", get(bootstrap))
        .route("/api/logout", post(logout))
        .route("/api/entries", get(list_entries).post(create_entry))
        .route(
            "/api/entries/{id}",
            axum::routing::put(edit_entry).delete(remove_entry),
        )
        .route("/api/summary", get(summary))
        .route("/api/subscriptions", get(subscriptions))
        .route("/api/rules", post(save_rule))
        .route("/api/rules/{id}", delete(remove_rule))
        .route("/api/settings", axum::routing::put(save_settings))
        .route("/api/import/preview", post(preview))
        .route("/api/import/commit", post(commit_import))
        .route("/api/export", get(export))
        .route_layer(middleware::from_fn_with_state(state.clone(), authorize));
    Router::new()
        .merge(private)
        .route("/api/login", post(login))
        .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
        .route(
            "/",
            get(|| async { Html(include_str!("../web/index.html")) }),
        )
        .route(
            "/app.css",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
                    include_str!("../web/app.css"),
                )
            }),
        )
        .route(
            "/app.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../web/app.js"),
                )
            }),
        )
        // Leave room for multipart headers around an 8 MiB bill file.
        .layer(DefaultBodyLimit::max(9 * 1024 * 1024))
        .layer(middleware::from_fn(security))
        .with_state(state)
}

async fn security(request: Request, next: Next) -> Response {
    let mut response = if request.uri().path().starts_with("/api/")
        && !matches!(
            *request.method(),
            axum::http::Method::GET | axum::http::Method::HEAD
        )
        && request
            .headers()
            .get("x-tally-request")
            .and_then(|v| v.to_str().ok())
            != Some("1")
    {
        ApiError(StatusCode::FORBIDDEN, "请求验证失败，请刷新页面".into()).into_response()
    } else {
        next.run(request).await
    };
    for (name, value) in [
        ("cache-control", "no-store"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
        (
            "permissions-policy",
            "camera=(), microphone=(), geolocation=()",
        ),
    ] {
        response.headers_mut().insert(name, value.parse().unwrap());
    }
    response
}

fn cookie(headers: &HeaderMap) -> String {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .find_map(|s| s.trim().strip_prefix("tally_session="))
        .unwrap_or("")
        .to_owned()
}

async fn authorize(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let raw = cookie(request.headers());
    match state.db.run(move |c| auth::session(c, &raw)).await {
        Ok(true) => next.run(request).await,
        Ok(false) => ApiError(StatusCode::UNAUTHORIZED, "请先登录".into()).into_response(),
        Err(e) => internal(e).into_response(),
    }
}

#[derive(Deserialize)]
struct Login {
    username: String,
    password: String,
}
async fn login(State(state): State<AppState>, Json(input): Json<Login>) -> Result<Response> {
    if input.username.len() > 256 || input.password.len() > 1024 {
        return Err(bad("用户名或密码过长"));
    }
    {
        let mut attempts = state
            .attempts
            .lock()
            .map_err(|_| internal("login lock".into()))?;
        attempts.retain(|t| t.elapsed() < Duration::from_secs(60));
        if attempts.len() >= 8 {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "尝试过于频繁，请一分钟后重试".into(),
            ));
        }
        attempts.push(Instant::now());
    }
    let _permit = state.login_gate.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "登录处理中，请稍后重试".into(),
        )
    })?;
    let raw = state
        .db
        .run(move |c| {
            if !auth::verify(c, &input.username, &input.password)? {
                return Ok(None);
            }
            Ok(Some(auth::new_session(c)?))
        })
        .await
        .map_err(internal)?
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "用户名或密码不正确".into()))?;
    let cookie = format!(
        "tally_session={raw}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{}",
        auth::SESSION_SECONDS,
        if state.secure_cookie { "; Secure" } else { "" }
    );
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({"ok":true}))).into_response())
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response> {
    let hash = auth::digest(&cookie(&headers));
    state
        .db
        .run(move |c| {
            c.execute("DELETE FROM sessions WHERE token_hash=?1", [hash])
                .map(|_| ())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(internal)?;
    Ok((
        [(
            header::SET_COOKIE,
            format!(
                "tally_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
                if state.secure_cookie { "; Secure" } else { "" }
            ),
        )],
        Json(json!({"ok":true})),
    )
        .into_response())
}

fn load_rules(c: &rusqlite::Connection) -> db::DbResult<Vec<Rule>> {
    let mut stmt = c
        .prepare("SELECT id,pattern,category,kind FROM rules ORDER BY length(pattern) DESC,id DESC")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, String>(3)?))
        })
        .map_err(|e| e.to_string())?;
    rows.map(|r| {
        let (id, pattern, category, kind) = r.map_err(|e| e.to_string())?;
        Ok(Rule {
            id,
            pattern,
            category,
            kind: serde_json::from_value(json!(kind)).map_err(|e| e.to_string())?,
        })
    })
    .collect()
}

async fn bootstrap(State(state): State<AppState>) -> Result<Json<Value>> {
    state.db.run(|c| {
        let entries=db::entries(c)?;
        let mut accounts:Vec<String>=entries.iter().map(|e|e.account.clone()).chain(["微信".into(),"支付宝".into(),"工资卡".into(),"现金".into()]).collect();
        accounts.sort();accounts.dedup();
        let username:String=c.query_row("SELECT username FROM owner WHERE id=1",[],|r|r.get(0)).map_err(|e|e.to_string())?;
        Ok(Json(json!({"username":username,"ledger_start":db::setting(c,"ledger_start")?,"accounts":accounts,"rules":load_rules(c)?,"last_salary":entries.iter().find(|e|e.kind==Kind::Income && e.category=="工资"),"count":entries.len()})))
    }).await.map_err(internal)
}

#[derive(Default, Deserialize)]
struct EntryQuery {
    month: Option<String>,
    q: Option<String>,
    kind: Option<String>,
    mode: Option<String>,
    page: Option<usize>,
}
async fn list_entries(
    State(state): State<AppState>,
    Query(query): Query<EntryQuery>,
) -> Result<Json<Value>> {
    if let Some(m) = &query.month {
        ledger::month_index(m).map_err(bad)?;
    }
    state.db.run(move|c| {
        let start=db::setting(c,"ledger_start")?;
        let search=query.q.unwrap_or_default().to_lowercase();
        let rows:Vec<_>=db::entries(c)?.into_iter().filter(|e| {
            let in_period=match &query.month {
                Some(m) if query.mode.as_deref()==Some("allocated")=>e.date.starts_with(m) || e.expense_in(m,true)!=0,
                Some(m)=>e.date.starts_with(m),
                None=>e.date>=start,
            };
            in_period && query.kind.as_ref().is_none_or(|k|k.is_empty() || e.kind.as_str()==k)
                && (search.is_empty() || format!("{} {} {} {} {}",e.merchant,e.description,e.category,e.account,e.note).to_lowercase().contains(&search))
        }).collect();
        let total=rows.len();
        let page=query.page.unwrap_or(1).clamp(1,10000);
        Ok(Json(json!({"entries":rows.into_iter().skip((page-1)*50).take(50).collect::<Vec<_>>(),"total":total,"page":page})))
    }).await.map_err(internal)
}

async fn create_entry(
    State(state): State<AppState>,
    Json(mut entry): Json<Entry>,
) -> Result<Json<Value>> {
    entry.id = 0;
    entry.source = "manual".into();
    entry.external_id = None;
    entry.validate().map_err(bad)?;
    state
        .db
        .run(move |c| Ok(Json(json!({"id":db::insert(c,&entry)?}))))
        .await
        .map_err(internal)
}
async fn edit_entry(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(mut entry): Json<Entry>,
) -> Result<Json<Value>> {
    entry.validate().map_err(bad)?;
    state
        .db
        .run(move |c| {
            let previous = db::entry(c, id)?.ok_or("记录不存在")?;
            entry.id = id;
            entry.source = previous.source;
            entry.external_id = previous.external_id;
            db::update(c, &entry)?;
            Ok(Json(json!({"ok":true})))
        })
        .await
        .map_err(bad)
}
async fn remove_entry(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Json<Value>> {
    state
        .db
        .run(move |c| {
            c.execute("DELETE FROM entries WHERE id=?1", [id])
                .map(|n| Json(json!({"deleted":n})))
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(internal)
}

#[derive(Deserialize)]
struct SummaryQuery {
    month: String,
    mode: Option<String>,
}
async fn summary(
    State(state): State<AppState>,
    Query(query): Query<SummaryQuery>,
) -> Result<Json<Value>> {
    ledger::month_index(&query.month).map_err(bad)?;
    state
        .db
        .run(move |c| {
            make_summary(
                &db::entries(c)?,
                &db::setting(c, "ledger_start")?,
                &query.month,
                query.mode.as_deref() == Some("allocated"),
            )
        })
        .await
        .map(Json)
        .map_err(bad)
}

pub fn make_summary(
    entries: &[Entry],
    start: &str,
    month: &str,
    allocated: bool,
) -> db::DbResult<Value> {
    let index = ledger::month_index(month)?;
    if month < &start[..7] {
        return Err("该月份早于正式记账起点".into());
    }
    let mut income = 0i64;
    let mut expense = 0i64;
    let mut refund = 0i64;
    let mut categories = BTreeMap::<String, i64>::new();
    let mut count = 0usize;
    let mut unclassified = 0usize;
    for e in entries {
        let amount = e.expense_in(month, allocated);
        expense += amount;
        if amount != 0 {
            *categories.entry(e.category.clone()).or_default() += amount;
        }
        if e.date.starts_with(month) {
            if e.kind == Kind::Income {
                income += e.amount_cents;
            }
            if e.kind == Kind::Refund {
                refund += e.amount_cents;
            }
        }
        if (e.date.starts_with(month) || amount != 0) && e.kind != Kind::Transfer {
            count += 1;
            if e.category == "待分类" {
                unclassified += 1;
            }
        }
    }
    let year = &month[..4];
    let mut salary = 0i64;
    let mut bonus = 0i64;
    let mut year_income = 0i64;
    let mut income_months = HashSet::new();
    for e in entries.iter().filter(|e| {
        e.kind == Kind::Income
            && e.date.starts_with(year)
            && e.date.as_str() >= start
            && e.date[..7] <= month[..]
    }) {
        year_income += e.amount_cents;
        income_months.insert(&e.date[..7]);
        if e.category == "工资" {
            salary += e.amount_cents;
        }
        if e.category == "年终奖" {
            bonus += e.amount_cents;
        }
    }
    let first = ledger::month_index(&format!("{year}-01"))?.max(ledger::month_index(&start[..7])?);
    let months: Vec<Value> = (first..=index)
        .map(|i| {
            let m = format!("{:04}-{:02}", i / 12, i % 12 + 1);
            let inc: i64 = entries
                .iter()
                .filter(|e| e.kind == Kind::Income && e.date.starts_with(&m))
                .map(|e| e.amount_cents)
                .sum();
            let exp: i64 = entries.iter().map(|e| e.expense_in(&m, allocated)).sum();
            json!({"month":m,"income":inc,"expense":exp})
        })
        .collect();
    let pending: i64 = entries
        .iter()
        .filter(|e| e.kind == Kind::Expense && e.date[..7] <= month[..])
        .map(|e| match (&e.split_start, e.split_months) {
            (Some(s), Some(n)) => (0..n)
                .filter_map(|i| ledger::add_months(s, i).ok())
                .filter(|m| m.as_str() > month)
                .map(|m| e.expense_in(&m, true))
                .sum(),
            _ => 0,
        })
        .sum();
    let mut categories: Vec<_> = categories
        .into_iter()
        .map(|(name, amount)| json!({"name":name,"amount":amount}))
        .collect();
    categories.sort_by_key(|v| -v["amount"].as_i64().unwrap_or(0));
    Ok(
        json!({"month":month,"mode":if allocated{"allocated"}else{"cash"},"income":income,"expense":expense,"balance":income-expense,"refund":refund,"pending":pending,"count":count,"unclassified":unclassified,"categories":categories,"months":months,"year_income":year_income,"salary":salary,"bonus":bonus,"income_months":income_months.len(),"income_average":if income_months.is_empty(){0}else{year_income/income_months.len() as i64},"income_average_without_bonus":if income_months.is_empty(){0}else{(year_income-bonus)/income_months.len() as i64}}),
    )
}

async fn subscriptions(State(state): State<AppState>) -> Result<Json<Value>> {
    state.db.run(|c|Ok(Json(json!({"entries":db::entries(c)?.into_iter().filter(|e|e.split_start.is_some()).collect::<Vec<_>>()})))).await.map_err(internal)
}

#[derive(Deserialize)]
struct RuleInput {
    pattern: String,
    category: String,
    kind: Kind,
}
async fn save_rule(
    State(state): State<AppState>,
    Json(rule): Json<RuleInput>,
) -> Result<Json<Value>> {
    if rule.pattern.trim().is_empty()
        || rule.pattern.chars().count() > 200
        || rule.category.trim().is_empty()
        || rule.category.chars().count() > 60
    {
        return Err(bad("请输入有效的商户关键词和分类"));
    }
    state.db.run(move|c|c.execute("INSERT INTO rules(pattern,category,kind) VALUES (?1,?2,?3) ON CONFLICT(pattern) DO UPDATE SET category=excluded.category,kind=excluded.kind",params![rule.pattern.trim(),rule.category.trim(),rule.kind.as_str()]).map(|_|Json(json!({"ok":true}))).map_err(|e|e.to_string())).await.map_err(internal)
}
async fn remove_rule(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Json<Value>> {
    state
        .db
        .run(move |c| {
            c.execute("DELETE FROM rules WHERE id=?1", [id])
                .map(|_| Json(json!({"ok":true})))
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(internal)
}

#[derive(Deserialize)]
struct Settings {
    ledger_start: String,
}
async fn save_settings(
    State(state): State<AppState>,
    Json(input): Json<Settings>,
) -> Result<Json<Value>> {
    if !ledger::valid_date(&input.ledger_start) || !input.ledger_start.ends_with("-01") {
        return Err(bad("请选择记账起始月份"));
    }
    state
        .db
        .run(move |c| {
            c.execute(
                "UPDATE settings SET value=?1 WHERE key='ledger_start'",
                [input.ledger_start],
            )
            .map(|_| Json(json!({"ok":true})))
            .map_err(|e| e.to_string())
        })
        .await
        .map_err(internal)
}

async fn preview(State(state): State<AppState>, mut upload: Multipart) -> Result<Json<Value>> {
    let _permit = state.import_gate.clone().try_acquire_owned().map_err(|_| {
        ApiError(
            StatusCode::TOO_MANY_REQUESTS,
            "正在解析另一份账单，请稍后重试".into(),
        )
    })?;
    let field = upload
        .next_field()
        .await
        .map_err(|_| bad("文件上传失败"))?
        .ok_or_else(|| bad("请选择账单文件"))?;
    let name = field
        .file_name()
        .ok_or_else(|| bad("缺少文件名"))?
        .to_owned();
    let bytes = field
        .bytes()
        .await
        .map_err(|_| bad("上传失败，文件不能超过 8MB"))?;
    let rules = state.db.run(|c| load_rules(c)).await.map_err(internal)?;
    let mut parsed = tokio::task::spawn_blocking(move || import::parse(&name, &bytes, &rules))
        .await
        .map_err(|_| internal("import task".into()))?
        .map_err(bad)?;
    state
        .db
        .run(move |c| {
            let existing: HashSet<_> = db::entries(c)?
                .into_iter()
                .filter_map(|e| e.external_id.map(|id| (e.source, id)))
                .collect();
            for row in &mut parsed.rows {
                if row
                    .entry
                    .external_id
                    .as_ref()
                    .is_some_and(|id| existing.contains(&(row.entry.source.clone(), id.clone())))
                {
                    row.duplicate = true;
                }
            }
            let token = auth::token();
            let now = Utc::now().timestamp();
            // Single owner: keep at most one active preview, never the uploaded original file.
            let tx = c.transaction().map_err(|e| e.to_string())?;
            tx.execute("DELETE FROM previews", [])
                .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO previews(token,created,payload) VALUES (?1,?2,?3)",
                params![
                    token,
                    now,
                    serde_json::to_string(&parsed).map_err(|e| e.to_string())?
                ],
            )
            .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Json(json!({"token":token,"preview":parsed})))
        })
        .await
        .map_err(internal)
}

#[derive(Deserialize)]
struct Commit {
    token: String,
    indices: Vec<usize>,
}
async fn commit_import(
    State(state): State<AppState>,
    Json(input): Json<Commit>,
) -> Result<Json<Value>> {
    if input.indices.len() > 10000 || input.indices.is_empty() {
        return Err(bad("请选择 1–10000 条记录"));
    }
    state
        .db
        .run(move |c| {
            let tx = c.transaction().map_err(|e| e.to_string())?;
            let text: Option<String> = tx
                .query_row(
                    "SELECT payload FROM previews WHERE token=?1 AND created>?2",
                    params![input.token, Utc::now().timestamp() - 1800],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            let preview: Preview =
                serde_json::from_str(&text.ok_or("预览已过期或已导入，请重新上传")?)
                    .map_err(|e| e.to_string())?;
            let mut inserted = 0;
            let mut skipped = 0;
            let mut seen = HashSet::new();
            for index in input.indices {
                if !seen.insert(index) {
                    continue;
                }
                let row = preview.rows.get(index).ok_or("选择的记录不存在")?;
                if row.duplicate {
                    skipped += 1;
                    continue;
                }
                if db::insert(&tx, &row.entry)?.is_some() {
                    inserted += 1;
                } else {
                    skipped += 1;
                }
            }
            tx.execute("DELETE FROM previews WHERE token=?1", [input.token])
                .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Json(json!({"inserted":inserted,"skipped":skipped})))
        })
        .await
        .map_err(bad)
}

async fn export(State(state): State<AppState>) -> Result<Response> {
    let value=state.db.run(|c|Ok(json!({"version":1,"exported_at":Utc::now().to_rfc3339(),"ledger_start":db::setting(c,"ledger_start")?,"entries":db::entries(c)?,"rules":load_rules(c)?}))).await.map_err(internal)?;
    Ok((
        [(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"tally-export.json\"",
        )],
        Json(value),
    )
        .into_response())
}

pub fn today() -> String {
    Utc::now()
        .with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap())
        .date_naive()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use tower::ServiceExt;

    async fn app() -> Router {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        db.run(|c| auth::set_owner(c, "owner", "test-password-long"))
            .await
            .unwrap();
        router(AppState::new(db, false))
    }

    #[tokio::test]
    async fn authentication_csrf_and_logout_protect_every_data_route() {
        let app = app().await;
        for route in [
            "/api/state",
            "/api/entries",
            "/api/summary?month=2026-07",
            "/api/subscriptions",
            "/api/export",
        ] {
            let r = app
                .clone()
                .oneshot(Request::builder().uri(route).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{route}");
        }
        let payload = r#"{"username":"owner","password":"test-password-long"}"#;
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header("content-type", "application/json")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/login")
                    .header("content-type", "application/json")
                    .header("x-tally-request", "1")
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let cookie = r.headers()[header::SET_COOKIE].to_str().unwrap().to_owned();
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        let cookie = cookie.split(';').next().unwrap();
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/state")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(
            String::from_utf8(to_bytes(r.into_body(), 10000).await.unwrap().to_vec())
                .unwrap()
                .contains("owner")
        );
        let r = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/logout")
                    .header("cookie", cookie)
                    .header("x-tally-request", "1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app
            .oneshot(
                Request::builder()
                    .uri("/api/state")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn salary_bonus_refund_and_transfer_are_counted_once() {
        let mut salary = ledger::sample();
        salary.date = "2026-07-05".into();
        salary.kind = Kind::Income;
        salary.amount_cents = 2_500_000;
        salary.category = "工资".into();
        salary.split_start = None;
        salary.split_months = None;
        let mut bonus = salary.clone();
        bonus.category = "年终奖".into();
        bonus.amount_cents = 1_000_000;
        let mut transfer = salary.clone();
        transfer.kind = Kind::Transfer;
        let mut expense = ledger::sample();
        expense.date = "2026-07-01".into();
        let mut refund = expense.clone();
        refund.kind = Kind::Refund;
        refund.split_start = None;
        refund.split_months = None;
        refund.amount_cents = 500;
        let rows = vec![salary, bonus, transfer, expense, refund];
        let cash = make_summary(&rows, "2026-07-01", "2026-07", false).unwrap();
        assert_eq!(cash["income"], 3_500_000);
        assert_eq!(cash["expense"], 14_300);
        assert_eq!(cash["pending"], 14_800);
        assert_eq!(cash["income_months"], 1);
        assert_eq!(cash["bonus"], 1_000_000);
        let accrual = make_summary(&rows, "2026-07-01", "2027-02", true).unwrap();
        assert_eq!(accrual["expense"], 1234);
    }
}
