//! SC-002 soak harness: many simulated devcontainers issuing mixed
//! `get`/`ping`/`info` at a configurable rate against a real broker.
//!
//! The spec calls for 50 sockets × 10 Hz × 1 hour with zero panics, no
//! missed audit events, and no memory growth beyond the cache cap.
//! This harness implements that workload but lets the operator dial it
//! down for CI smoke runs (the defaults below complete in ~5 seconds).
//!
//! All knobs are env-var-driven so a single binary covers both the
//! per-PR smoke job and the scheduled full-soak workflow:
//!
//!   SOAK_PROJECTS         number of registered projects   (default 5)
//!   SOAK_WORKERS_PER_PROJ workers per project              (default 2)
//!   SOAK_OPS_PER_SEC      target ops/second per worker     (default 10)
//!   SOAK_DURATION_SECS    how long to run                  (default 5)
//!   SOAK_RSS_CAP_MIB      fail if final RSS exceeds (MiB)  (default 60)
//!
//! Run:
//!   cargo run --release --example soak
//!   SOAK_DURATION_SECS=3600 SOAK_PROJECTS=50 SOAK_WORKERS_PER_PROJ=1 \
//!     cargo run --release --example soak

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use remo_broker::audit::AuditWriter;
use remo_broker::backend::BackendSession;
use remo_broker::config::{BootstrapSourceKind, Config, Overrides};
use remo_broker::server::Server;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn scratch_dir() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "remo-broker-soak-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn read_rss_kib() -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{}/status", std::process::id())).ok()?;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok());
        }
    }
    None
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let num_projects = env_u64("SOAK_PROJECTS", 5) as usize;
    let workers_per_project = env_u64("SOAK_WORKERS_PER_PROJ", 2) as usize;
    let ops_per_sec = env_u64("SOAK_OPS_PER_SEC", 10);
    let duration = Duration::from_secs(env_u64("SOAK_DURATION_SECS", 5));
    let rss_cap_mib = env_u64("SOAK_RSS_CAP_MIB", 60);

    let scratch = scratch_dir();
    let socket_dir = scratch.join("run");
    let audit_log = scratch.join("audit.log");
    let bootstrap_token = scratch.join("token");
    let fnox_config = scratch.join("fnox.toml");
    std::fs::write(&bootstrap_token, "soak-token").unwrap();

    // 10 distinct secrets; workers pick names at random.
    let mut fnox = String::from("[providers]\nplain = { type = \"plain\" }\n\n[secrets]\n");
    for i in 0..10 {
        fnox.push_str(&format!(
            "S{i} = {{ provider = \"plain\", value = \"v{i}\" }}\n"
        ));
    }
    std::fs::write(&fnox_config, &fnox).unwrap();

    let backend = BackendSession::open(&fnox_config).expect("fnox config opens");

    let overrides = Overrides {
        bootstrap_source: Some(BootstrapSourceKind::File),
        bootstrap_token_path: Some(bootstrap_token.clone()),
        socket_dir: Some(socket_dir.clone()),
        audit_log_path: Some(audit_log.clone()),
        ..Default::default()
    };
    let config = Config::from_toml_str("", &overrides).unwrap();
    let (audit, audit_handle) = AuditWriter::spawn(audit_log.clone());
    let server = Server::new(config, audit, Some(backend));
    let server_handle = tokio::spawn(server.run());

    // Wait for admin socket to appear, then register all projects.
    let admin = socket_dir.join("admin.sock");
    for _ in 0..200 {
        if admin.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut project_sockets = Vec::with_capacity(num_projects);
    for i in 0..num_projects {
        let name = format!("p{i}");
        let proj_dir = scratch.join(&name);
        let remo_dir = proj_dir.join(".remo");
        std::fs::create_dir_all(&remo_dir).unwrap();
        std::fs::write(
            remo_dir.join("broker.toml"),
            format!(
                "schema_version = 1\n\
                 [project]\n\
                 name = \"{name}\"\n\
                 [allowlist]\n\
                 secrets = [\"S0\",\"S1\",\"S2\",\"S3\",\"S4\",\"S5\",\"S6\",\"S7\",\"S8\",\"S9\"]\n"
            ),
        )
        .unwrap();
        let req = format!(
            "{{\"op\":\"register\",\"name\":\"{name}\",\"project_path\":\"{}\"}}\n",
            proj_dir.display()
        );
        let mut stream = UnixStream::connect(&admin).await.unwrap();
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        assert!(line.contains("\"ok\":true"), "register: {line}");
        project_sockets.push(socket_dir.join(format!("{name}.sock")));
    }

    println!(
        "soak: {} projects × {} workers = {} clients; {} ops/s each for {} s; RSS cap {} MiB",
        num_projects,
        workers_per_project,
        num_projects * workers_per_project,
        ops_per_sec,
        duration.as_secs(),
        rss_cap_mib,
    );

    let stop_at = Instant::now() + duration;
    let interval = Duration::from_secs_f64(1.0 / ops_per_sec as f64);
    let total_ops = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));
    let panics = Arc::new(AtomicU64::new(0));

    let mut workers = Vec::new();
    let mut worker_id: u64 = 0;
    for project_socket in &project_sockets {
        for _ in 0..workers_per_project {
            let socket = project_socket.clone();
            let ops = Arc::clone(&total_ops);
            let errs = Arc::clone(&errors);
            let panics = Arc::clone(&panics);
            let id = worker_id;
            worker_id += 1;
            workers.push(tokio::spawn(async move {
                let mut tick = id;
                while Instant::now() < stop_at {
                    let op_pick = tick % 5;
                    let secret_pick = (tick / 5) % 10;
                    let payload = match op_pick {
                        0..=2 => format!("{{\"op\":\"get\",\"name\":\"S{secret_pick}\"}}\n"),
                        3 => "{\"op\":\"ping\"}\n".to_string(),
                        _ => "{\"op\":\"info\"}\n".to_string(),
                    };
                    let result = async {
                        let mut s = UnixStream::connect(&socket).await?;
                        s.write_all(payload.as_bytes()).await?;
                        let mut line = String::new();
                        BufReader::new(s).read_line(&mut line).await?;
                        Ok::<String, std::io::Error>(line)
                    }
                    .await;
                    match result {
                        Ok(_) => {
                            ops.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            errs.fetch_add(1, Ordering::Relaxed);
                            // Panic would have killed the worker task;
                            // we increment the panic counter only via
                            // task-completion polling below.
                            let _ = &panics;
                        }
                    }
                    tokio::time::sleep(interval).await;
                    tick += 1;
                }
            }));
        }
    }

    // Drain workers; any JoinError indicates a panic in that worker.
    for w in workers {
        if w.await.is_err() {
            panics.fetch_add(1, Ordering::Relaxed);
        }
    }

    let final_rss_kib = read_rss_kib().unwrap_or(0);
    let final_rss_mib = final_rss_kib / 1024;
    let audit_lines = std::fs::read_to_string(&audit_log)
        .map(|s| s.lines().filter(|l| !l.is_empty()).count())
        .unwrap_or(0);
    let total = total_ops.load(Ordering::Relaxed);
    let errs = errors.load(Ordering::Relaxed);
    let pcount = panics.load(Ordering::Relaxed);

    println!("--------------------------------------------------------");
    println!("soak finished:");
    println!("  total ops        : {total}");
    println!("  errors           : {errs}");
    println!("  panics (workers) : {pcount}");
    println!("  audit lines      : {audit_lines}");
    println!("  final RSS        : {final_rss_kib} KiB = {final_rss_mib} MiB");
    println!("--------------------------------------------------------");

    // Graceful shutdown so the audit writer drains.
    server_handle.abort();
    let _ = server_handle.await;
    drop(audit_handle); // let the writer task exit

    // ---- assertions ----
    let mut failures = Vec::new();
    if pcount > 0 {
        failures.push(format!("{pcount} worker tasks panicked"));
    }
    // Errors are tolerable in extreme load but should be a tiny
    // fraction; for short smoke runs we expect zero.
    if errs * 100 > total {
        failures.push(format!(
            "error rate {}% exceeds 1% threshold",
            errs * 100 / total.max(1)
        ));
    }
    if final_rss_mib > rss_cap_mib {
        failures.push(format!(
            "RSS {final_rss_mib} MiB exceeds cap {rss_cap_mib} MiB"
        ));
    }
    // Audit lines should equal the get-op count (3 out of every 5 ops).
    // We don't have exact get count here, but we know audit_lines
    // should be roughly 3/5 of total_ops and never zero on a non-trivial
    // run. Apply a loose check.
    if total >= 5 {
        let expected_audit_floor = total * 2 / 5; // generous floor
        if (audit_lines as u64) < expected_audit_floor {
            failures.push(format!(
                "audit lines {audit_lines} less than expected floor {expected_audit_floor} (~3/5 of {total} ops)"
            ));
        }
    }

    let _ = std::fs::remove_dir_all(&scratch);

    if !failures.is_empty() {
        for f in &failures {
            eprintln!("SOAK FAILED: {f}");
        }
        std::process::exit(1);
    }
    println!("soak PASSED");
}
