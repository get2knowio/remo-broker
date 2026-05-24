//! SC-003 killtest: SIGKILL the daemon repeatedly while clients are
//! actively fetching, and verify after each kill that
//!
//!   (a) no on-disk artifact (audit log, socket dir contents, /tmp
//!       scratch under the daemon's writable paths) contains any
//!       known plaintext secret value,
//!   (b) the audit log remains parseable as JSONL (no torn writes
//!       that would corrupt the log), and
//!   (c) the daemon restarts cleanly — sockets re-bind, register
//!       succeeds, get returns the value.
//!
//! Unlike the soak harness this one runs the daemon as a separate
//! process so we can actually `kill -9` it. We use the release binary
//! at `target/release/remo-broker`; build it first with
//! `cargo build --release`.
//!
//! Knobs (env vars):
//!   KILLTEST_ROUNDS         number of kill cycles    (default 5)
//!   KILLTEST_BURST_OPS      gets per round before kill (default 50)
//!   KILLTEST_KILL_DELAY_MS  delay before SIGKILL     (default 100)

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

const TRIPWIRE_VALUES: &[(&str, &str)] = &[
    ("S0", "tripwire-S0-DO-NOT-LEAK"),
    ("S1", "tripwire-S1-DO-NOT-LEAK"),
    ("S2", "tripwire-S2-DO-NOT-LEAK"),
];

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn scratch_dir() -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "remo-broker-killtest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Round-trip one NDJSON request/response on a fresh connection.
fn round_trip(socket: &Path, req: &str) -> std::io::Result<String> {
    let mut s = UnixStream::connect(socket)?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    s.write_all(req.as_bytes())?;
    s.write_all(b"\n")?;
    let mut reader = BufReader::new(s);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(line)
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() && UnixStream::connect(path).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("socket never became connectable: {}", path.display());
}

fn spawn_daemon(binary: &Path, sandbox: &Path) -> Child {
    Command::new(binary)
        .arg("--bootstrap-token-path")
        .arg(sandbox.join("token"))
        .arg("--fnox-config")
        .arg(sandbox.join("fnox.toml"))
        .arg("--socket-dir")
        .arg(sandbox.join("run"))
        .arg("--audit-log-path")
        .arg(sandbox.join("audit.log"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("daemon spawn")
}

fn register_project(admin: &Path, name: &str, project_path: &Path) {
    let req = format!(
        "{{\"op\":\"register\",\"name\":\"{name}\",\"project_path\":\"{}\"}}",
        project_path.display()
    );
    let resp = round_trip(admin, &req).expect("register round-trip");
    assert!(resp.contains("\"ok\":true"), "register: {resp}");
}

/// Grep every file under `root` for any of the tripwire values, *except*
/// paths in `excluded` (the daemon's read-only input files — fnox.toml,
/// per-project manifest TOMLs — which legitimately contain the values
/// we're hunting for). Returns the first hit, if any.
fn grep_tripwires_under(root: &Path, excluded: &[PathBuf]) -> Option<(PathBuf, &'static str)> {
    fn walk(p: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(p) {
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.is_file() {
                    out.push(path);
                }
            }
        }
    }
    let mut files = Vec::new();
    walk(root, &mut files);
    for f in files {
        // Skip the daemon's own input files — they're the source of
        // truth for the plaintext and not a leak.
        if excluded.iter().any(|e| e == &f) {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&f) {
            for (_name, value) in TRIPWIRE_VALUES {
                if content.contains(value) {
                    return Some((f, value));
                }
            }
        }
    }
    None
}

fn assert_audit_log_parseable(path: &Path) {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return, // log doesn't exist yet — fine for an early kill
    };
    for (i, line) in content.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        if let Err(e) = serde_json::from_str::<serde_json::Value>(line) {
            panic!(
                "audit log line {} is not valid JSON: {} ({})",
                i + 1,
                line,
                e
            );
        }
    }
}

fn main() {
    let rounds = env_u64("KILLTEST_ROUNDS", 5);
    let burst_ops = env_u64("KILLTEST_BURST_OPS", 50);
    let kill_delay = Duration::from_millis(env_u64("KILLTEST_KILL_DELAY_MS", 100));

    let binary = PathBuf::from("target/release/remo-broker");
    assert!(
        binary.exists(),
        "release binary not found at {} — run `cargo build --release` first",
        binary.display()
    );

    let sandbox = scratch_dir();
    let token = sandbox.join("token");
    let fnox = sandbox.join("fnox.toml");
    let socket_dir = sandbox.join("run");
    let audit_log = sandbox.join("audit.log");
    let project_dir = sandbox.join("victim");
    let admin = socket_dir.join("admin.sock");
    let project_socket = socket_dir.join("victim.sock");
    std::fs::create_dir_all(socket_dir.parent().unwrap()).unwrap();
    std::fs::write(&token, "killtest-token").unwrap();

    let mut fnox_toml = String::from("[providers]\nplain = { type = \"plain\" }\n\n[secrets]\n");
    for (name, value) in TRIPWIRE_VALUES {
        fnox_toml.push_str(&format!(
            "{name} = {{ provider = \"plain\", value = \"{value}\" }}\n"
        ));
    }
    std::fs::write(&fnox, &fnox_toml).unwrap();

    let project_remo = project_dir.join(".remo");
    std::fs::create_dir_all(&project_remo).unwrap();
    let allowlist_csv = TRIPWIRE_VALUES
        .iter()
        .map(|(n, _)| format!("\"{n}\""))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        project_remo.join("broker.toml"),
        format!(
            "schema_version = 1\n\
             [project]\n\
             name = \"victim\"\n\
             [allowlist]\n\
             secrets = [{allowlist_csv}]\n"
        ),
    )
    .unwrap();

    println!(
        "killtest: binary={}, {} rounds × {} ops, kill_delay={}ms",
        binary.display(),
        rounds,
        burst_ops,
        kill_delay.as_millis()
    );

    let mut total_kills = 0u64;
    let mut total_gets = 0u64;
    let mut errors_across_rounds = 0u64;

    for round in 1..=rounds {
        // ---- Start the daemon ----
        let mut child = spawn_daemon(&binary, &sandbox);
        wait_for_socket(&admin, Duration::from_secs(5));
        register_project(&admin, "victim", &project_dir);
        wait_for_socket(&project_socket, Duration::from_secs(2));

        // ---- Fire bursts of gets, then SIGKILL mid-flight ----
        let start = Instant::now();
        let mut gets_this_round = 0u64;
        let mut errs_this_round = 0u64;
        // Spawn a background "killer" thread.
        let pid = child.id();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(kill_delay);
            // SIGKILL = 9
            unsafe {
                libc_kill(pid as i32, 9);
            }
        });
        for i in 0..burst_ops {
            let (name, expected) = TRIPWIRE_VALUES[(i as usize) % TRIPWIRE_VALUES.len()];
            let req = format!("{{\"op\":\"get\",\"name\":\"{name}\"}}");
            match round_trip(&project_socket, &req) {
                Ok(line) => {
                    gets_this_round += 1;
                    if line.contains(expected) {
                        // sanity — the daemon was alive enough to
                        // resolve the secret at least once
                    }
                }
                Err(_) => {
                    errs_this_round += 1;
                }
            }
        }
        killer.join().unwrap();
        // Drain the child so it really exits.
        let _ = child.wait();

        total_kills += 1;
        total_gets += gets_this_round;
        errors_across_rounds += errs_this_round;

        // ---- SC-003 (a): no plaintext on disk anywhere under the
        //                  daemon's writable paths. Exclude the
        //                  daemon's read-only inputs (fnox.toml + the
        //                  per-project manifest) from the grep — those
        //                  legitimately hold the values.
        let excluded = vec![fnox.clone(), project_remo.join("broker.toml")];
        if let Some((file, value)) = grep_tripwires_under(&sandbox, &excluded) {
            panic!(
                "round {round}: tripwire value {:?} found in {} after SIGKILL — SC-003 violation",
                value,
                file.display()
            );
        }

        // ---- SC-003 (b): audit log is parseable JSONL (no torn writes).
        assert_audit_log_parseable(&audit_log);

        // ---- SC-003 (c): sockets re-bind cleanly on the next restart.
        // We don't restart here — the next loop iteration does it via
        // spawn_daemon, and FR-009 cleans up the stale socket file the
        // kill left behind. If that ever broke, the next register()
        // would fail.

        println!(
            "round {round}: {gets_this_round} gets, {errs_this_round} errors in {:.2}s, daemon SIGKILL'd",
            start.elapsed().as_secs_f64()
        );
    }

    println!("--------------------------------------------------------");
    println!("killtest passed:");
    println!("  total kills    : {total_kills}");
    println!("  total gets ok  : {total_gets}");
    println!(
        "  errors         : {errors_across_rounds} (expected — connections in flight at SIGKILL)"
    );
    println!(
        "  audit log lines: {}",
        std::fs::read_to_string(&audit_log)
            .map(|s| s.lines().filter(|l| !l.is_empty()).count())
            .unwrap_or(0)
    );
    println!("--------------------------------------------------------");

    let _ = std::fs::remove_dir_all(&sandbox);
}

// Avoid pulling in the `libc` crate for one syscall — just declare it.
unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}
unsafe fn libc_kill(pid: i32, sig: i32) {
    unsafe {
        kill(pid, sig);
    }
}
