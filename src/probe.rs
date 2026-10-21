//! The read-only scripts JServer runs on a server, and turning what they print
//! into numbers. Everything comes from /proc, /sys and a few standard tools;
//! nothing is installed or written on the server.

use crate::describe::{self, Group, HostNames};
use std::collections::HashMap;

/// Runs once per login. Later scripts run in the same shell, so `$T` stays set.
pub const SETUP: &str = r#"export LC_ALL=C
T=; command -v timeout >/dev/null 2>&1 && T="timeout 5"
echo ok"#;

/// Facts that rarely change. Runs at login and every 10 minutes.
pub const INFO: &str = r#"echo @@host; hostname 2>/dev/null
echo @@os; (. /etc/os-release 2>/dev/null; echo "$PRETTY_NAME")
echo @@kernel; uname -sr 2>/dev/null
echo @@cpu; grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2
echo @@cores; nproc 2>/dev/null || grep -c ^processor /proc/cpuinfo
echo @@clk; getconf CLK_TCK 2>/dev/null
echo @@page; getconf PAGESIZE 2>/dev/null
echo @@user; id -un 2>/dev/null
echo @@nics; for d in /sys/class/net/*; do [ -e "$d/device" ] && echo "${d##*/}"; done; true
echo @@disks; for d in /sys/block/*; do [ -e "$d/device" ] && echo "${d##*/}"; done; true
echo @@reboot; [ -f /var/run/reboot-required ] && echo yes; true
echo @@docker; command -v docker >/dev/null 2>&1 && $T docker ps --no-trunc --format '{{.ID}}	{{.Names}}	{{.Image}}' </dev/null 2>&1 | head -300; true
echo @@qm; [ -x /usr/sbin/qm ] && $T /usr/sbin/qm list </dev/null 2>/dev/null | tail -n +2; true
echo @@pct; [ -x /usr/sbin/pct ] && $T /usr/sbin/pct list </dev/null 2>/dev/null | tail -n +2; true"#;

/// Counters for one sample. Runs every few seconds.
pub const SAMPLE: &str = r#"echo @@stat; grep '^cpu ' /proc/stat
echo @@mem; cat /proc/meminfo
echo @@load; cat /proc/loadavg
echo @@up; cat /proc/uptime
echo @@net; cat /proc/net/dev
echo @@dstat; cat /proc/diskstats
echo @@df; $T df -P -k -x tmpfs -x devtmpfs -x overlay -x squashfs -x efivarfs -x fuse.lxcfs -x fuse 2>/dev/null | tail -n +2
echo @@vm; grep -E '^(oom_kill|pswpin|pswpout) ' /proc/vmstat
echo @@arc; grep -E '^(size|c_max) ' /proc/spl/kstat/zfs/arcstats 2>/dev/null
echo @@pid; cat /proc/[0-9]*/stat 2>/dev/null; true"#;

/// Which service, container or VM each new process belongs to, its owner and command line.
pub fn meta_script(pids: &[u32]) -> String {
    let list: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
    format!(
        "for p in {}; do echo \"@@m $p\"; echo \"c $(grep -m1 -E '^0::|name=systemd' /proc/$p/cgroup 2>/dev/null)\"; echo \"u $(stat -c %U /proc/$p 2>/dev/null)\"; echo \"a $(tr '\\0\\n' '  ' </proc/$p/cmdline 2>/dev/null | head -c 400)\"; done",
        list.join(" ")
    )
}

/// Splits script output into `@@name` sections.
pub fn sections(lines: &[String]) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut cur = String::new();
    for l in lines {
        if let Some(name) = l.strip_prefix("@@") {
            cur = name.trim().to_string();
            out.entry(cur.clone()).or_default();
        } else if !cur.is_empty() {
            out.get_mut(&cur).unwrap().push(l.clone());
        }
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct Info {
    pub hostname: String,
    pub os: String,
    pub kernel: String,
    pub cpu_model: String,
    pub cores: u32,
    pub clk_tck: u64,
    pub page_size: u64,
    pub user: String,
    pub nics: Vec<String>,
    pub disks: Vec<String>,
    pub reboot_required: bool,
    /// Set when docker exists but this login cannot list containers.
    pub docker_note: String,
    pub names: HostNames,
}

pub fn parse_info(lines: &[String]) -> Info {
    let s = sections(lines);
    let first = |k: &str| s.get(k).and_then(|v| v.first()).map(|x| x.trim().to_string()).unwrap_or_default();
    let list = |k: &str| s.get(k).cloned().unwrap_or_default().into_iter().map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>();
    let mut names = HostNames::default();
    let mut docker_note = String::new();
    for l in list("docker") {
        let parts: Vec<&str> = l.split('\t').collect();
        if parts.len() >= 3 && parts[0].len() >= 12 && parts[0].chars().all(|c| c.is_ascii_hexdigit()) {
            names.containers.insert(parts[0].to_string(), (parts[1].to_string(), parts[2].to_string()));
        } else if l.to_lowercase().contains("permission denied") {
            docker_note = "This login cannot ask Docker for container names. Add the user to the docker group to see them.".into();
        }
    }
    // qm list: VMID NAME STATUS MEM(MB) BOOTDISK(GB) PID
    for l in list("qm") {
        let mut it = l.split_whitespace();
        if let (Some(id), Some(name)) = (it.next(), it.next()) {
            names.vms.insert(id.to_string(), name.to_string());
        }
    }
    // pct list: VMID Status [Lock] Name
    for l in list("pct") {
        let parts: Vec<&str> = l.split_whitespace().collect();
        if parts.len() >= 3 {
            names.lxc.insert(parts[0].to_string(), parts[parts.len() - 1].to_string());
        }
    }
    Info {
        hostname: first("host"),
        os: first("os"),
        kernel: first("kernel"),
        cpu_model: first("cpu").split_whitespace().collect::<Vec<_>>().join(" "),
        cores: first("cores").parse().unwrap_or(1).max(1),
        clk_tck: first("clk").parse().unwrap_or(100).max(1),
        page_size: first("page").parse().unwrap_or(4096).max(1),
        user: first("user"),
        nics: list("nics"),
        disks: list("disks"),
        reboot_required: first("reboot") == "yes",
        docker_note,
        names,
    }
}

/// One process line from /proc/<pid>/stat.
#[derive(Clone, Debug)]
pub struct ProcStat {
    pub pid: u32,
    pub comm: String,
    pub kernel_thread: bool,
    pub ticks: u64,
    pub start: u64,
    pub rss_pages: u64,
}

pub fn parse_proc_stat(line: &str) -> Option<ProcStat> {
    let open = line.find('(')?;
    let close = line.rfind(')')?;
    let pid = line[..open].trim().parse().ok()?;
    let comm = line[open + 1..close].to_string();
    let f: Vec<&str> = line[close + 1..].split_whitespace().collect();
    if f.len() < 22 {
        return None;
    }
    let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
    const PF_KTHREAD: u64 = 0x0020_0000;
    Some(ProcStat {
        pid,
        comm,
        kernel_thread: n(6) & PF_KTHREAD != 0 || pid == 2 || n(1) == 2,
        ticks: n(11) + n(12),
        start: n(19),
        rss_pages: n(21),
    })
}

#[derive(Clone, Debug, Default)]
pub struct Meta {
    pub cgroup: String,
    pub user: String,
    pub cmdline: String,
}

pub fn parse_meta(lines: &[String]) -> HashMap<u32, Meta> {
    let mut out = HashMap::new();
    let mut cur: Option<u32> = None;
    for l in lines {
        if let Some(p) = l.strip_prefix("@@m ") {
            cur = p.trim().parse().ok();
            if let Some(p) = cur {
                out.insert(p, Meta::default());
            }
            continue;
        }
        let Some(m) = cur.and_then(|p| out.get_mut(&p)) else { continue };
        if let Some(v) = l.strip_prefix("c ") {
            m.cgroup = v.trim().to_string();
        } else if let Some(v) = l.strip_prefix("u ") {
            // Container users that the host does not know show as UNKNOWN.
            m.user = v.trim().to_string();
            if m.user == "UNKNOWN" {
                m.user.clear();
            }
        } else if let Some(v) = l.strip_prefix("a ") {
            m.cmdline = v.trim().to_string();
        }
    }
    out
}

#[derive(Clone, Debug, Default)]
pub struct Mount {
    pub mount: String,
    pub fs: String,
    pub size: u64,
    pub used: u64,
    pub avail: u64,
}

/// A finished sample: whole-server numbers plus usage grouped by what owns it.
#[derive(Clone, Debug, Default)]
pub struct Sample {
    pub ts: i64,
    pub cpu: f64,
    pub iowait: f64,
    pub steal: f64,
    pub mem_total: u64,
    pub mem_avail: u64,
    pub mem_free: u64,
    pub arc: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub load: [f64; 3],
    pub uptime: u64,
    pub net_rx: f64,
    pub net_tx: f64,
    pub disk_read: f64,
    pub disk_write: f64,
    /// Busiest physical disk, percent of time it was working.
    pub disk_busy: f64,
    pub procs: i64,
    pub oom_kills: u64,
    pub mounts: Vec<Mount>,
    pub groups: Vec<Group>,
}

#[derive(Clone, Copy, Default)]
struct Counters {
    cpu_total: u64,
    cpu_idle: u64,
    cpu_iowait: u64,
    cpu_steal: u64,
    net_rx: u64,
    net_tx: u64,
    disk_read: u64,
    disk_write: u64,
}

/// Keeps the previous sample's counters for one server so rates can be worked out.
#[derive(Default)]
pub struct Tracker {
    prev: Option<(f64, Counters)>,
    prev_busy: HashMap<String, u64>,
    prev_ticks: HashMap<(u32, u64), u64>,
    meta: HashMap<(u32, u64), Meta>,
}

impl Tracker {
    /// Processes seen in `lines` that we have no owner details for yet.
    pub fn missing_meta(&self, lines: &[String]) -> Vec<u32> {
        let s = sections(lines);
        s.get("pid")
            .map(|v| {
                v.iter()
                    .filter_map(|l| parse_proc_stat(l))
                    .filter(|p| !p.kernel_thread && !self.meta.contains_key(&(p.pid, p.start)))
                    .map(|p| p.pid)
                    .take(600)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn add_meta(&mut self, lines: &[String], sample_lines: &[String]) {
        let got = parse_meta(lines);
        let s = sections(sample_lines);
        for p in s.get("pid").into_iter().flatten().filter_map(|l| parse_proc_stat(l)) {
            if let Some(m) = got.get(&p.pid) {
                self.meta.insert((p.pid, p.start), m.clone());
            }
        }
    }

    /// Turns one SAMPLE reply into a Sample. `secs` is the time since the last one.
    pub fn sample(&mut self, lines: &[String], info: &Info, now: f64, ts: i64) -> Sample {
        let s = sections(lines);
        let get = |k: &str| s.get(k).cloned().unwrap_or_default();
        let mut c = Counters::default();
        let mut out = Sample { ts, ..Default::default() };

        // cpu  user nice system idle iowait irq softirq steal guest guest_nice
        if let Some(l) = get("stat").first() {
            let v: Vec<u64> = l.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
            // guest time is already inside user time.
            c.cpu_total = v.iter().take(8).sum();
            c.cpu_idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            c.cpu_iowait = v.get(4).copied().unwrap_or(0);
            c.cpu_steal = v.get(7).copied().unwrap_or(0);
        }
        let mut mem: HashMap<String, u64> = HashMap::new();
        for l in get("mem") {
            let mut it = l.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                mem.insert(k.trim_end_matches(':').to_string(), v.parse::<u64>().unwrap_or(0) * 1024);
            }
        }
        let m = |k: &str| mem.get(k).copied().unwrap_or(0);
        out.mem_total = m("MemTotal");
        out.mem_free = m("MemFree");
        out.mem_avail = if mem.contains_key("MemAvailable") { m("MemAvailable") } else { m("MemFree") + m("Cached") + m("Buffers") };
        out.swap_total = m("SwapTotal");
        out.swap_used = m("SwapTotal").saturating_sub(m("SwapFree"));
        for l in get("arc") {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() >= 3 && f[0] == "size" {
                out.arc = f[2].parse().unwrap_or(0);
            }
        }
        if let Some(l) = get("load").first() {
            let v: Vec<f64> = l.split_whitespace().take(3).filter_map(|x| x.parse().ok()).collect();
            if v.len() == 3 {
                out.load = [v[0], v[1], v[2]];
            }
        }
        out.uptime = get("up").first().and_then(|l| l.split_whitespace().next()).and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) as u64;

        // Only physical (or virtio) network cards, so bridges and VM taps are not counted twice.
        for l in get("net") {
            let Some((name, rest)) = l.split_once(':') else { continue };
            let name = name.trim();
            let physical = if info.nics.is_empty() { name != "lo" } else { info.nics.iter().any(|n| n == name) };
            if !physical {
                continue;
            }
            let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            if v.len() >= 9 {
                c.net_rx += v[0];
                c.net_tx += v[8];
            }
        }
        // diskstats: major minor name reads merged sectors ms writes merged sectors ms inflight io_ms ...
        let mut busy_now: HashMap<String, u64> = HashMap::new();
        for l in get("dstat") {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 13 || !info.disks.iter().any(|d| d == f[2]) {
                continue;
            }
            let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
            c.disk_read += n(5) * 512;
            c.disk_write += n(9) * 512;
            busy_now.insert(f[2].to_string(), n(12));
        }
        for l in get("df") {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() >= 6 {
                let kb = |i: usize| f[i].parse::<u64>().unwrap_or(0) * 1024;
                let mount = f[5..].join(" ");
                if mount.starts_with("/boot/efi") || kb(1) == 0 {
                    continue;
                }
                out.mounts.push(Mount { fs: f[0].to_string(), size: kb(1), used: kb(2), avail: kb(3), mount });
            }
        }
        out.mounts.sort_by(|a, b| a.mount.cmp(&b.mount));
        out.mounts.dedup_by(|a, b| a.fs == b.fs && a.size == b.size);
        for l in get("vm") {
            if let Some(v) = l.strip_prefix("oom_kill ") {
                out.oom_kills = v.trim().parse().unwrap_or(0);
            }
        }

        if let Some((t0, p)) = self.prev {
            let secs = (now - t0).max(0.5);
            let dt = c.cpu_total.saturating_sub(p.cpu_total).max(1) as f64;
            out.cpu = (1.0 - c.cpu_idle.saturating_sub(p.cpu_idle) as f64 / dt) * 100.0;
            out.iowait = c.cpu_iowait.saturating_sub(p.cpu_iowait) as f64 / dt * 100.0;
            out.steal = c.cpu_steal.saturating_sub(p.cpu_steal) as f64 / dt * 100.0;
            out.net_rx = c.net_rx.saturating_sub(p.net_rx) as f64 / secs;
            out.net_tx = c.net_tx.saturating_sub(p.net_tx) as f64 / secs;
            out.disk_read = c.disk_read.saturating_sub(p.disk_read) as f64 / secs;
            out.disk_write = c.disk_write.saturating_sub(p.disk_write) as f64 / secs;
            for (d, ms) in &busy_now {
                if let Some(prev) = self.prev_busy.get(d) {
                    let pct = ms.saturating_sub(*prev) as f64 / (secs * 1000.0) * 100.0;
                    out.disk_busy = out.disk_busy.max(pct.min(100.0));
                }
            }
        }

        // Processes, grouped by the service, container or VM they belong to.
        let procs: Vec<ProcStat> = get("pid").iter().filter_map(|l| parse_proc_stat(l)).collect();
        out.procs = procs.len() as i64;
        let secs = self.prev.map(|(t0, _)| (now - t0).max(0.5));
        let cores = info.cores.max(1) as f64;
        let mut ticks_now = HashMap::with_capacity(procs.len());
        let mut groups: HashMap<String, Group> = HashMap::new();
        for p in &procs {
            let key = (p.pid, p.start);
            ticks_now.insert(key, p.ticks);
            let cpu = match (secs, self.prev_ticks.get(&key)) {
                (Some(secs), Some(prev)) => p.ticks.saturating_sub(*prev) as f64 / info.clk_tck as f64 / secs / cores * 100.0,
                _ => 0.0,
            };
            let mem = p.rss_pages * info.page_size;
            let meta = self.meta.get(&key);
            let (gkey, kind, label) = describe::classify(&p.comm, p.kernel_thread, meta, &info.names);
            let g = groups.entry(gkey.clone()).or_insert_with(|| Group { key: gkey, kind, label, ..Default::default() });
            g.cpu += cpu;
            g.mem += mem;
            g.procs += 1;
            // The process that weighs the most names what the group runs.
            let weight = cpu * 1e9 + mem as f64;
            if weight >= g.main_weight {
                g.main_weight = weight;
                g.cmdline = meta.map(|m| m.cmdline.clone()).unwrap_or_default();
                g.main = describe::program_of(&p.comm, &g.cmdline);
            }
            if let Some(m) = meta {
                if !m.user.is_empty() && !g.users.contains(&m.user) && g.users.len() < 4 {
                    g.users.push(m.user.clone());
                }
            }
        }
        // Forget details about processes that have exited.
        self.meta.retain(|k, _| ticks_now.contains_key(k));
        self.prev_ticks = ticks_now;
        self.prev_busy = busy_now;
        self.prev = Some((now, c));
        let mut groups: Vec<Group> = groups.into_values().collect();
        for g in &mut groups {
            g.explain = describe::explain(g);
        }
        groups.sort_by(|a, b| b.mem.cmp(&a.mem));
        out.groups = groups;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_proc_stat_with_spaces_in_name() {
        let l = "1635 (kvm) S 1 1635 1635 0 -1 138412352 1 0 0 0 9000 1000 0 0 20 0 50 0 4242 1 3325627 18446744073709551615";
        let p = parse_proc_stat(l).unwrap();
        assert_eq!((p.pid, p.comm.as_str(), p.ticks, p.start, p.rss_pages), (1635, "kvm", 10000, 4242, 3325627));
        let k = parse_proc_stat("12 (kworker/0:1 (ev)) I 2 0 0 0 -1 69238880 0 0 0 0 5 7 0 0 20 0 1 0 99 0 0 1").unwrap();
        assert_eq!(k.comm, "kworker/0:1 (ev)");
        assert!(k.kernel_thread);
    }

    #[test]
    fn reads_meta_blocks() {
        let lines: Vec<String> = ["@@m 10", "c 0::/system.slice/nginx.service", "u www-data", "a nginx: worker process ", "@@m 11", "c ", "u ", "a "]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let m = parse_meta(&lines);
        assert_eq!(m[&10].cgroup, "0::/system.slice/nginx.service");
        assert_eq!(m[&10].user, "www-data");
        assert_eq!(m[&11].cmdline, "");
    }
}
