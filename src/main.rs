use std::{
    io::{self, Read},
    path::PathBuf,
};
use tally::{
    auth,
    db::Database,
    server::{self, AppState},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(4)
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path =
        PathBuf::from(std::env::var("TALLY_DB").unwrap_or_else(|_| "data/tally.sqlite3".into()));
    let command = args.first().map(String::as_str).unwrap_or("serve");
    if matches!(command, "--help" | "-h" | "help") {
        println!(
            "Tally 拾账\n\n  tally init <用户名> [--password-stdin]  创建或重置私人账户\n  tally serve [--dev]                  启动服务；--dev 仅限本机 HTTP 开发\n  tally backup <目标文件>              创建一致的 SQLite 备份\n\n环境变量：TALLY_DB（默认 data/tally.sqlite3）、TALLY_BIND（默认 127.0.0.1:3080）"
        );
        return Ok(());
    }
    let db = Database::open(&path)?;
    match command {
        "init" => {
            let username = args
                .get(1)
                .filter(|s| !s.starts_with('-'))
                .ok_or("用法：tally init <用户名>")?;
            let password = if args.iter().any(|a| a == "--password-stdin") {
                let mut s = String::new();
                io::stdin().take(4096).read_to_string(&mut s)?;
                s.trim_end_matches(['\n', '\r']).to_owned()
            } else {
                rpassword::prompt_password("设置密码（至少 12 个字符）：")?
            };
            let mut conn = db.0.lock().map_err(|_| "数据库锁异常")?;
            auth::set_owner(&mut conn, username, &password)?;
            println!("账户已保存，旧会话已撤销。");
        }
        "serve" => {
            let configured = db
                .run(|c| {
                    c.query_row("SELECT EXISTS(SELECT 1 FROM owner)", [], |r| {
                        r.get::<_, bool>(0)
                    })
                    .map_err(|e| e.to_string())
                })
                .await?;
            if !configured {
                return Err("请先运行 tally init <用户名> 设置私人账户".into());
            }
            let bind: std::net::SocketAddr = std::env::var("TALLY_BIND")
                .unwrap_or_else(|_| "127.0.0.1:3080".into())
                .parse()?;
            let dev = args.iter().any(|a| a == "--dev");
            if dev && !bind.ip().is_loopback() {
                return Err("--dev 只允许监听本机回环地址".into());
            }
            let listener = tokio::net::TcpListener::bind(bind).await?;
            println!(
                "拾账已启动：http://{bind}（{}）",
                if dev {
                    "本机开发"
                } else {
                    "请通过 HTTPS 反向代理访问"
                }
            );
            axum::serve(listener, server::router(AppState::new(db, !dev)))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
        }
        "backup" => {
            let destination = args.get(1).ok_or("用法：tally backup <目标文件>")?;
            let dest = PathBuf::from(destination);
            if dest.exists() {
                return Err("备份目标已存在，请使用新文件名".into());
            }
            let target = Database::open(&dest)?;
            let conn = db.0.lock().map_err(|_| "数据库锁异常")?;
            let mut target = target.0.lock().map_err(|_| "数据库锁异常")?;
            let backup = rusqlite::backup::Backup::new(&conn, &mut target)?;
            backup.run_to_completion(64, std::time::Duration::from_millis(10), None)?;
            println!("备份完成：{}", dest.display());
        }
        _ => return Err("未知命令，运行 tally --help 查看用法".into()),
    }
    Ok(())
}
