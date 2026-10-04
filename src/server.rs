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
    collections::{BTreeMap, HashMap, HashSet},
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
        .route("/api/entries/categories", post(batch_categories))
        .route("/api/classification", get(classification))
        .route("/api/refund-targets", get(refund_targets))
        .route(
            "/api/entries/{id}",
            axum::routing::put(edit_entry).delete(remove_entry),
        )
        .route("/api/summary", get(summary))
        .route("/api/subscriptions", get(subscriptions))
        .route("/api/rules", post(save_rule))
        .route("/api/rules/apply", post(apply_rules))
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
        .prepare("SELECT id,pattern,category,kind,description_pattern FROM rules ORDER BY (length(description_pattern)>0) DESC,length(pattern)+length(description_pattern) DESC,id DESC")
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get::<_, String>(3)?,
                r.get(4)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    rows.map(|r| {
        let (id, pattern, category, kind, description_pattern) = r.map_err(|e| e.to_string())?;
        Ok(Rule {
            id,
            pattern,
            description_pattern,
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
        Ok(Json(json!({"username":username,"ledger_start":db::setting(c,"ledger_start")?,"accounts":accounts,"rules":load_rules(c)?,"last_salary":entries.iter().find(|e|e.kind==Kind::Income && e.amount_cents>0 && e.category=="工资"),"count":entries.len()})))
    }).await.map_err(internal)
}

#[derive(Default, Deserialize)]
struct EntryQuery {
    month: Option<String>,
    q: Option<String>,
    kind: Option<String>,
    mode: Option<String>,
    page: Option<usize>,
    group_bikes: Option<bool>,
}

fn display_entry_rows(
    rows: &[&Entry],
    views: &HashMap<i64, Value>,
    group_bikes: bool,
) -> Vec<Value> {
    let mut displayed: Vec<Value> = Vec::new();
    let mut groups = HashMap::<String, usize>::new();
    for entry in rows {
        let Some(view) = views.get(&entry.id) else {
            continue;
        };
        if group_bikes
            && entry.kind == Kind::Expense
            && entry.amount_cents == 0
            && import::is_bike_ride(&entry.merchant, &entry.description)
        {
            let month = &entry.date[..7];
            let index = *groups.entry(month.to_owned()).or_insert_with(|| {
                displayed.push(json!({"row_type":"bike_group", "month":month,
                    "count":0, "entries":[]}));
                displayed.len() - 1
            });
            let group = &mut displayed[index];
            let members = group["entries"].as_array_mut().expect("bike group entries");
            members.push(view.clone());
            group["count"] = json!(members.len());
        } else {
            displayed.push(view.clone());
        }
    }
    displayed
}

fn entry_views(entries: &[Entry]) -> db::DbResult<HashMap<i64, Value>> {
    let totals = ledger::refund_totals(entries);
    let lookup: HashMap<_, _> = entries.iter().map(|e| (e.id, e)).collect();
    let mut linked = HashMap::<i64, i64>::new();
    for e in entries.iter().filter(|e| e.kind == Kind::Refund) {
        if let Some(id) = e.refund_of {
            *linked.entry(id).or_default() += e.amount_cents;
        }
    }
    entries
        .iter()
        .map(|e| {
            let mut value = serde_json::to_value(e).map_err(|e| e.to_string())?;
            let refunded = totals.get(&e.id).copied().unwrap_or(0);
            value["refunded_total_cents"] = json!(refunded);
            value["linked_refunded_cents"] = json!(linked.get(&e.id).copied().unwrap_or(0));
            value["net_amount_cents"] = json!(match e.kind {
                Kind::Expense => e.amount_cents - refunded,
                Kind::Refund => 0,
                _ => e.amount_cents,
            });
            value["refund_target"] = e
                .refund_of
                .and_then(|id| lookup.get(&id))
                .map(|target| {
                    json!({"id":target.id,"date":target.date,"merchant":target.merchant,
                "description":target.description,"amount_cents":target.amount_cents})
                })
                .unwrap_or(Value::Null);
            Ok((e.id, value))
        })
        .collect()
}

fn in_entry_month(entry: &Entry, month: &str, allocated: bool) -> bool {
    if entry.date.starts_with(month) {
        return true;
    }
    if allocated && let (Some(start), Some(count)) = (&entry.split_start, entry.split_months) {
        return month >= start.as_str()
            && ledger::add_months(start, count - 1).is_ok_and(|end| month <= end.as_str());
    }
    false
}

#[derive(Default, Deserialize)]
struct RefundTargetQuery {
    q: Option<String>,
    date: Option<String>,
    selected: Option<i64>,
}

async fn refund_targets(
    State(state): State<AppState>,
    Query(query): Query<RefundTargetQuery>,
) -> Result<Json<Value>> {
    if query
        .date
        .as_ref()
        .is_some_and(|date| !ledger::valid_date(date))
    {
        return Err(bad("退款日期无效"));
    }
    state.db.run(move |c| {
        let entries = db::entries(c)?;
        let views = entry_views(&entries)?;
        let search = query.q.unwrap_or_default().to_lowercase();
        let candidates: Vec<_> = entries.iter().filter(|e| e.kind == Kind::Expense && e.amount_cents > 0
            && query.date.as_ref().is_none_or(|date| e.date <= *date)
            && (search.is_empty() || format!("{} {} {} {} {} {}.{:02}", e.date, e.merchant,
                e.description, e.category, e.external_id.as_deref().unwrap_or(""), e.amount_cents / 100, e.amount_cents % 100)
                .to_lowercase().contains(&search))).collect();
        let total = candidates.len();
        let mut selected: Vec<_> = candidates.into_iter().take(100).map(|e| e.id).collect();
        if let Some(id) = query.selected
            && !selected.contains(&id)
            && entries.iter().any(|e| e.id == id && e.kind == Kind::Expense && e.amount_cents > 0) {
            selected.push(id);
        }
        Ok(Json(json!({"entries":selected.iter().filter_map(|id|views.get(id)).collect::<Vec<_>>(),"total":total})))
    }).await.map_err(internal)
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
        let entries = db::entries(c)?;
        let views = entry_views(&entries)?;
        let rows:Vec<_>=entries.iter().filter(|e| {
            let in_period=match &query.month {
                Some(m)=>in_entry_month(e,m,query.mode.as_deref()==Some("allocated")),
                None=>e.date>=start,
            };
            in_period && query.kind.as_ref().is_none_or(|k|k.is_empty() || e.kind.as_str()==k)
                && (search.is_empty() || format!("{} {} {} {} {}",e.merchant,e.description,e.category,e.account,e.note).to_lowercase().contains(&search))
        }).collect();
        let total=rows.len();
        // Group the complete filtered result before pagination so one month cannot
        // be split into several partial ride counts across pages.
        let displayed = display_entry_rows(&rows, &views, query.group_bikes.unwrap_or(false));
        let display_total = displayed.len();
        let page=query.page.unwrap_or(1).clamp(1,10000);
        Ok(Json(json!({"entries":displayed.into_iter().skip((page-1)*50).take(50).collect::<Vec<_>>(),"total":total,"display_total":display_total,"page":page})))
    }).await.map_err(internal)
}

async fn create_entry(
    State(state): State<AppState>,
    Json(mut entry): Json<Entry>,
) -> Result<Json<Value>> {
    entry.id = 0;
    entry.source = "manual".into();
    entry.external_id = None;
    entry.import_status.clear();
    entry.validate().map_err(bad)?;
    state
        .db
        .run(move |c| Ok(Json(json!({"id":db::insert(c,&entry)?}))))
        .await
        .map_err(bad)
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
            entry.import_status = previous.import_status;
            db::update(c, &entry)?;
            Ok(Json(json!({"ok":true})))
        })
        .await
        .map_err(bad)
}
async fn remove_entry(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Json<Value>> {
    state
        .db
        .run(move |c| db::remove(c, id).map(|n| Json(json!({"deleted":n}))))
        .await
        .map_err(bad)
}

#[derive(Deserialize)]
struct CategoryBatch {
    ids: Vec<i64>,
    category: String,
    #[serde(default = "fixed_category")]
    category_locked: bool,
}

#[derive(Default, Deserialize)]
struct ClassificationQuery {
    q: Option<String>,
    merchant: Option<String>,
    kind: Option<Kind>,
    pending: Option<bool>,
    page: Option<usize>,
}

async fn classification(
    State(state): State<AppState>,
    Query(query): Query<ClassificationQuery>,
) -> Result<Json<Value>> {
    state.db.run(move |c| {
        let entries = db::entries(c)?;
        let views = entry_views(&entries)?;
        let search = query.q.unwrap_or_default().to_lowercase();
        let rows: Vec<_> = entries.iter().filter(|e| {
            matches!(e.kind, Kind::Expense | Kind::Income)
                && views[&e.id]["net_amount_cents"].as_i64().unwrap_or(0) > 0
                && (!query.pending.unwrap_or(true) || e.category == "待分类")
                && query.kind.as_ref().is_none_or(|kind| *kind == e.kind)
                && (search.is_empty() || format!("{} {} {} {}",e.merchant,e.description,e.category,e.note).to_lowercase().contains(&search))
        }).collect();
        let mut grouped = BTreeMap::<String,(usize,i64)>::new();
        for e in &rows {
            let group = grouped.entry(e.merchant.clone()).or_default();
            group.0 += 1;
            group.1 += views[&e.id]["net_amount_cents"].as_i64().unwrap_or(0);
        }
        let mut merchants: Vec<_> = grouped.into_iter().map(|(merchant,(count,amount))|json!({"merchant":merchant,"count":count,"amount_cents":amount})).collect();
        merchants.sort_by(|a,b| b["count"].as_u64().cmp(&a["count"].as_u64()).then(a["merchant"].as_str().cmp(&b["merchant"].as_str())));
        let merchants_total = merchants.len();
        let rows: Vec<_> = rows.into_iter().filter(|e|query.merchant.as_ref().is_none_or(|merchant|e.merchant == *merchant)).collect();
        let page = query.page.unwrap_or(1).clamp(1,10000);
        Ok(Json(json!({"entries":rows.iter().skip((page-1)*50).take(50).filter_map(|e|views.get(&e.id)).collect::<Vec<_>>(),"total":rows.len(),"page":page,"merchants":merchants.into_iter().take(100).collect::<Vec<_>>(),"merchants_total":merchants_total})))
    }).await.map_err(internal)
}

fn fixed_category() -> bool {
    true
}

fn categorize_selected(
    conn: &mut rusqlite::Connection,
    input: CategoryBatch,
) -> db::DbResult<Value> {
    let category = input.category.trim();
    if category.is_empty() || category.chars().count() > 60 || category.contains('\0') {
        return Err("请输入有效分类".into());
    }
    if input.ids.is_empty() || input.ids.len() > 500 || input.ids.iter().any(|id| *id <= 0) {
        return Err("请选择 1–500 笔记录".into());
    }
    let ids: HashSet<_> = input.ids.iter().copied().collect();
    if ids.len() != input.ids.len() {
        return Err("选择的记录不能重复".into());
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let mut updated = 0;
    for id in &input.ids {
        let previous = db::entry(&tx, *id)?.ok_or("有记录已删除，请刷新后重新选择")?;
        if previous.category != category || previous.category_locked != input.category_locked {
            updated += tx.execute(
                "UPDATE entries SET payload=json_set(payload,'$.category',?1,'$.category_locked',json(?2)) WHERE id=?3",
                params![category, if input.category_locked {"true"} else {"false"}, id],
            ).map_err(|e| e.to_string())?;
        }
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(json!({"ok":true,"selected":ids.len(),"updated":updated}))
}

async fn batch_categories(
    State(state): State<AppState>,
    Json(input): Json<CategoryBatch>,
) -> Result<Json<Value>> {
    state
        .db
        .run(move |c| categorize_selected(c, input).map(Json))
        .await
        .map_err(bad)
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
    let effective = ledger::effective_entries(entries);
    let entries = effective.as_slice();
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
        if e.kind == Kind::Expense {
            let mut gross = e.clone();
            gross.refunded_cents = 0;
            refund += gross.expense_in(month, allocated) - amount;
        }
        if amount != 0 {
            *categories.entry(e.category.clone()).or_default() += amount;
        }
        if e.date.starts_with(month) && e.kind == Kind::Income {
            income += e.amount_cents;
        }
        if (e.date.starts_with(month) || amount != 0) && e.kind != Kind::Transfer {
            count += 1;
            if e.category == "待分类"
                && ((e.kind == Kind::Income && e.amount_cents > 0) || amount > 0)
            {
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
            && e.amount_cents > 0
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
    state.db.run(|c| {
        let entries = db::entries(c)?;
        let views = entry_views(&entries)?;
        Ok(Json(json!({"entries":entries.iter().filter(|e|e.split_start.is_some()).filter_map(|e|views.get(&e.id)).collect::<Vec<_>>()})))
    }).await.map_err(internal)
}

#[derive(Deserialize)]
struct RuleInput {
    #[serde(default)]
    id: Option<i64>,
    pattern: String,
    #[serde(default)]
    description_pattern: String,
    category: String,
    kind: Kind,
}

fn reclassify_matching(
    conn: &rusqlite::Connection,
    scopes: &[Rule],
    rules: &[Rule],
) -> db::DbResult<Value> {
    let mut matched = 0;
    let mut updated = 0;
    let mut locked_skipped = 0;
    let mut stmt = conn
        .prepare("UPDATE entries SET payload=json_set(payload,'$.category',?1) WHERE id=?2")
        .map_err(|e| e.to_string())?;
    for entry in db::entries(conn)? {
        if !scopes
            .iter()
            .any(|rule| rule.matches(&entry.kind, &entry.merchant, &entry.description))
        {
            continue;
        }
        matched += 1;
        if entry.category_locked {
            locked_skipped += 1;
            continue;
        }
        let category =
            import::matching_rule(&entry.kind, &entry.merchant, &entry.description, rules)
                .map(|rule| rule.category.as_str())
                .unwrap_or("待分类");
        if category != entry.category {
            if category.trim().is_empty()
                || category.chars().count() > 60
                || category.contains('\0')
            {
                return Err("请输入有效分类".into());
            }
            // Only the category changes: retain even unknown payload fields and
            // avoid revalidating all refund links for each row in a large batch.
            updated += stmt
                .execute(params![category, entry.id])
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(json!({"ok":true,"matched":matched,"updated":updated,"locked_skipped":locked_skipped}))
}

fn store_rule(conn: &mut rusqlite::Connection, input: RuleInput) -> db::DbResult<Value> {
    let pattern = input.pattern.trim();
    let description_pattern = input.description_pattern.trim();
    let category = input.category.trim();
    if (pattern.is_empty() && description_pattern.is_empty())
        || pattern.chars().count() > 200
        || pattern.contains('\0')
        || description_pattern.chars().count() > 200
        || description_pattern.contains('\0')
        || category.is_empty()
        || category.chars().count() > 60
        || category.contains('\0')
    {
        return Err("请填写商户或商品关键词，以及有效分类".into());
    }
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let matching_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM rules WHERE pattern=?1 AND description_pattern=?2 AND kind=?3",
            params![pattern, description_pattern, input.kind.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let old_id = input.id.or(matching_id);
    let previous = load_rules(&tx)?
        .into_iter()
        .find(|rule| Some(rule.id) == old_id);
    if input.id.is_some() && previous.is_none() {
        return Err("规则不存在，请刷新后重试".into());
    }
    if matching_id.is_some() && matching_id != old_id {
        return Err("相同条件的规则已存在，请编辑已有规则".into());
    }
    if let Some(id) = old_id {
        tx.execute(
            "UPDATE rules SET pattern=?1,description_pattern=?2,category=?3,kind=?4 WHERE id=?5",
            params![
                pattern,
                description_pattern,
                category,
                input.kind.as_str(),
                id
            ],
        )
        .map_err(|e| e.to_string())?;
    } else {
        tx.execute(
            "INSERT INTO rules(pattern,description_pattern,category,kind) VALUES (?1,?2,?3,?4)",
            params![pattern, description_pattern, category, input.kind.as_str()],
        )
        .map_err(|e| e.to_string())?;
    }
    let mut scopes = vec![Rule {
        id: 0,
        pattern: pattern.into(),
        description_pattern: description_pattern.into(),
        category: category.into(),
        kind: input.kind,
    }];
    // Replacing a rule's kind also removes its old scope, just like deleting it.
    if let Some(previous) = previous {
        scopes.push(previous);
    }
    let result = reclassify_matching(&tx, &scopes, &load_rules(&tx)?)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(result)
}

fn delete_rule(conn: &mut rusqlite::Connection, id: i64) -> db::DbResult<Value> {
    let tx = conn.transaction().map_err(|e| e.to_string())?;
    let scopes: Vec<_> = load_rules(&tx)?
        .into_iter()
        .filter(|rule| rule.id == id)
        .collect();
    tx.execute("DELETE FROM rules WHERE id=?1", [id])
        .map_err(|e| e.to_string())?;
    let result = reclassify_matching(&tx, &scopes, &load_rules(&tx)?)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(result)
}

async fn save_rule(
    State(state): State<AppState>,
    Json(rule): Json<RuleInput>,
) -> Result<Json<Value>> {
    state
        .db
        .run(move |c| store_rule(c, rule).map(Json))
        .await
        .map_err(bad)
}
async fn remove_rule(State(state): State<AppState>, Path(id): Path<i64>) -> Result<Json<Value>> {
    state
        .db
        .run(move |c| delete_rule(c, id).map(Json))
        .await
        .map_err(bad)
}
async fn apply_rules(State(state): State<AppState>) -> Result<Json<Value>> {
    state
        .db
        .run(|c| {
            let tx = c.transaction().map_err(|e| e.to_string())?;
            let rules = load_rules(&tx)?;
            let result = reclassify_matching(&tx, &rules, &rules)?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Json(result))
        })
        .await
        .map_err(bad)
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
            let existing: HashMap<_, _> = db::entries(c)?
                .into_iter()
                .filter_map(|e| e.external_id.clone().map(|id| ((e.source.clone(), id), e)))
                .collect();
            mark_saved_import_rows(&mut parsed, &existing);
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

fn refund_can_update(previous: &Entry, incoming: &Entry) -> bool {
    previous.kind == Kind::Expense
        && incoming.kind == Kind::Expense
        && incoming.refunded_cents > previous.refunded_cents
        && incoming.refunded_cents <= previous.amount_cents
}

fn mark_saved_import_rows(parsed: &mut Preview, existing: &HashMap<(String, String), Entry>) {
    for row in &mut parsed.rows {
        if let Some(id) = &row.entry.external_id {
            let key = (row.entry.source.clone(), id.clone());
            if let Some(previous) = existing.get(&key) {
                let update = refund_can_update(previous, &row.entry);
                // The parser already chose the largest cumulative refund for
                // this identity; do not replace that choice with the first row.
                row.duplicate |= !update;
                if update && !row.duplicate {
                    row.warnings
                        .push("此支出已入账：更新累计退款金额，保留原分类、备注和分摊计划".into());
                } else if row.entry.refunded_cents > previous.amount_cents {
                    row.warnings
                        .push("账单退款金额超过已保存的支出金额，请先核对并编辑原记录".into());
                }
            }
        }
    }
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
            let mut updated = 0;
            let mut existing: HashMap<_, _> = db::entries(&tx)?
                .into_iter()
                .filter_map(|e| e.external_id.clone().map(|id| ((e.source.clone(), id), e)))
                .collect();
            let mut seen = HashSet::new();
            let rules = load_rules(&tx)?;
            for index in input.indices {
                if !seen.insert(index) {
                    continue;
                }
                let row = preview.rows.get(index).ok_or("选择的记录不存在")?;
                if row.duplicate {
                    skipped += 1;
                    continue;
                }
                if let Some(external_id) = &row.entry.external_id {
                    let key = (row.entry.source.clone(), external_id.clone());
                    if let Some(previous) = existing.get_mut(&key) {
                        if refund_can_update(previous, &row.entry) {
                            previous.refunded_cents = row.entry.refunded_cents;
                            previous.import_status = row.entry.import_status.clone();
                            db::update(&tx, previous)?;
                            updated += 1;
                        } else {
                            skipped += 1;
                        }
                        continue;
                    }
                }
                let mut entry = row.entry.clone();
                entry.category =
                    import::classify(&entry.kind, &entry.merchant, &entry.description, &rules);
                if db::insert(&tx, &entry)?.is_some() {
                    inserted += 1;
                } else {
                    skipped += 1;
                }
            }
            tx.execute("DELETE FROM previews WHERE token=?1", [input.token])
                .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Json(
                json!({"inserted":inserted,"skipped":skipped,"updated":updated}),
            ))
        })
        .await
        .map_err(bad)
}

async fn export(State(state): State<AppState>) -> Result<Response> {
    let value=state.db.run(|c|Ok(json!({"version":1,"exported_at":Utc::now().to_rfc3339(),"ledger_start":db::setting(c,"ledger_start")?,"entries":db::entries(c)?,"rules":load_rules(c)?}))).await.map_err(internal)?;
    Ok((
        [(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"yibi-export.json\"",
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

    fn rule_input(pattern: &str, category: &str, kind: Kind) -> RuleInput {
        RuleInput {
            id: None,
            pattern: pattern.into(),
            description_pattern: String::new(),
            category: category.into(),
            kind,
        }
    }

    fn rule_id(conn: &rusqlite::Connection, pattern: &str) -> i64 {
        conn.query_row("SELECT id FROM rules WHERE pattern=?1", [pattern], |row| {
            row.get(0)
        })
        .unwrap()
    }

    #[test]
    fn saved_rules_update_all_dates_and_only_category_payload_fields() {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        let mut conn = db.0.lock().unwrap();
        let mut member = ledger::sample();
        member.merchant = "合成自如商户".into();
        member.date = "2025-02-01".into();
        member.category = "人工旧分类".into();
        member.source = "wechat".into();
        member.external_id = Some("synthetic-member".into());
        let id = db::insert(&conn, &member).unwrap().unwrap();
        conn.execute("UPDATE entries SET payload=json_set(payload,'$.future_field','preserve-me') WHERE id=?1",[id]).unwrap();
        let mut zero = member.clone();
        zero.date = "2026-09-01".into();
        zero.amount_cents = 0;
        zero.external_id = None;
        zero.split_start = None;
        zero.split_months = None;
        let zero_id = db::insert(&conn, &zero).unwrap().unwrap();
        let mut refund = zero.clone();
        refund.kind = Kind::Refund;
        refund.amount_cents = 500;
        refund.refund_of = Some(id);
        db::insert(&conn, &refund).unwrap();
        let mut income = zero.clone();
        income.kind = Kind::Income;
        db::insert(&conn, &income).unwrap();
        let mut unrelated = zero.clone();
        unrelated.merchant = "另一个商户".into();
        unrelated.description = "自如商品".into();
        db::insert(&conn, &unrelated).unwrap();
        let snapshot = |conn: &rusqlite::Connection| {
            let mut stmt = conn
                .prepare("SELECT id,payload FROM entries ORDER BY id")
                .unwrap();
            stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .map(|row| {
                let (id, payload) = row.unwrap();
                (id, serde_json::from_str::<Value>(&payload).unwrap())
            })
            .collect::<HashMap<_, _>>()
        };
        let before = snapshot(&conn);
        let result =
            store_rule(&mut conn, rule_input(" 自如 ", " 居家生活 ", Kind::Expense)).unwrap();
        assert_eq!(
            result,
            json!({"ok":true,"matched":2,"updated":2,"locked_skipped":0})
        );
        for (identifier, mut after) in snapshot(&conn) {
            let mut old = before[&identifier].clone();
            if identifier == id || identifier == zero_id {
                assert_eq!(after["category"], "居家生活");
            } else {
                assert_eq!(after["category"], old["category"]);
            }
            after.as_object_mut().unwrap().remove("category");
            old.as_object_mut().unwrap().remove("category");
            assert_eq!(after, old);
        }
        assert_eq!(db::entry(&conn, id).unwrap().unwrap().category, "居家生活");
        assert_eq!(
            db::entry(&conn, zero_id).unwrap().unwrap().category,
            "居家生活"
        );
        assert_eq!(
            store_rule(&mut conn, rule_input("自如", "居家生活", Kind::Expense)).unwrap()["updated"],
            0
        );
    }

    #[test]
    fn rule_priority_and_deletion_fallback_are_consistent_for_existing_records() {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        let mut conn = db.0.lock().unwrap();
        let mut entry = ledger::sample();
        entry.split_start = None;
        entry.split_months = None;
        entry.merchant = "测试咖啡旗舰店".into();
        entry.description = "咖啡".into();
        let id = db::insert(&conn, &entry).unwrap().unwrap();
        store_rule(&mut conn, rule_input("测试", "宽泛分类", Kind::Expense)).unwrap();
        store_rule(&mut conn, rule_input("测试咖啡", "具体分类", Kind::Expense)).unwrap();
        store_rule(&mut conn, rule_input("测试", "修改宽泛分类", Kind::Expense)).unwrap();
        assert_eq!(db::entry(&conn, id).unwrap().unwrap().category, "具体分类");
        let specific = rule_id(&conn, "测试咖啡");
        assert_eq!(delete_rule(&mut conn, specific).unwrap()["updated"], 1);
        assert_eq!(
            db::entry(&conn, id).unwrap().unwrap().category,
            "修改宽泛分类"
        );
        let broad = rule_id(&conn, "测试");
        assert_eq!(delete_rule(&mut conn, broad).unwrap()["updated"], 1);
        assert_eq!(db::entry(&conn, id).unwrap().unwrap().category, "待分类");
        assert_eq!(delete_rule(&mut conn, broad).unwrap()["updated"], 0);
        assert!(load_rules(&conn).unwrap().is_empty());
    }

    #[test]
    fn conditional_rules_coexist_override_broad_rules_and_edit_both_scopes() {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        let mut conn = db.0.lock().unwrap();
        let mut e = ledger::sample();
        e.merchant = "京东".into();
        e.description = "京东-外卖午餐".into();
        let meal = db::insert(&conn, &e).unwrap().unwrap();
        e.description = "京东-购物日用品".into();
        let goods = db::insert(&conn, &e).unwrap().unwrap();
        let mut broad = rule_input("京东", "日用购物", Kind::Expense);
        store_rule(&mut conn, broad).unwrap();
        let mut specific = rule_input("京东", "餐饮", Kind::Expense);
        specific.description_pattern = "外卖".into();
        store_rule(&mut conn, specific).unwrap();
        assert_eq!(load_rules(&conn).unwrap().len(), 2);
        assert_eq!(db::entry(&conn, meal).unwrap().unwrap().category, "餐饮");
        broad = rule_input("京东", "购物", Kind::Expense);
        store_rule(&mut conn, broad).unwrap();
        assert_eq!(db::entry(&conn, meal).unwrap().unwrap().category, "餐饮");
        let rules = load_rules(&conn).unwrap();
        assert_eq!(
            import::classify(&Kind::Expense, "京东", "外卖订单", &rules),
            "餐饮"
        );
        assert_eq!(
            import::classify(&Kind::Expense, "京东", "只有编号", &rules),
            "购物"
        );
        let specific_id = rules
            .iter()
            .find(|r| r.description_pattern == "外卖")
            .unwrap()
            .id;
        let mut edited = rule_input("京东", "日用品", Kind::Expense);
        edited.id = Some(specific_id);
        edited.description_pattern = "购物".into();
        let result = store_rule(&mut conn, edited).unwrap();
        assert_eq!(result["updated"], 2);
        assert_eq!(db::entry(&conn, meal).unwrap().unwrap().category, "购物");
        assert_eq!(db::entry(&conn, goods).unwrap().unwrap().category, "日用品");
        delete_rule(&mut conn, specific_id).unwrap();
        assert_eq!(db::entry(&conn, goods).unwrap().unwrap().category, "购物");
        let mut only_description = rule_input("", "餐饮", Kind::Expense);
        only_description.description_pattern = "外卖".into();
        store_rule(&mut conn, only_description).unwrap();
        assert_eq!(db::entry(&conn, meal).unwrap().unwrap().category, "餐饮");
    }

    #[test]
    fn fixed_categories_survive_rule_changes_and_batch_edits_are_atomic() {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        let mut conn = db.0.lock().unwrap();
        let mut e = ledger::sample();
        e.merchant = "京东".into();
        let fixed = db::insert(&conn, &e).unwrap().unwrap();
        let automatic = db::insert(&conn, &e).unwrap().unwrap();
        conn.execute(
            "UPDATE entries SET payload=json_set(payload,'$.extra','keep') WHERE id=?1",
            [fixed],
        )
        .unwrap();
        let before: String = conn
            .query_row("SELECT payload FROM entries WHERE id=?1", [fixed], |r| {
                r.get(0)
            })
            .unwrap();
        let result = categorize_selected(
            &mut conn,
            CategoryBatch {
                ids: vec![fixed],
                category: "餐饮".into(),
                category_locked: true,
            },
        )
        .unwrap();
        assert_eq!(result["updated"], 1);
        let result = store_rule(&mut conn, rule_input("京东", "购物", Kind::Expense)).unwrap();
        assert_eq!(result["locked_skipped"], 1);
        assert_eq!(db::entry(&conn, fixed).unwrap().unwrap().category, "餐饮");
        assert_eq!(
            db::entry(&conn, automatic).unwrap().unwrap().category,
            "购物"
        );
        let broad_id = rule_id(&conn, "京东");
        delete_rule(&mut conn, broad_id).unwrap();
        assert_eq!(db::entry(&conn, fixed).unwrap().unwrap().category, "餐饮");
        assert_eq!(
            db::entry(&conn, automatic).unwrap().unwrap().category,
            "待分类"
        );
        let after: String = conn
            .query_row("SELECT payload FROM entries WHERE id=?1", [fixed], |r| {
                r.get(0)
            })
            .unwrap();
        let mut before: Value = serde_json::from_str(&before).unwrap();
        let mut after: Value = serde_json::from_str(&after).unwrap();
        for value in [&mut before, &mut after] {
            value.as_object_mut().unwrap().remove("category");
            value.as_object_mut().unwrap().remove("category_locked");
        }
        assert_eq!(after, before);
        assert!(
            categorize_selected(
                &mut conn,
                CategoryBatch {
                    ids: vec![fixed, 999999],
                    category: "错误改动".into(),
                    category_locked: false
                }
            )
            .is_err()
        );
        assert_eq!(db::entry(&conn, fixed).unwrap().unwrap().category, "餐饮");
        assert!(db::entry(&conn, fixed).unwrap().unwrap().category_locked);
        categorize_selected(
            &mut conn,
            CategoryBatch {
                ids: vec![fixed],
                category: "餐饮".into(),
                category_locked: false,
            },
        )
        .unwrap();
        store_rule(&mut conn, rule_input("京东", "购物", Kind::Expense)).unwrap();
        assert_eq!(db::entry(&conn, fixed).unwrap().unwrap().category, "购物");
        assert!(
            categorize_selected(
                &mut conn,
                CategoryBatch {
                    ids: vec![fixed, fixed],
                    category: "购物".into(),
                    category_locked: true
                }
            )
            .is_err()
        );
    }

    #[test]
    fn changing_rule_kind_reclassifies_old_and_new_scopes() {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        let mut conn = db.0.lock().unwrap();
        let mut entry = ledger::sample();
        entry.merchant = "MerchantABC".into();
        entry.split_start = None;
        entry.split_months = None;
        let expense = db::insert(&conn, &entry).unwrap().unwrap();
        entry.kind = Kind::Income;
        let income = db::insert(&conn, &entry).unwrap().unwrap();
        store_rule(
            &mut conn,
            rule_input("merchant", "旧支出分类", Kind::Expense),
        )
        .unwrap();
        let mut edited = rule_input("MERCHANT", "工资", Kind::Income);
        edited.id = Some(rule_id(&conn, "merchant"));
        let result = store_rule(&mut conn, edited).unwrap();
        assert_eq!(result["updated"], 2);
        assert_eq!(load_rules(&conn).unwrap().len(), 1);
        assert_eq!(
            db::entry(&conn, expense).unwrap().unwrap().category,
            "待分类"
        );
        assert_eq!(db::entry(&conn, income).unwrap().unwrap().category, "工资");
    }

    #[test]
    fn failed_batch_rolls_back_rule_and_all_classification_changes() {
        let db =
            Database::from_connection(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
        let mut conn = db.0.lock().unwrap();
        let mut entry = ledger::sample();
        entry.merchant = "事务测试".into();
        entry.split_start = None;
        entry.split_months = None;
        db::insert(&conn, &entry).unwrap();
        entry.date = "2026-08-01".into();
        db::insert(&conn, &entry).unwrap();
        conn.execute_batch("CREATE TRIGGER reject_classification BEFORE UPDATE ON entries WHEN NEW.date='2026-08-01' BEGIN SELECT RAISE(ABORT,'synthetic-failure'); END;").unwrap();
        assert!(store_rule(&mut conn, rule_input("事务", "新的分类", Kind::Expense)).is_err());
        assert!(load_rules(&conn).unwrap().is_empty());
        assert!(
            db::entries(&conn)
                .unwrap()
                .iter()
                .all(|e| e.category == entry.category)
        );
        conn.execute_batch("DROP TRIGGER reject_classification;")
            .unwrap();
        store_rule(&mut conn, rule_input("事务", "新的分类", Kind::Expense)).unwrap();
        let id = rule_id(&conn, "事务");
        conn.execute_batch("CREATE TRIGGER reject_delete_batch BEFORE UPDATE ON entries BEGIN SELECT RAISE(ABORT,'synthetic-failure'); END;").unwrap();
        assert!(delete_rule(&mut conn, id).is_err());
        assert_eq!(load_rules(&conn).unwrap().len(), 1);
        assert!(
            db::entries(&conn)
                .unwrap()
                .iter()
                .all(|e| e.category == "新的分类")
        );
    }

    #[test]
    fn zero_records_do_not_change_income_averages_or_unclassified_totals() {
        let mut salary = ledger::sample();
        salary.kind = Kind::Income;
        salary.split_start = None;
        salary.split_months = None;
        salary.date = "2026-07-01".into();
        salary.category = "工资".into();
        let mut zero = salary.clone();
        zero.date = "2026-08-01".into();
        zero.category = "待分类".into();
        zero.amount_cents = 0;
        let mut ride = zero.clone();
        ride.kind = Kind::Expense;
        let summary = make_summary(
            &[salary.clone(), zero, ride],
            "2026-07-01",
            "2026-08",
            false,
        )
        .unwrap();
        assert_eq!(summary["income"], 0);
        assert_eq!(summary["expense"], 0);
        assert_eq!(summary["refund"], 0);
        assert_eq!(summary["unclassified"], 0);
        assert_eq!(summary["income_months"], 1);
        assert_eq!(summary["income_average"], salary.amount_cents);
        assert_eq!(summary["categories"], json!([]));
        assert_eq!(summary["count"], 2);
    }

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
            "/api/refund-targets",
            "/api/classification",
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
        expense.id = 10;
        expense.date = "2026-07-01".into();
        let mut refund = expense.clone();
        refund.kind = Kind::Refund;
        refund.split_start = None;
        refund.split_months = None;
        refund.amount_cents = 500;
        refund.id = 11;
        refund.refund_of = Some(expense.id);
        let rows = vec![salary, bonus, transfer, expense, refund];
        let cash = make_summary(&rows, "2026-07-01", "2026-07", false).unwrap();
        assert_eq!(cash["income"], 3_500_000);
        assert_eq!(cash["expense"], 14_300);
        assert_eq!(cash["pending"], 14_300);
        assert_eq!(cash["income_months"], 1);
        assert_eq!(cash["bonus"], 1_000_000);
        let accrual = make_summary(&rows, "2026-07-01", "2027-02", true).unwrap();
        assert_eq!(accrual["expense"], 1192);
    }

    #[test]
    fn refunds_restate_original_month_and_keep_auditable_zero_rows() {
        let mut expense = ledger::sample();
        expense.id = 1;
        expense.date = "2026-07-01".into();
        expense.category = "待分类".into();
        expense.refunded_cents = 14800;
        let mut refund = expense.clone();
        refund.id = 2;
        refund.date = "2026-09-01".into();
        refund.kind = Kind::Refund;
        refund.refunded_cents = 0;
        refund.split_start = None;
        refund.split_months = None;
        refund.refund_of = Some(1);
        let rows = vec![expense.clone(), refund];
        let july = make_summary(&rows, "2026-07-01", "2026-07", false).unwrap();
        assert_eq!(july["expense"], 0);
        assert_eq!(july["refund"], 14800);
        assert_eq!(july["unclassified"], 0);
        assert_eq!(july["pending"], 0);
        let september = make_summary(&rows, "2026-07-01", "2026-09", false).unwrap();
        assert_eq!(september["expense"], 0);
        assert_eq!(september["income"], 0);
        assert_eq!(september["months"][0]["expense"], 0);
        assert!(in_entry_month(&expense, "2027-02", true));
        assert!(in_entry_month(&expense, "2028-01", true));
        assert!(!in_entry_month(&expense, "2028-02", true));
        let views = entry_views(&rows).unwrap();
        assert_eq!(views[&1]["amount_cents"], 14800);
        assert_eq!(views[&1]["net_amount_cents"], 0);
        assert_eq!(views[&1]["linked_refunded_cents"], 14800);
        assert_eq!(views[&2]["net_amount_cents"], 0);
        assert_eq!(views[&2]["refund_target"]["id"], 1);
    }

    #[test]
    fn repeated_payment_statuses_keep_largest_refund_for_new_and_saved_imports() {
        for statuses in [
            ["已退款(¥20.00)", "已全额退款"],
            ["已全额退款", "已退款(¥20.00)"],
        ] {
            let mut csv =
                "交易时间,交易类型,交易对方,商品,收/支,金额(元),支付方式,当前状态,交易单号\n"
                    .to_owned();
            for status in statuses {
                csv.push_str(&format!("2026-07-01 12:00:00,商户消费,合成商户,合成商品,支出,100.00,零钱,{status},synthetic-repeat\n"));
            }
            for saved_refund in [None, Some(0), Some(2000), Some(10000)] {
                let mut parsed = import::parse("wechat.csv", csv.as_bytes(), &[]).unwrap();
                assert!(parsed.errors.is_empty());
                assert_eq!(parsed.rows.len(), 2);
                let mut existing = HashMap::new();
                if let Some(refunded) = saved_refund {
                    let mut saved = parsed.rows[0].entry.clone();
                    saved.id = 1;
                    saved.refunded_cents = refunded;
                    existing.insert(
                        (saved.source.clone(), saved.external_id.clone().unwrap()),
                        saved,
                    );
                }
                mark_saved_import_rows(&mut parsed, &existing);
                let selected: Vec<_> = parsed.rows.iter().filter(|row| !row.duplicate).collect();
                if saved_refund == Some(10000) {
                    assert!(selected.is_empty());
                } else {
                    assert_eq!(selected.len(), 1);
                    assert_eq!(selected[0].entry.refunded_cents, 10000);
                    assert_eq!(selected[0].entry.import_status, "已全额退款");
                    assert_eq!(
                        selected[0]
                            .warnings
                            .iter()
                            .any(|s| s.contains("此支出已入账")),
                        saved_refund.is_some()
                    );
                }
            }
        }
    }
}
