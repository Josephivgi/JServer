//! Background recorder. Runs as `JServer.exe --collector`, keeps one SSH login
//! open per server in ~/.ssh/config, samples each every few seconds and rolls
//! usage per service / container / VM up into one row per minute.

use crate::probe::{self, Info, Tracker};
use crate::remote::{Failure, Session};
use crate::{db, paths, sshconf};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::{Duration, Instant};

/// Groups below all of these in a minute are not stored, to keep the database small.
const MIN_CPU_PCT: f64 = 0.05;
const MIN_MEM_BYTES: u64 = 20 * 1024 * 1024;
const MAX_GROUPS_PER_MINUTE: usize = 60;

pub fn log(msg: &str) {
    let path = paths::data_dir().join("collector.log");
    if std::fs::metadata(&path).map(|m| m.len() > 512 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, paths::data_dir().join("collector.old.log"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {msg}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}

fn open_lock() -> std::io::Result<File> {
    File::options().read(true).write(true).create(true).truncate(false).open(paths::lock_path())
}

/// True if a collector process currently holds the lock file.
pub fn is_running() -> bool {
    let Ok(f) = open_lock() else {
        return false;
    };
    match f.try_lock() {
        Ok(()) => {
            let _ = f.unlock();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// Starts a detached collector process if one is not already running.
pub fn spawn_detached() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    if is_running() {
        return Ok(());
    }
    std::process::Command::new(std::env::current_exe()?)
        .arg("--collector")
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}

struct Worker {
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

pub fn run() -> Result<(), String> {
    let lock = open_lock().map_err(|e| e.to_string())?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
        Err(std::fs::TryLockError::Error(e)) => return Err(format!("lock failed: {e}")),
    }
    let conn = db::open(&paths::db_path()).map_err(|e| e.to_string())?;
    db::mark_collector_started(&conn, std::process::id(), now()).map_err(|e| e.to_string())?;
    log(&format!("collector started, pid {}", std::process::id()));

    let retry = Arc::new(AtomicI64::new(db::setting_i64(&conn, db::KEY_RETRY, 0)));
    let mut workers: HashMap<String, Worker> = HashMap::new();
    let mut last_config: Option<Instant> = None;
    let mut aliases: Vec<String> = Vec::new();
    let mut last_prune = Instant::now() - Duration::from_secs(3600);

    loop {
        let _ = db::heartbeat(&conn, now());
        if !db::monitoring_enabled(&conn) {
            log("monitoring turned off, collector exiting");
            break;
        }
        let generation = db::setting_i64(&conn, db::KEY_RETRY, 0);
        let bumped = generation != retry.swap(generation, Ordering::SeqCst);
        if bumped || last_config.is_none_or(|t| t.elapsed() >= Duration::from_secs(60)) {
            last_config = Some(Instant::now());
            aliases = sshconf::aliases();
            if let Err(e) = db::ensure_servers(&conn, &aliases).and_then(|_| db::forget_servers_not_in(&conn, &aliases)) {
                log(&format!("server list update failed: {e}"));
            }
        }
        let enabled: HashMap<String, bool> = db::servers(&conn).unwrap_or_default().into_iter().map(|s| (s.alias, s.enabled)).collect();
        // Stop workers for servers that were paused or removed from the config.
        let gone: Vec<String> = workers
            .iter()
            .filter(|(a, w)| !aliases.contains(a) || !enabled.get(*a).copied().unwrap_or(true) || w.handle.is_finished())
            .map(|(a, _)| a.clone())
            .collect();
        for a in gone {
            if let Some(w) = workers.remove(&a) {
                w.stop.store(true, Ordering::SeqCst);
                if !enabled.get(&a).copied().unwrap_or(true) {
                    let _ = db::set_status(&conn, &a, "paused", "Paused", "", now());
                }
            }
        }
        let statuses: HashMap<String, String> = db::servers(&conn).unwrap_or_default().into_iter().map(|s| (s.alias, s.status)).collect();
        for a in &aliases {
            if !enabled.get(a).copied().unwrap_or(true) && statuses.get(a).is_some_and(|s| s != "paused") && !workers.contains_key(a) {
                let _ = db::set_status(&conn, a, "paused", "Paused", "", now());
            }
            if enabled.get(a).copied().unwrap_or(true) && !workers.contains_key(a) {
                let stop = Arc::new(AtomicBool::new(false));
                let (alias, s, r) = (a.clone(), stop.clone(), retry.clone());
                let handle = std::thread::spawn(move || worker(&alias, &s, &r));
                workers.insert(a.clone(), Worker { stop, handle });
            }
        }
        if last_prune.elapsed() >= Duration::from_secs(3600) {
            last_prune = Instant::now();
            let days = db::setting_i64(&conn, db::KEY_RETENTION, db::DEFAULT_RETENTION).max(1);
            match db::prune(&conn, now() - days * 86_400) {
                Ok(n) if n > 0 => log(&format!("removed {n} rows older than {days} days")),
                Err(e) => log(&format!("prune failed: {e}")),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    for w in workers.values() {
        w.stop.store(true, Ordering::SeqCst);
    }
    for (_, w) in workers {
        let _ = w.handle.join();
    }
    drop(lock);
    Ok(())
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn now_f() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

/// After a failed login, waits until the user presses Retry (on this server)
/// or Refresh (all servers). Nothing is retried on a timer: a rejected key or
/// an unreachable host will not fix itself, and retrying a rejected key over
/// and over can get you banned by fail2ban.
fn wait_for_retry(conn: &rusqlite::Connection, alias: &str, stop: &AtomicBool, retry: &AtomicI64) -> bool {
    let all = retry.load(Ordering::SeqCst);
    let mine = db::server_retry(conn, alias);
    loop {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        if retry.load(Ordering::SeqCst) != all || db::server_retry(conn, alias) != mine {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[derive(Default)]
struct Acc {
    cpu_sum: f64,
    cpu_max: f64,
    mem_sum: u64,
    mem_max: u64,
    procs: i64,
    group: crate::describe::Group,
}

fn worker(alias: &str, stop: &AtomicBool, retry: &AtomicI64) {
    let mut conn = match db::open(&paths::db_path()) {
        Ok(c) => c,
        Err(e) => {
            log(&format!("{alias}: could not open database: {e}"));
            return;
        }
    };
    // A connection that was working and dropped gets one reconnect straight away.
    let mut reconnect = false;
    while !stop.load(Ordering::SeqCst) {
        let _ = db::set_status(&conn, alias, "connecting", "Connecting", "", now());
        let login = Session::connect(alias, probe::SETUP).and_then(|(mut s, _, ms)| {
            let info = probe::parse_info(&s.exec(probe::INFO, Duration::from_secs(40))?);
            Ok((s, ms, info))
        });
        let (mut session, login_ms, info) = match login {
            Ok(v) => v,
            Err(f) => {
                let _ = db::set_status(&conn, alias, f.status, f.label, &f.detail, now());
                log(&format!("{alias}: {} ({}), waiting for Retry", f.label, f.detail));
                reconnect = false;
                if !wait_for_retry(&conn, alias, stop, retry) {
                    break;
                }
                continue;
            }
        };
        let _ = db::set_connected(&conn, alias, &info, login_ms, now());
        log(&format!("{alias}: connected in {login_ms} ms ({}, {} cores){}", info.os, info.cores, if reconnect { " after the connection dropped" } else { "" }));
        match monitor(alias, &mut conn, &mut session, info, stop) {
            Ok(()) => break,
            Err(f) => {
                let _ = db::set_status(&conn, alias, f.status, f.label, &f.detail, now());
                log(&format!("{alias}: {} during monitoring ({})", f.label, f.detail));
                drop(session);
                if reconnect {
                    // It dropped again right after reconnecting: stop and wait for Retry.
                    reconnect = false;
                    if !wait_for_retry(&conn, alias, stop, retry) {
                        break;
                    }
                } else {
                    reconnect = true;
                }
            }
        }
    }
}

/// Samples until stopped (Ok) or the connection fails (Err).
fn monitor(alias: &str, conn: &mut rusqlite::Connection, session: &mut Session, mut info: Info, stop: &AtomicBool) -> Result<(), Failure> {
    let mut tracker = Tracker::default();
    let mut info_at = Instant::now();
    let mut minute_acc: HashMap<String, Acc> = HashMap::new();
    let mut minute_samples = 0u32;
    let mut current_minute: Option<i64> = None;
    let mut first = true;
    let interval = Duration::from_secs(db::INTERVAL);
    while !stop.load(Ordering::SeqCst) {
        let started = Instant::now();
        if info_at.elapsed() >= Duration::from_secs(600) {
            info_at = Instant::now();
            info = probe::parse_info(&session.exec(probe::INFO, Duration::from_secs(40))?);
            let _ = conn.execute(
                "UPDATE servers SET reboot = ?2, docker_note = ?3, os = ?4, kernel = ?5 WHERE alias = ?1",
                rusqlite::params![alias, info.reboot_required as i64, info.docker_note, info.os, info.kernel],
            );
        }
        let lines = session.exec(probe::SAMPLE, Duration::from_secs(30))?;
        let missing = tracker.missing_meta(&lines);
        if !missing.is_empty() {
            for chunk in missing.chunks(150) {
                let meta = session.exec(&probe::meta_script(chunk), Duration::from_secs(30))?;
                tracker.add_meta(&meta, &lines);
            }
        }
        let ts = now();
        let sample = tracker.sample(&lines, &info, now_f(), ts);
        // The first sample has no previous counters, so it has no rates yet.
        if first {
            first = false;
        } else {
            if let Err(e) = db::write_sample(conn, alias, &sample) {
                log(&format!("{alias}: write_sample failed: {e}"));
            }
            let minute = ts - ts.rem_euclid(60);
            if let Some(m) = current_minute.filter(|m| *m != minute) {
                flush_minute(conn, alias, m, &minute_acc, minute_samples);
                minute_acc.clear();
                minute_samples = 0;
            }
            current_minute = Some(minute);
            minute_samples += 1;
            for g in &sample.groups {
                let a = minute_acc.entry(g.key.clone()).or_default();
                a.cpu_sum += g.cpu;
                a.cpu_max = a.cpu_max.max(g.cpu);
                a.mem_sum += g.mem;
                a.mem_max = a.mem_max.max(g.mem);
                a.procs = a.procs.max(g.procs);
                a.group = g.clone();
            }
        }
        let left = interval.saturating_sub(started.elapsed());
        let end = Instant::now() + left;
        while Instant::now() < end {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    if let Some(m) = current_minute {
        flush_minute(conn, alias, m, &minute_acc, minute_samples);
    }
    Ok(())
}

fn flush_minute(conn: &mut rusqlite::Connection, alias: &str, minute: i64, acc: &HashMap<String, Acc>, samples: u32) {
    if samples == 0 {
        return;
    }
    let n = samples as f64;
    let mut rows: Vec<db::GroupMinute> = acc
        .values()
        .map(|a| db::GroupMinute {
            group: &a.group,
            cpu_avg: a.cpu_sum / n,
            cpu_max: a.cpu_max,
            mem_avg: (a.mem_sum as f64 / n) as u64,
            mem_max: a.mem_max,
            procs: a.procs,
        })
        .filter(|r| r.cpu_avg >= MIN_CPU_PCT || r.mem_avg >= MIN_MEM_BYTES)
        .collect();
    if rows.len() > MAX_GROUPS_PER_MINUTE {
        rows.sort_by(|a, b| b.cpu_avg.total_cmp(&a.cpu_avg));
        let mut keep: Vec<db::GroupMinute> = rows.drain(..MAX_GROUPS_PER_MINUTE / 2).collect();
        rows.sort_by(|a, b| b.mem_avg.cmp(&a.mem_avg));
        keep.extend(rows.into_iter().take(MAX_GROUPS_PER_MINUTE / 2));
        rows = keep;
    }
    if let Err(e) = db::write_minute(conn, alias, minute, &rows) {
        log(&format!("{alias}: write_minute failed: {e}"));
    }
}

/// `JServer.exe --probe <alias>`: one login and two samples, printed. For checking a server by hand.
pub fn probe_cli(alias: &str) -> Result<(), String> {
    let (mut s, _, ms) = Session::connect(alias, probe::SETUP).map_err(|f| format!("{}: {}", f.label, f.detail))?;
    let info = probe::parse_info(&s.exec(probe::INFO, Duration::from_secs(40)).map_err(|f| f.detail)?);
    println!("{alias}: logged in in {ms} ms as {} on {} ({}), {} cores", info.user, info.hostname, info.os, info.cores);
    let mut t = Tracker::default();
    let mut sample = None;
    for i in 0..2 {
        let lines = s.exec(probe::SAMPLE, Duration::from_secs(30)).map_err(|f| f.detail)?;
        let missing = t.missing_meta(&lines);
        for chunk in missing.chunks(150) {
            let meta = s.exec(&probe::meta_script(chunk), Duration::from_secs(30)).map_err(|f| f.detail)?;
            t.add_meta(&meta, &lines);
        }
        sample = Some(t.sample(&lines, &info, now_f(), now()));
        if i == 0 {
            std::thread::sleep(Duration::from_secs(3));
        }
    }
    let s = sample.unwrap();
    let gb = |b: u64| b as f64 / 1073741824.0;
    println!(
        "cpu {:.1}%  iowait {:.1}%  steal {:.1}%  mem {:.1} of {:.1} GB used  swap {:.1}/{:.1} GB  zfs arc {:.1} GB  load {:?}  procs {}",
        s.cpu,
        s.iowait,
        s.steal,
        gb(s.mem_total - s.mem_avail),
        gb(s.mem_total),
        gb(s.swap_used),
        gb(s.swap_total),
        gb(s.arc),
        s.load,
        s.procs
    );
    println!("net in {:.0} B/s out {:.0} B/s  disk read {:.0} B/s write {:.0} B/s busy {:.0}%", s.net_rx, s.net_tx, s.disk_read, s.disk_write, s.disk_busy);
    for m in &s.mounts {
        println!("  {:<20} {:>6.1} of {:>6.1} GB used", m.mount, gb(m.used), gb(m.size));
    }
    println!("Top by memory:");
    for g in s.groups.iter().take(12) {
        println!("  {:<34} {:<8} {:>8.1} MB  cpu {:>5.1}%  x{:<3} {}", g.label, g.kind.chip(), g.mem as f64 / 1048576.0, g.cpu, g.procs, g.explain);
    }
    if !info.docker_note.is_empty() {
        println!("Note: {}", info.docker_note);
    }
    Ok(())
}
