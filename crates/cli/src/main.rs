//! `twinbase` — run TwinBase headless (CI, scripts) and bridge MCP for coding agents.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use twinbase_core::api::call;
use twinbase_core::{Actor, App};

#[derive(Parser)]
#[command(name = "twinbase", version, about = "TwinBase: database client and wire-protocol mock servers")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Postgres,
    Mysql,
    Mssql,
    Redis,
    Mongodb,
    Dynamodb,
    Firestore,
}

#[derive(Subcommand)]
enum Command {
    /// Run the TwinBase core without the desktop app: workspace mocks (auto-start ones) and the MCP endpoint.
    Serve {
        /// Workspace folder (default: the last one used).
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// MCP/API port (default from settings, 47800).
        #[arg(long)]
        port: Option<u16>,
        /// Start every mock server in the workspace, not just auto-start ones.
        #[arg(long)]
        all_mocks: bool,
        /// Expose the JSON API for a browser-hosted UI (development).
        #[arg(long)]
        dev_api: bool,
    },
    /// Start one mock server without a workspace (ideal for tests and CI).
    Mock {
        kind: Kind,
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Keep data in this folder (default: temporary, deleted on exit).
        #[arg(long)]
        data: Option<PathBuf>,
        /// Seed to load on a fresh start: SQL, Redis commands, MongoDB Extended JSON
        /// (`{"collections": {"users": [...]}}`), DynamoDB tables (`{"tables": [...]}`) or Firestore
        /// documents (`{"documents": {"users/alice": {...}}}`).
        #[arg(long)]
        seed: Option<PathBuf>,
        /// Database to create/seed (PostgreSQL/MySQL/MongoDB).
        #[arg(long, default_value = "app")]
        database: String,
        /// PostgreSQL: run a real PostgreSQL server (an installed one, or the bin folder given)
        /// behind the mock instead of TwinLite.
        #[arg(long, num_args = 0..=1, default_missing_value = "", value_name = "BIN_DIR")]
        native: Option<PathBuf>,
        /// Require this user/password instead of accepting any credentials.
        #[arg(long, requires = "password")]
        user: Option<String>,
        #[arg(long, requires = "user")]
        password: Option<String>,
    },
    /// Bridge MCP over stdio to a running TwinBase app (for agents that launch MCP servers as programs).
    Mcp {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Print MCP setup instructions for coding agents.
    McpSetup,
    /// Export mocks and query collections to a `.twinbase` bundle.
    Export {
        #[arg(long, short)]
        out: PathBuf,
        #[arg(long = "mock")]
        mocks: Vec<String>,
        #[arg(long = "collection")]
        collections: Vec<String>,
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Only definitions and seed scripts, no data files.
        #[arg(long)]
        no_data: bool,
    },
    /// Import a `.twinbase` bundle into a workspace.
    Import {
        file: PathBuf,
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

fn app_dir() -> PathBuf {
    twinbase_core::settings::app_dir()
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Logs go to stderr so stdout stays clean for MCP.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();
    match cli.command {
        Command::Serve { workspace, port, all_mocks, dev_api } => {
            let app = App::new(app_dir(), workspace)?;
            let port = port.unwrap_or(app.settings().mcp.port);
            app.autostart_mocks().await;
            if all_mocks {
                for m in app.data().mocks {
                    if let Err(e) = app.start_mock(&m.id).await {
                        eprintln!("mock {}: {e}", m.id);
                    }
                }
            }
            let (addr, _task) = twinbase_core::http::serve(app.clone(), twinbase_core::http::HttpOptions { port, dev_api })
                .await
                .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
            eprintln!("TwinBase core running · workspace {} · MCP http://{addr}/mcp", app.workspace_key());
            for m in app.mock_statuses().await.into_iter().filter(|m| m.running) {
                eprintln!("  {} ({:?}) on {}:{}", m.def.name, m.def.kind, m.def.host, m.def.port);
            }
            if dev_api {
                eprintln!("  dev API token: {}", app.settings().mcp.token);
            }
            tokio::signal::ctrl_c().await?;
            app.stop_all_mocks().await;
        }
        Command::Mock { kind, port, host, data, seed, database, user, password, native } => {
            let kind = match kind {
                Kind::Postgres => twinbase_mock::MockKind::Postgres,
                Kind::Mysql => twinbase_mock::MockKind::Mysql,
                Kind::Mssql => twinbase_mock::MockKind::Mssql,
                Kind::Redis => twinbase_mock::MockKind::Redis,
                Kind::Mongodb => twinbase_mock::MockKind::Mongodb,
                Kind::Dynamodb => twinbase_mock::MockKind::Dynamodb,
                Kind::Firestore => twinbase_mock::MockKind::Firestore,
            };
            let auth = match (user, password) {
                (Some(user), Some(password)) => twinbase_mock::AuthMode::Password { user, password },
                _ => twinbase_mock::AuthMode::Any,
            };
            let fresh = data.as_ref().is_none_or(|d| !d.exists());
            if native.is_some() && kind != twinbase_mock::MockKind::Postgres {
                bail!("--native is only for PostgreSQL mocks");
            }
            let native = native.map(|bin| twinbase_mock::NativeEngine { bin: Some(bin).filter(|b| !b.as_os_str().is_empty()) });
            let handle = twinbase_mock::start(twinbase_mock::MockConfig {
                kind,
                host,
                port,
                data_dir: data,
                auth,
                faults: Default::default(),
                native,
            })
            .await?;
            if let twinbase_mock::Backend::NativePg(server) = &handle.backend {
                let seeded = match (&seed, fresh) {
                    (Some(p), true) => {
                        twinbase_mock::native_pg::seed(
                            server,
                            &database,
                            &std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?,
                        )
                        .await
                    }
                    _ => server.ensure("postgres", &database).await,
                };
                if let Err(e) = seeded {
                    handle.stop().await;
                    bail!("seed failed: {e}");
                }
                eprintln!("PostgreSQL {} engine", server.bins.version);
            } else if let twinbase_mock::Backend::Sql(engine) = &handle.backend {
                let engine = engine.clone();
                let seed_sql = match (&seed, fresh) {
                    (Some(p), true) => Some(std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?),
                    _ => None,
                };
                let db = database.clone();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    if kind == twinbase_mock::MockKind::Mysql {
                        let mut s = engine.session(None, "root")?;
                        if let Some(e) = s.execute(&format!("CREATE DATABASE IF NOT EXISTS `{db}`")).error {
                            bail!("{e}");
                        }
                    }
                    if let Some(sql) = seed_sql {
                        let mut s = engine.session(Some(&db), "twinbase")?;
                        if let Some(e) = s.execute(&sql).error {
                            bail!("seed failed: {}", e.message);
                        }
                    }
                    Ok(())
                })
                .await??;
            } else if let (twinbase_mock::Backend::Firestore(store), Some(p), true) = (&handle.backend, &seed, fresh) {
                let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                let n = twinbase_mock::firestore::apply_seed(store, &text).map_err(|e| anyhow::anyhow!("seed failed: {e}"))?;
                eprintln!("seeded {n} documents");
            } else if let (twinbase_mock::Backend::Dynamo(store), Some(p), true) = (&handle.backend, &seed, fresh) {
                let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                let n = twinbase_mock::dynamo::apply_seed(store, &text).map_err(|e| anyhow::anyhow!("seed failed: {e}"))?;
                eprintln!("seeded {n} items");
            } else if let (twinbase_mock::Backend::Mongo(store), Some(p), true) = (&handle.backend, &seed, fresh) {
                let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                let n = twinbase_mock::mongo::apply_seed(store, &database, &text).map_err(|e| anyhow::anyhow!("seed failed: {e}"))?;
                eprintln!("seeded {n} documents into {database}");
            } else if let (twinbase_mock::Backend::Redis(store), Some(p), true) = (&handle.backend, &seed, fresh) {
                let text = std::fs::read_to_string(p)?;
                let mut db = 0;
                for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                    let args: Vec<Vec<u8>> = twinbase_core::split_redis_line(line);
                    store.execute(&mut db, &args);
                }
            }
            // Machine-readable line first, for scripts.
            println!(
                "{}",
                json!({ "kind": format!("{kind:?}").to_lowercase(), "address": handle.addr.to_string(), "port": handle.addr.port() })
            );
            eprintln!("{kind:?} mock listening on {} (Ctrl+C to stop)", handle.addr);
            tokio::signal::ctrl_c().await?;
            handle.stop().await;
        }
        Command::Mcp { url, token } => {
            let settings = twinbase_core::settings::LocalStore::new(app_dir()).load_settings();
            let url = url.unwrap_or_else(|| format!("http://127.0.0.1:{}/mcp", settings.mcp.port));
            let token = token.unwrap_or(settings.mcp.token);
            twinbase_core::mcp::stdio_bridge(&url, &token).await?;
        }
        Command::McpSetup => {
            let settings = twinbase_core::settings::LocalStore::new(app_dir()).load_settings();
            let info = twinbase_core::mcp::install_instructions(&settings);
            for a in info["agents"].as_array().unwrap() {
                println!("## {}\n{}\n\n{}\n", a["name"].as_str().unwrap(), a["steps"].as_str().unwrap(), a["config"].as_str().unwrap());
            }
        }
        Command::Export { out, mocks, collections, workspace, no_data } => {
            let app = App::new(app_dir(), workspace)?;
            let manifest = call(
                &app,
                "bundle.export",
                json!({ "path": out.to_string_lossy(), "selection": { "mocks": mocks, "collections": collections, "include_data": !no_data } }),
                &Actor::Cli,
            )
            .await
            .map_err(|e| anyhow::anyhow!(e.message))?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        Command::Import { file, workspace } => {
            let app = App::new(app_dir(), workspace)?;
            let summary = call(&app, "bundle.import", json!({ "path": file.to_string_lossy() }), &Actor::Cli)
                .await
                .map_err(|e| anyhow::anyhow!(e.message))?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
    }
    Ok(())
}
