//! Plain findings about one server, worked out from what the recorder stored.

use crate::describe::Kind;
use crate::{db, fmt};
use rusqlite::Connection;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warn,
    Danger,
}

#[derive(Clone, Debug)]
pub struct Insight {
    pub level: Level,
    pub title: String,
    pub detail: String,
}

fn add(out: &mut Vec<Insight>, level: Level, title: impl Into<String>, detail: impl Into<String>) {
    out.push(Insight { level, title: title.into(), detail: detail.into() });
}

/// What to do about a login that failed, by status.
pub fn connection_hint(status: &str, alias: &str) -> &'static str {
    let _ = alias;
    match status {
        "timeout" => "Port 22 did not answer in time. The server may be off, or a firewall or your network is blocking SSH.",
        "down" => "There is no route to the server from this PC. Check that it is on and that your network can reach it.",
        "refused" => "The server answered but nothing is listening for SSH on that port.",
        "dns" => "The host name in ~/.ssh/config could not be looked up.",
        "denied" => "The server did not accept your key. Check that the public key is in ~/.ssh/authorized_keys on the server for this user. Fix that, then press Retry.",
        "hostkey" => "The server's host key is not in known_hosts, or it changed. JServer never accepts host keys by itself. Run ssh once in a terminal to check and accept it.",
        "lost" => "The connection dropped and logging in again did not work. Press Retry when the server is back.",
        _ => "",
    }
}

fn label_of(g: &db::GroupSummary) -> String {
    match g.kind {
        Kind::Service | Kind::UserService => format!("{} (service)", g.label),
        Kind::Docker => format!("{} (Docker)", g.label),
        _ => g.label.clone(),
    }
}

/// Findings for one server over `from..to`, worst first.
pub fn compute(conn: &Connection, server: &db::Server, from: i64, to: i64) -> db::Result<Vec<Insight>> {
    let mut out = Vec::new();
    let alias = server.alias.as_str();
    let now = chrono::Utc::now().timestamp();

    if !matches!(server.status.as_str(), "up" | "connecting" | "paused" | "") {
        let hint = connection_hint(&server.status, alias);
        add(&mut out, Level::Danger, format!("Cannot log in: {}", server.label.to_lowercase()), if hint.is_empty() { server.detail.clone() } else { hint.to_string() });
    }

    let latest = db::latest(conn, alias)?.filter(|p| now - p.ts < 120);
    let groups = db::live(conn, alias)?;
    if let Some(p) = &latest {
        let avail_pct = if p.mem_total > 0.0 { p.mem_avail / p.mem_total * 100.0 } else { 100.0 };
        let top_mem = groups.iter().filter(|g| g.kind != Kind::Kernel).max_by(|a, b| a.mem.total_cmp(&b.mem));
        let who = top_mem.map(|g| format!(" The biggest user is {} with {}.", g.label, fmt::bytes(g.mem))).unwrap_or_default();
        if avail_pct < 5.0 {
            add(&mut out, Level::Danger, "Memory is almost full", format!("Only {} of {} is free for programs.{who} Linux may start stopping programs.", fmt::bytes(p.mem_avail), fmt::bytes(p.mem_total)));
        } else if avail_pct < 15.0 {
            add(&mut out, Level::Warn, "Memory is running low", format!("{} of {} is free for programs.{who}", fmt::bytes(p.mem_avail), fmt::bytes(p.mem_total)));
        }
        if p.arc > p.mem_total * 0.2 {
            add(
                &mut out,
                Level::Info,
                format!("ZFS is caching {} in memory", fmt::bytes(p.arc)),
                "ZFS keeps recently used disk data in RAM (the ARC). It gives memory back when programs need it, but more slowly than the normal file cache. You can cap it with zfs_arc_max.",
            );
        }
        if p.swap_total > 0.0 && p.swap_used > 256.0 * 1048576.0 && p.swap_pct() > 20.0 {
            add(&mut out, Level::Warn, format!("{} of swap in use", fmt::bytes(p.swap_used)), "Some memory has been moved to disk, which is much slower. This usually means memory ran short at some point.");
        }
        for m in db::mounts(conn, alias)? {
            let pct = m.pct();
            if pct >= 90.0 {
                add(&mut out, Level::Danger, format!("Disk {} is {}% full", m.mount, pct), format!("{} left of {}. Programs fail to write when it fills up.", fmt::bytes(m.avail), fmt::bytes(m.size)));
            } else if pct >= 80.0 {
                add(&mut out, Level::Warn, format!("Disk {} is {}% full", m.mount, pct), format!("{} left of {}.", fmt::bytes(m.avail), fmt::bytes(m.size)));
            }
        }
    }

    if let Some((min, max, first)) = db::oom_range(conn, alias, from, to)? {
        if max > min {
            add(
                &mut out,
                Level::Danger,
                format!("Linux stopped {} program{} for lack of memory", max - min, if max - min == 1 { "" } else { "s" }),
                format!("Out-of-memory kills since {}. Check which program grew in the memory chart around then.", fmt::day_time(first)),
            );
        }
    }

    let series = db::series(conn, alias, from, to, 60)?;
    if !series.is_empty() {
        let n = series.len() as f64;
        let avg = |f: fn(&db::Point) -> f64| series.iter().map(f).sum::<f64>() / n;
        let cores = server.cores.max(1) as f64;
        let cpu = avg(|p| p.cpu);
        let iowait = avg(|p| p.iowait);
        let steal = avg(|p| p.steal);
        let busy_minutes = series.iter().filter(|p| p.cpu >= 90.0).count();
        if cpu >= 80.0 {
            add(&mut out, Level::Danger, format!("CPU averaged {} over this range", fmt::pct(cpu)), "The server is busy most of the time. Work queues up and pages and jobs get slower.");
        } else if busy_minutes >= 15 {
            add(&mut out, Level::Warn, format!("CPU was above 90% for {} minutes", busy_minutes), "Look at the CPU chart to see when, and hover it to see what was running.");
        }
        let recent: Vec<&db::Point> = series.iter().filter(|p| p.ts >= to - 15 * 60).collect();
        if !recent.is_empty() {
            let load = recent.iter().map(|p| p.load1).sum::<f64>() / recent.len() as f64;
            if load > cores * 1.5 {
                add(&mut out, Level::Warn, format!("Load {:.1} on {} cores", load, cores), "More work is waiting than the CPU cores can run. This can also be programs waiting on a slow disk.");
            }
        }
        if iowait >= 15.0 {
            add(&mut out, Level::Warn, format!("CPU waits on disk {} of the time", fmt::pct(iowait)), "The disks are slow to answer for what is being asked of them. The disk I/O chart shows when.");
        }
        if steal >= 8.0 {
            add(&mut out, Level::Warn, format!("{} of CPU time taken by the host", fmt::pct(steal)), "This is a VM and the machine it runs on is giving its CPU to other VMs. Upgrading the plan or moving hosts helps; nothing inside this server will.");
        }
    }

    if server.reboot {
        add(&mut out, Level::Info, "Restart needed", "Updates were installed that only take effect after a reboot (/var/run/reboot-required).");
    }
    if !server.docker_note.is_empty() {
        add(&mut out, Level::Info, "Container names are hidden", server.docker_note.clone());
    }

    // What used the most over the range.
    let sums = db::group_summaries(conn, alias, from, to)?;
    let minutes = db::minutes_recorded(conn, alias, from, to)?.max(1) as f64;
    if let Some(g) = sums.iter().filter(|g| g.kind != Kind::Kernel).max_by(|a, b| a.cpu_load.total_cmp(&b.cpu_load)) {
        if g.cpu_load / minutes >= 1.0 {
            add(
                &mut out,
                Level::Info,
                format!("Most CPU: {}", label_of(g)),
                format!("Averaged {} of the whole server, peaking at {}. {}", fmt::pct(g.cpu_load / minutes), fmt::pct(g.cpu_peak), g.explain),
            );
        }
    }
    if let Some(g) = sums.iter().filter(|g| g.kind != Kind::Kernel).max_by(|a, b| a.mem_avg.total_cmp(&b.mem_avg)) {
        add(&mut out, Level::Info, format!("Most memory: {}", label_of(g)), format!("Averaged {}, at most {}. {}", fmt::bytes(g.mem_avg), fmt::bytes(g.mem_peak), g.explain));
    }

    out.sort_by(|a, b| b.level.cmp(&a.level));
    Ok(out)
}
