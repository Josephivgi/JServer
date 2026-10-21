//! SQLite storage in %APPDATA%\JServer\jserver.db. The background recorder
//! writes; the window reads.

use crate::describe::{Group, Kind};
use crate::probe::{Info, Sample};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

pub type Result<T> = rusqlite::Result<T>;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS servers (
    alias        TEXT PRIMARY KEY,
    enabled      INTEGER NOT NULL DEFAULT 1,
    status       TEXT,
    label        TEXT,
    detail       TEXT,
    checked_at   INTEGER,
    connected_at INTEGER,
    login_ms     INTEGER,
    hostname     TEXT,
    os           TEXT,
    kernel       TEXT,
    cpu_model    TEXT,
    cores        INTEGER,
    login_user   TEXT,
    reboot       INTEGER NOT NULL DEFAULT 0,
    docker_note  TEXT,
    last_sample  INTEGER
);
CREATE TABLE IF NOT EXISTS samples (
    server     TEXT    NOT NULL,
    ts         INTEGER NOT NULL,
    cpu        REAL    NOT NULL,
    iowait     REAL    NOT NULL,
    steal      REAL    NOT NULL,
    mem_total  INTEGER NOT NULL,
    mem_avail  INTEGER NOT NULL,
    mem_free   INTEGER NOT NULL,
    arc        INTEGER NOT NULL,
    swap_total INTEGER NOT NULL,
    swap_used  INTEGER NOT NULL,
    load1      REAL    NOT NULL,
    load5      REAL    NOT NULL,
    load15     REAL    NOT NULL,
    uptime     INTEGER NOT NULL,
    net_rx     REAL    NOT NULL,
    net_tx     REAL    NOT NULL,
    disk_read  REAL    NOT NULL,
    disk_write REAL    NOT NULL,
    disk_busy  REAL    NOT NULL,
    procs      INTEGER NOT NULL,
    oom_kills  INTEGER NOT NULL,
    PRIMARY KEY (server, ts)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS live (
    server  TEXT    NOT NULL,
    key     TEXT    NOT NULL,
    kind    TEXT    NOT NULL,
    label   TEXT    NOT NULL,
    main    TEXT    NOT NULL,
    cmdline TEXT    NOT NULL,
    users   TEXT    NOT NULL,
    explain TEXT    NOT NULL,
    cpu     REAL    NOT NULL,
    mem     INTEGER NOT NULL,
    procs   INTEGER NOT NULL,
    PRIMARY KEY (server, key)
);
CREATE TABLE IF NOT EXISTS mounts (
    server TEXT    NOT NULL,
    mount  TEXT    NOT NULL,
    fs     TEXT    NOT NULL,
    size   INTEGER NOT NULL,
    used   INTEGER NOT NULL,
    avail  INTEGER NOT NULL,
    PRIMARY KEY (server, mount)
);
CREATE TABLE IF NOT EXISTS groups (
    server  TEXT NOT NULL,
    key     TEXT NOT NULL,
    kind    TEXT NOT NULL,
    label   TEXT NOT NULL,
    main    TEXT NOT NULL,
    explain TEXT NOT NULL,
    PRIMARY KEY (server, key)
);
CREATE TABLE IF NOT EXISTS group_minutes (
    server  TEXT    NOT NULL,
    minute  INTEGER NOT NULL,
    key     TEXT    NOT NULL,
    cpu_avg REAL    NOT NULL,
    cpu_max REAL    NOT NULL,
    mem_avg INTEGER NOT NULL,
    mem_max INTEGER NOT NULL,
    procs   INTEGER NOT NULL,
    PRIMARY KEY (server, minute, key)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS collector_status (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    pid       INTEGER NOT NULL,
    started   INTEGER NOT NULL,
    heartbeat INTEGER NOT NULL
);
"#;

pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

// ---------- settings ----------

/// "0" stops the background recorder (the updater uses this to restart it).
pub const KEY_MONITORING: &str = "monitoring";
/// Bumped by Refresh so the recorder retries servers that failed.
pub const KEY_RETRY: &str = "retry_gen";
pub const KEY_RETENTION: &str = "retention_days";
pub const DEFAULT_RETENTION: i64 = 30;
/// Seconds between samples.
pub const INTERVAL: u64 = 5;

pub fn setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()
}

pub fn setting_i64(conn: &Connection, key: &str, default: i64) -> i64 {
    setting(conn, key).ok().flatten().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute("INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value", params![key, value])?;
    Ok(())
}

pub fn monitoring_enabled(conn: &Connection) -> bool {
    setting(conn, KEY_MONITORING).ok().flatten().is_none_or(|v| v != "0")
}

pub fn bump_retry(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO settings(key, value) VALUES (?1, '1') ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
        [KEY_RETRY],
    )?;
    Ok(())
}

// ---------- recorder status ----------

pub struct CollectorStatus {
    pub started: i64,
    pub heartbeat: i64,
}

pub fn mark_collector_started(conn: &Connection, pid: u32, ts: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO collector_status(id, pid, started, heartbeat) VALUES (1, ?1, ?2, ?2)
         ON CONFLICT(id) DO UPDATE SET pid = excluded.pid, started = excluded.started, heartbeat = excluded.heartbeat",
        params![pid, ts],
    )?;
    Ok(())
}

pub fn heartbeat(conn: &Connection, ts: i64) -> Result<()> {
    conn.execute("UPDATE collector_status SET heartbeat = ?1 WHERE id = 1", [ts])?;
    Ok(())
}

pub fn collector_status(conn: &Connection) -> Result<Option<CollectorStatus>> {
    conn.query_row("SELECT started, heartbeat FROM collector_status WHERE id = 1", [], |r| {
        Ok(CollectorStatus { started: r.get(0)?, heartbeat: r.get(1)? })
    })
    .optional()
}

pub fn collector_pid(conn: &Connection) -> Result<Option<u32>> {
    conn.query_row("SELECT pid FROM collector_status WHERE id = 1", [], |r| r.get(0)).optional()
}

// ---------- servers ----------

#[derive(Clone, Debug, Default)]
pub struct Server {
    pub alias: String,
    pub enabled: bool,
    pub status: String,
    pub label: String,
    pub detail: String,
    pub checked_at: i64,
    pub connected_at: i64,
    pub login_ms: i64,
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub cpu_model: String,
    pub cores: i64,
    pub login_user: String,
    pub reboot: bool,
    pub docker_note: String,
    pub last_sample: i64,
}

pub fn ensure_servers(conn: &Connection, aliases: &[String]) -> Result<()> {
    for a in aliases {
        conn.execute("INSERT OR IGNORE INTO servers(alias) VALUES (?1)", [a])?;
    }
    Ok(())
}

pub fn servers(conn: &Connection) -> Result<Vec<Server>> {
    let mut st = conn.prepare(
        "SELECT alias, enabled, COALESCE(status,''), COALESCE(label,''), COALESCE(detail,''), COALESCE(checked_at,0),
                COALESCE(connected_at,0), COALESCE(login_ms,0), COALESCE(hostname,''), COALESCE(os,''), COALESCE(kernel,''),
                COALESCE(cpu_model,''), COALESCE(cores,0), COALESCE(login_user,''), reboot, COALESCE(docker_note,''), COALESCE(last_sample,0)
         FROM servers",
    )?;
    st.query_map([], |r| {
        Ok(Server {
            alias: r.get(0)?,
            enabled: r.get::<_, i64>(1)? != 0,
            status: r.get(2)?,
            label: r.get(3)?,
            detail: r.get(4)?,
            checked_at: r.get(5)?,
            connected_at: r.get(6)?,
            login_ms: r.get(7)?,
            hostname: r.get(8)?,
            os: r.get(9)?,
            kernel: r.get(10)?,
            cpu_model: r.get(11)?,
            cores: r.get(12)?,
            login_user: r.get(13)?,
            reboot: r.get::<_, i64>(14)? != 0,
            docker_note: r.get(15)?,
            last_sample: r.get(16)?,
        })
    })?
    .collect()
}

pub fn set_enabled(conn: &Connection, alias: &str, on: bool) -> Result<()> {
    conn.execute("UPDATE servers SET enabled = ?2 WHERE alias = ?1", params![alias, on as i64])?;
    Ok(())
}

pub fn set_status(conn: &Connection, alias: &str, status: &str, label: &str, detail: &str, ts: i64) -> Result<()> {
    conn.execute(
        "UPDATE servers SET status = ?2, label = ?3, detail = ?4, checked_at = ?5 WHERE alias = ?1",
        params![alias, status, label, detail, ts],
    )?;
    Ok(())
}

pub fn set_connected(conn: &Connection, alias: &str, info: &Info, login_ms: u64, ts: i64) -> Result<()> {
    conn.execute(
        "UPDATE servers SET status = 'up', label = 'Online', detail = '', checked_at = ?2, connected_at = ?2, login_ms = ?3,
                hostname = ?4, os = ?5, kernel = ?6, cpu_model = ?7, cores = ?8, login_user = ?9, reboot = ?10, docker_note = ?11
         WHERE alias = ?1",
        params![alias, ts, login_ms as i64, info.hostname, info.os, info.kernel, info.cpu_model, info.cores, info.user, info.reboot_required as i64, info.docker_note],
    )?;
    Ok(())
}

// ---------- samples ----------

pub fn write_sample(conn: &mut Connection, alias: &str, s: &Sample) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT OR REPLACE INTO samples(server, ts, cpu, iowait, steal, mem_total, mem_avail, mem_free, arc, swap_total, swap_used,
            load1, load5, load15, uptime, net_rx, net_tx, disk_read, disk_write, disk_busy, procs, oom_kills)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
        params![
            alias, s.ts, s.cpu, s.iowait, s.steal, s.mem_total as i64, s.mem_avail as i64, s.mem_free as i64, s.arc as i64,
            s.swap_total as i64, s.swap_used as i64, s.load[0], s.load[1], s.load[2], s.uptime as i64, s.net_rx, s.net_tx,
            s.disk_read, s.disk_write, s.disk_busy, s.procs, s.oom_kills as i64
        ],
    )?;
    tx.execute("DELETE FROM live WHERE server = ?1", [alias])?;
    {
        let mut ins = tx.prepare(
            "INSERT INTO live(server, key, kind, label, main, cmdline, users, explain, cpu, mem, procs) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        )?;
        for g in &s.groups {
            ins.execute(params![alias, g.key, g.kind.as_str(), g.label, g.main, g.cmdline, g.users.join(", "), g.explain, g.cpu, g.mem as i64, g.procs])?;
        }
    }
    tx.execute("DELETE FROM mounts WHERE server = ?1", [alias])?;
    {
        let mut ins = tx.prepare("INSERT OR REPLACE INTO mounts(server, mount, fs, size, used, avail) VALUES (?1,?2,?3,?4,?5,?6)")?;
        for m in &s.mounts {
            ins.execute(params![alias, m.mount, m.fs, m.size as i64, m.used as i64, m.avail as i64])?;
        }
    }
    tx.execute("UPDATE servers SET last_sample = ?2 WHERE alias = ?1", params![alias, s.ts])?;
    tx.commit()
}

pub struct GroupMinute<'a> {
    pub group: &'a Group,
    pub cpu_avg: f64,
    pub cpu_max: f64,
    pub mem_avg: u64,
    pub mem_max: u64,
    pub procs: i64,
}

pub fn write_minute(conn: &mut Connection, alias: &str, minute: i64, rows: &[GroupMinute]) -> Result<()> {
    let tx = conn.transaction()?;
    {
        let mut g = tx.prepare(
            "INSERT INTO groups(server, key, kind, label, main, explain) VALUES (?1,?2,?3,?4,?5,?6)
             ON CONFLICT(server, key) DO UPDATE SET kind = excluded.kind, label = excluded.label, main = excluded.main, explain = excluded.explain",
        )?;
        let mut m = tx.prepare(
            "INSERT OR REPLACE INTO group_minutes(server, minute, key, cpu_avg, cpu_max, mem_avg, mem_max, procs) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
        )?;
        for r in rows {
            g.execute(params![alias, r.group.key, r.group.kind.as_str(), r.group.label, r.group.main, r.group.explain])?;
            m.execute(params![alias, minute, r.group.key, r.cpu_avg, r.cpu_max, r.mem_avg as i64, r.mem_max as i64, r.procs])?;
        }
    }
    tx.commit()
}

pub fn prune(conn: &Connection, before: i64) -> Result<usize> {
    let a = conn.execute("DELETE FROM samples WHERE ts < ?1", [before])?;
    let b = conn.execute("DELETE FROM group_minutes WHERE minute < ?1", [before])?;
    Ok(a + b)
}

/// Removes everything stored for servers no longer in ~/.ssh/config.
pub fn forget_servers_not_in(conn: &Connection, aliases: &[String]) -> Result<()> {
    let known: Vec<String> = conn.prepare("SELECT alias FROM servers")?.query_map([], |r| r.get(0))?.collect::<Result<_>>()?;
    for a in known.iter().filter(|a| !aliases.contains(a)) {
        for t in ["live", "mounts"] {
            conn.execute(&format!("DELETE FROM {t} WHERE server = ?1"), [a])?;
        }
        conn.execute("DELETE FROM servers WHERE alias = ?1", [a])?;
    }
    Ok(())
}

// ---------- reading for the window ----------

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
pub struct Point {
    pub ts: i64,
    pub cpu: f64,
    pub iowait: f64,
    pub steal: f64,
    pub mem_total: f64,
    pub mem_avail: f64,
    pub mem_free: f64,
    pub arc: f64,
    pub swap_total: f64,
    pub swap_used: f64,
    pub load1: f64,
    pub load5: f64,
    pub load15: f64,
    pub uptime: i64,
    pub net_rx: f64,
    pub net_tx: f64,
    pub disk_read: f64,
    pub disk_write: f64,
    pub disk_busy: f64,
    pub procs: f64,
    pub oom_kills: i64,
}

impl Point {
    pub fn mem_used(&self) -> f64 {
        (self.mem_total - self.mem_avail).max(0.0)
    }
    pub fn mem_pct(&self) -> f64 {
        if self.mem_total > 0.0 { self.mem_used() / self.mem_total * 100.0 } else { 0.0 }
    }
    pub fn swap_pct(&self) -> f64 {
        if self.swap_total > 0.0 { self.swap_used / self.swap_total * 100.0 } else { 0.0 }
    }
}

const POINT_COLS: &str = "cpu, iowait, steal, mem_total, mem_avail, mem_free, arc, swap_total, swap_used, load1, load5, load15, uptime,
    net_rx, net_tx, disk_read, disk_write, disk_busy, procs, oom_kills";

fn point(r: &rusqlite::Row, ts: i64) -> Result<Point> {
    Ok(Point {
        ts,
        cpu: r.get(1)?,
        iowait: r.get(2)?,
        steal: r.get(3)?,
        mem_total: r.get(4)?,
        mem_avail: r.get(5)?,
        mem_free: r.get(6)?,
        arc: r.get(7)?,
        swap_total: r.get(8)?,
        swap_used: r.get(9)?,
        load1: r.get(10)?,
        load5: r.get(11)?,
        load15: r.get(12)?,
        uptime: r.get::<_, f64>(13)? as i64,
        net_rx: r.get(14)?,
        net_tx: r.get(15)?,
        disk_read: r.get(16)?,
        disk_write: r.get(17)?,
        disk_busy: r.get(18)?,
        procs: r.get(19)?,
        oom_kills: r.get::<_, f64>(20)? as i64,
    })
}

pub fn latest(conn: &Connection, alias: &str) -> Result<Option<Point>> {
    conn.query_row(
        &format!("SELECT ts, {POINT_COLS} FROM samples WHERE server = ?1 ORDER BY ts DESC LIMIT 1"),
        [alias],
        |r| {
            let ts = r.get(0)?;
            point(r, ts)
        },
    )
    .optional()
}

/// Averages over `bucket`-second buckets (the first sample of a login has no rates, so cpu = 0 rows from it are skipped).
pub fn series(conn: &Connection, alias: &str, from: i64, to: i64, bucket: i64) -> Result<Vec<Point>> {
    let b = bucket.max(1);
    let avg = POINT_COLS.split(',').map(|c| format!("AVG({0})", c.trim())).collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT (ts / {b}) * {b} AS bt, {avg} FROM samples WHERE server = ?1 AND ts BETWEEN ?2 AND ?3 GROUP BY bt ORDER BY bt");
    let mut st = conn.prepare(&sql)?;
    st.query_map(params![alias, from, to], |r| {
        let ts = r.get(0)?;
        point(r, ts)
    })?
    .collect()
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
pub struct LiveGroup {
    pub key: String,
    pub kind: Kind,
    pub label: String,
    pub main: String,
    pub cmdline: String,
    pub users: String,
    pub explain: String,
    pub cpu: f64,
    pub mem: f64,
    pub procs: i64,
}

pub fn live(conn: &Connection, alias: &str) -> Result<Vec<LiveGroup>> {
    let mut st = conn.prepare("SELECT key, kind, label, main, cmdline, users, explain, cpu, mem, procs FROM live WHERE server = ?1 ORDER BY mem DESC")?;
    st.query_map([alias], |r| {
        Ok(LiveGroup {
            key: r.get(0)?,
            kind: Kind::parse(&r.get::<_, String>(1)?),
            label: r.get(2)?,
            main: r.get(3)?,
            cmdline: r.get(4)?,
            users: r.get(5)?,
            explain: r.get(6)?,
            cpu: r.get(7)?,
            mem: r.get(8)?,
            procs: r.get(9)?,
        })
    })?
    .collect()
}

#[derive(Clone, Debug, Default)]
pub struct MountRow {
    pub mount: String,
    pub fs: String,
    pub size: f64,
    pub used: f64,
    pub avail: f64,
}

impl MountRow {
    /// Same rounding as df's Use% column.
    pub fn pct(&self) -> f64 {
        let total = self.used + self.avail;
        if total > 0.0 { (self.used / total * 100.0).ceil() } else { 0.0 }
    }
}

pub fn mounts(conn: &Connection, alias: &str) -> Result<Vec<MountRow>> {
    let mut st = conn.prepare("SELECT mount, fs, size, used, avail FROM mounts WHERE server = ?1 ORDER BY mount")?;
    st.query_map([alias], |r| Ok(MountRow { mount: r.get(0)?, fs: r.get(1)?, size: r.get(2)?, used: r.get(3)?, avail: r.get(4)? }))?
        .collect()
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default)]
pub struct GroupSummary {
    pub key: String,
    pub kind: Kind,
    pub label: String,
    pub explain: String,
    /// Sum of per-minute CPU averages; divide by minutes recorded for the average.
    pub cpu_load: f64,
    pub cpu_peak: f64,
    pub mem_avg: f64,
    pub mem_peak: f64,
    pub minutes: i64,
}

pub fn group_summaries(conn: &Connection, alias: &str, from: i64, to: i64) -> Result<Vec<GroupSummary>> {
    let mut st = conn.prepare(
        "SELECT m.key, COALESCE(g.kind,'program'), COALESCE(g.label, m.key), COALESCE(g.explain,''),
                SUM(m.cpu_avg), MAX(m.cpu_max), AVG(m.mem_avg), MAX(m.mem_max), COUNT(*)
         FROM group_minutes m LEFT JOIN groups g ON g.server = m.server AND g.key = m.key
         WHERE m.server = ?1 AND m.minute BETWEEN ?2 AND ?3
         GROUP BY m.key ORDER BY SUM(m.cpu_avg) DESC",
    )?;
    st.query_map(params![alias, from, to], |r| {
        Ok(GroupSummary {
            key: r.get(0)?,
            kind: Kind::parse(&r.get::<_, String>(1)?),
            label: r.get(2)?,
            explain: r.get(3)?,
            cpu_load: r.get(4)?,
            cpu_peak: r.get(5)?,
            mem_avg: r.get(6)?,
            mem_peak: r.get(7)?,
            minutes: r.get(8)?,
        })
    })?
    .collect()
}

pub fn minutes_recorded(conn: &Connection, alias: &str, from: i64, to: i64) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(DISTINCT minute) FROM group_minutes WHERE server = ?1 AND minute BETWEEN ?2 AND ?3",
        params![alias, from, to],
        |r| r.get(0),
    )
}

/// (min, max) of the out-of-memory kill counter in a range, ignoring reboots.
pub fn oom_range(conn: &Connection, alias: &str, from: i64, to: i64) -> Result<Option<(i64, i64, i64)>> {
    conn.query_row(
        "SELECT MIN(oom_kills), MAX(oom_kills), MIN(ts) FROM samples WHERE server = ?1 AND ts BETWEEN ?2 AND ?3",
        params![alias, from, to],
        |r| Ok(r.get::<_, Option<i64>>(0)?.zip(r.get::<_, Option<i64>>(1)?).zip(r.get::<_, Option<i64>>(2)?).map(|((a, b), c)| (a, b, c))),
    )
}
