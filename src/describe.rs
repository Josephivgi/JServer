//! Groups a server's processes by what owns them (a VM, a container, a systemd
//! service, or a program name) and says in plain words what each one is.

use crate::probe::Meta;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Kind {
    Vm,
    Lxc,
    Docker,
    Service,
    UserService,
    Kernel,
    #[default]
    Program,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Vm => "vm",
            Kind::Lxc => "lxc",
            Kind::Docker => "docker",
            Kind::Service => "service",
            Kind::UserService => "user-service",
            Kind::Kernel => "kernel",
            Kind::Program => "program",
        }
    }
    pub fn parse(s: &str) -> Kind {
        match s {
            "vm" => Kind::Vm,
            "lxc" => Kind::Lxc,
            "docker" => Kind::Docker,
            "service" => Kind::Service,
            "user-service" => Kind::UserService,
            "kernel" => Kind::Kernel,
            _ => Kind::Program,
        }
    }
    /// The chip shown next to the name.
    pub fn chip(self) -> &'static str {
        match self {
            Kind::Vm => "VM",
            Kind::Lxc => "LXC",
            Kind::Docker => "Docker",
            Kind::Service => "Service",
            Kind::UserService => "User service",
            Kind::Kernel => "Kernel",
            Kind::Program => "Program",
        }
    }
}

/// Names the server itself reports for its VMs and containers.
#[derive(Clone, Debug, Default)]
pub struct HostNames {
    /// Proxmox VM id to name (`qm list`).
    pub vms: HashMap<String, String>,
    /// Proxmox container id to name (`pct list`).
    pub lxc: HashMap<String, String>,
    /// Full Docker container id to (name, image).
    pub containers: HashMap<String, (String, String)>,
}

#[derive(Clone, Debug, Default)]
pub struct Group {
    pub key: String,
    pub kind: Kind,
    pub label: String,
    /// Program name of the heaviest process in the group.
    pub main: String,
    pub cmdline: String,
    pub users: Vec<String>,
    /// Share of the whole server's CPU, 100 = every core busy.
    pub cpu: f64,
    pub mem: u64,
    pub procs: i64,
    pub explain: String,
    pub(crate) main_weight: f64,
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let rest = &s[i..];
    Some(&rest[..rest.find(end).unwrap_or(rest.len())])
}

fn is_hex_id(s: &str) -> bool {
    s.len() >= 12 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Works out the group key, kind and display name for one process.
pub fn classify(comm: &str, kernel_thread: bool, meta: Option<&Meta>, names: &HostNames) -> (String, Kind, String) {
    if kernel_thread {
        return ("kernel".into(), Kind::Kernel, "Linux kernel".into());
    }
    let cg = meta.map(|m| m.cgroup.as_str()).unwrap_or("");
    let path = cg.split_once("::").map(|(_, p)| p).or_else(|| cg.rsplit_once(':').map(|(_, p)| p)).unwrap_or("");

    // Proxmox VMs: /qemu.slice/101.scope
    if let Some(id) = between(path, "/qemu.slice/", ".scope") {
        let name = names.vms.get(id).cloned().or_else(|| meta.and_then(|m| vm_name_from_cmdline(&m.cmdline)));
        let label = match name {
            Some(n) => format!("VM {id} {n}"),
            None => format!("VM {id}"),
        };
        return (format!("vm:{id}"), Kind::Vm, label);
    }
    // Proxmox / LXC containers: /lxc/105/... or /lxc.payload.105/...
    if let Some(id) = between(path, "/lxc/", "/").or_else(|| between(path, "/lxc.payload.", "/")).or_else(|| between(path, "/lxc.monitor.", "/")) {
        let id = id.trim_end_matches(".scope");
        let label = match names.lxc.get(id) {
            Some(n) => format!("CT {id} {n}"),
            None => format!("Container {id}"),
        };
        return (format!("lxc:{id}"), Kind::Lxc, label);
    }
    // Docker: /system.slice/docker-<id>.scope or /docker/<id>
    let docker_id = between(path, "docker-", ".scope").or_else(|| between(path, "/docker/", "/"));
    if let Some(id) = docker_id.filter(|id| is_hex_id(id)) {
        let label = match names.containers.get(id) {
            Some((name, _)) => name.clone(),
            None => format!("container {}", &id[..12]),
        };
        return (format!("docker:{id}"), Kind::Docker, label);
    }
    // systemd services, system-wide or per user.
    let unit = path.rsplit('/').next().unwrap_or("");
    if let Some(name) = unit.strip_suffix(".service") {
        let name = name.trim_end_matches('@');
        if path.contains("/user.slice/") && !path.contains("/user@") {
            // A login session's own processes; fall through to the program name.
        } else if path.contains("/user@") {
            if !unit.starts_with("user@") {
                return (format!("user-service:{unit}"), Kind::UserService, name.to_string());
            }
        } else {
            return (format!("service:{unit}"), Kind::Service, name.to_string());
        }
    }
    if unit == "init.scope" && comm == "systemd" {
        return ("service:init".into(), Kind::Service, "systemd".into());
    }
    let cmdline = meta.map(|m| m.cmdline.as_str()).unwrap_or("");
    let prog = program_name(&program_of(comm, cmdline), cmdline);
    (format!("program:{prog}"), Kind::Program, prog)
}

/// `kvm ... -name jsystems-web,debug-threads=on ...` gives the VM name.
fn vm_name_from_cmdline(cmd: &str) -> Option<String> {
    let rest = cmd.split(" -name ").nth(1)?;
    let v = rest.split_whitespace().next()?;
    Some(v.split(',').next()?.trim_start_matches("guest=").to_string())
}

/// Groups worker processes under one name ("php-fpm: pool www" is php-fpm).
fn program_name(comm: &str, cmdline: &str) -> String {
    let c = comm.trim();
    for prefix in ["php-fpm", "postgres", "nginx", "apache2", "httpd", "kworker", "pvedaemon", "pveproxy", "spiceproxy", "pve-ha"] {
        if c.starts_with(prefix) {
            return prefix.to_string();
        }
    }
    if c.is_empty() {
        return cmdline.split_whitespace().next().unwrap_or("unknown").rsplit('/').next().unwrap_or("unknown").to_string();
    }
    c.to_string()
}

/// The script or jar an interpreter is running, if the command line says.
fn target(cmdline: &str) -> Option<String> {
    let mut args = cmdline.split_whitespace().skip(1);
    let mut jar_next = false;
    while let Some(a) = args.next() {
        if jar_next {
            return Some(a.rsplit('/').next().unwrap_or(a).to_string());
        }
        if a == "-jar" {
            jar_next = true;
            continue;
        }
        if a == "-m" {
            return args.next().map(|m| format!("module {m}"));
        }
        if a.starts_with('-') {
            continue;
        }
        let short = a.rsplit('/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/");
        return Some(short);
    }
    None
}

fn known(name: &str) -> Option<&'static str> {
    let n = name.trim_end_matches(".service").to_lowercase();
    let n = n.as_str();
    let d = match n {
        "kvm" | "qemu-system-x86_64" | "qemu-system-aarch64" => "A virtual machine. Its memory is the RAM given to the guest; look inside the VM to see what it runs.",
        "dockerd" | "docker" => "Docker's background service. It starts and watches your containers.",
        "containerd" | "containerd-shim" | "containerd-shim-runc-v2" => "Runs containers for Docker. Each running container has a small shim process.",
        "mysqld" | "mariadbd" | "mysql" | "mariadb" => "MySQL / MariaDB database. It keeps hot data in memory on purpose (innodb_buffer_pool_size).",
        "postgres" | "postgresql" => "PostgreSQL database. Its shared buffers count once per worker here, so real use is lower.",
        "redis-server" | "redis" | "valkey-server" => "Redis, an in-memory store. Its memory is the data it holds.",
        "mongod" => "MongoDB database. It uses spare memory as cache.",
        "nginx" => "nginx web server or reverse proxy.",
        "apache2" | "httpd" => "Apache web server.",
        "caddy" => "Caddy web server and HTTPS proxy.",
        "traefik" => "Traefik reverse proxy. It routes web traffic to your containers.",
        "haproxy" => "HAProxy load balancer.",
        "php-fpm" => "PHP-FPM, which runs your PHP sites. Several small workers are normal.",
        "php" => "A PHP program.",
        "node" | "nodejs" => "A Node.js app.",
        "bun" => "A Bun (JavaScript) app.",
        "deno" => "A Deno (JavaScript) app.",
        "python" | "python3" | "python3.12" | "python3.11" | "gunicorn" | "uvicorn" => "A Python app.",
        "java" => "A Java app. Java reserves its heap up front, so memory looks high even when idle.",
        "beam.smp" => "An Erlang/Elixir app (often RabbitMQ).",
        "sshd" => "The SSH server. Each open SSH login has its own small process.",
        "sshd-session" => "Open SSH logins, including JServer's own.",
        "rrdcached" => "Stores the graph data the Proxmox web interface shows.",
        "proxmox-firewall" => "Proxmox's newer nftables firewall service.",
        "zfs-zed" => "ZFS event daemon.",
        "postfix@-" => "Postfix mail server.",
        "chrony" => "Keeps the clock in sync.",
        "ssh" => "The SSH server. Each open SSH login has its own small process.",
        "multipathd" | "multipath-tools" => "Disk multipath service.",
        "lxc-monitord" | "lxc" => "Proxmox container monitor.",
        "watchdog-mux" => "Proxmox watchdog for high availability.",
        "qmeventd" => "Watches Proxmox VMs start and stop.",
        "pveupload-cleanup" | "pvebanner" => "Proxmox housekeeping.",
        "cloud-init" => "First-boot setup for cloud VMs.",
        "systemd" | "init" => "systemd starts and supervises everything else.",
        "systemd-journald" | "journald" => "systemd's log collector.",
        "rsyslogd" => "System log writer.",
        "cron" | "crond" => "Runs scheduled jobs.",
        "snapd" => "Ubuntu's snap package service.",
        "unattended-upgrades" => "Ubuntu's automatic security updates. It mostly waits.",
        "apt" | "apt-get" | "dpkg" => "Installing system updates right now.",
        "pvescheduler" => "Proxmox's scheduler for backups and replication jobs.",
        "pve-ha-crm" | "pve-ha-lrm" => "Proxmox high availability service.",
        "pve-lxc-syscalld" | "lxcfs" => "Helps Proxmox containers see their own resources.",
        "ksmtuned" => "Merges identical memory pages between VMs to save RAM.",
        "udisks2" | "udisksd" => "Disk management service for desktops. Not needed on most servers.",
        "modemmanager" => "Mobile modem service. Not needed on most servers.",
        "dbus" | "dbus-daemon" => "Message bus that system services use to talk to each other.",
        "systemd-logind" => "Tracks who is logged in.",
        "irqbalance" => "Spreads hardware interrupts across CPU cores.",
        "smartd" | "smartmontools" => "Watches disk health (SMART).",
        "postfix" | "master" => "Postfix mail server.",
        "rpcbind" => "Needed for NFS shares.",
        "iscsid" => "iSCSI storage client.",
        "lvm2-monitor" | "dmeventd" => "Watches LVM volumes.",
        "accounts-daemon" => "User account service.",
        "fail2ban-server" | "fail2ban" => "fail2ban, which blocks IPs that keep failing to log in.",
        "pveproxy" => "The Proxmox web interface and API.",
        "pvedaemon" => "Proxmox's management service (runs tasks for the web interface).",
        "pvestatd" => "Proxmox's stats collector for VMs and storage.",
        "pve-cluster" | "pmxcfs" => "Proxmox's cluster file system (/etc/pve).",
        "pve-firewall" => "Proxmox's firewall service.",
        "pve-ha" => "Proxmox high availability service.",
        "spiceproxy" => "Proxmox SPICE console proxy.",
        "corosync" => "Proxmox cluster messaging.",
        "zed" => "ZFS event daemon.",
        "ceph-osd" => "Ceph storage daemon, one per disk.",
        "tailscaled" => "Tailscale VPN.",
        "wireguard" | "wg-quick" => "WireGuard VPN.",
        "elasticsearch" => "Elasticsearch search engine. It reserves a large Java heap.",
        "clickhouse-server" => "ClickHouse analytics database.",
        "minecraft" => "A Minecraft server.",
        "coolify" => "Coolify, the self-hosted deploy platform.",
        "qemu-ga" => "QEMU guest agent, lets the host see inside this VM.",
        "polkitd" => "Permission service for system tools.",
        "networkd-dispatcher" | "systemd-networkd" | "networkmanager" => "Network setup service.",
        "systemd-resolved" => "DNS lookups for this server.",
        "systemd-udevd" => "Handles devices being added or removed.",
        "chronyd" | "systemd-timesyncd" | "ntpd" => "Keeps the clock in sync.",
        "t3code" => "T3 Code, an agent coding server.",
        _ => return None,
    };
    Some(d)
}

/// A one-line explanation for a group.
pub fn explain(g: &Group) -> String {
    match g.kind {
        Kind::Kernel => "Work the Linux kernel does itself: disks, network, file systems (ZFS shows up here).".into(),
        Kind::Vm => known("kvm").unwrap().into(),
        Kind::Lxc => "A Proxmox container. Look inside it to see which program uses the most.".into(),
        Kind::Docker => {
            let run = interpreter_target(g).map(|r| format!(" Running {r}.")).unwrap_or_default();
            match known(&g.main) {
                Some(d) => format!("Runs {} in Docker. {d}{run}", g.main),
                None if !g.main.is_empty() => format!("Runs {} in Docker.{run}", g.main),
                None => "A Docker container.".into(),
            }
        }
        Kind::Service | Kind::UserService => {
            let base = known(&g.label).or_else(|| known(&g.main));
            let run = interpreter_target(g).map(|r| format!(" Running {r}.")).unwrap_or_default();
            let who = if g.kind == Kind::UserService { "A service run by a user's own systemd" } else { "A systemd service" };
            match base {
                Some(d) => format!("{d}{run}"),
                None if g.main.is_empty() || g.main == g.label => format!("{who}.{run}"),
                None => format!("{who}. Its main program is {}.{run}", g.main),
            }
        }
        Kind::Program => {
            let base = known(&g.label).or_else(|| known(&g.main));
            let run = interpreter_target(g).map(|r| format!(" Running {r}.")).unwrap_or_default();
            match base {
                Some(d) => format!("{d}{run}"),
                None if !g.users.is_empty() => format!("A program started by {}, not by a service.{run}", g.users.join(", ")),
                None => format!("A program not started by a service.{run}"),
            }
        }
    }
}

const INTERPRETERS: [&str; 11] = ["node", "nodejs", "python", "python3", "java", "bun", "deno", "php", "ruby", "perl", "dotnet"];

/// The script or jar, when the group's main program is an interpreter.
fn interpreter_target(g: &Group) -> Option<String> {
    let main = g.main.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    if INTERPRETERS.contains(&g.main.as_str()) || INTERPRETERS.contains(&main) { target(&g.cmdline) } else { None }
}

/// The program a command line starts, for naming a group's main process.
/// Threads can rename their process (Python's "MainThread"), so the command line wins.
pub fn program_of(comm: &str, cmdline: &str) -> String {
    let argv0 = cmdline.split_whitespace().next().unwrap_or("");
    let base = argv0.rsplit('/').next().unwrap_or("").trim_end_matches(':');
    if base.is_empty() || base.starts_with('-') || base.contains(['@', '(', '[']) { comm.to_string() } else { base.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(cg: &str, cmd: &str) -> Meta {
        Meta { cgroup: cg.into(), user: "root".into(), cmdline: cmd.into() }
    }

    #[test]
    fn names_vms_containers_and_services() {
        let mut names = HostNames::default();
        names.vms.insert("101".into(), "jsystems-web".into());
        let (k, kind, label) = classify("kvm", false, Some(&meta("0::/qemu.slice/101.scope", "/usr/bin/kvm -id 101 -name jsystems-web,x")), &names);
        assert_eq!((k.as_str(), kind, label.as_str()), ("vm:101", Kind::Vm, "VM 101 jsystems-web"));
        let (_, kind, label) = classify("kvm", false, Some(&meta("0::/qemu.slice/100.scope", "/usr/bin/kvm -id 100 -name backend,debug-threads=on")), &names);
        assert_eq!((kind, label.as_str()), (Kind::Vm, "VM 100 backend"));
        let id = "21c9bb2336c929137e4b0a9572ad9090104fda91603d72018a0916c3f2b12f78";
        let (_, kind, label) = classify("php", false, Some(&meta(&format!("0::/system.slice/docker-{id}.scope"), "php artisan")), &names);
        assert_eq!((kind, label.as_str()), (Kind::Docker, "container 21c9bb2336c9"));
        let (_, kind, label) = classify("nginx", false, Some(&meta("0::/system.slice/nginx.service", "nginx: worker")), &names);
        assert_eq!((kind, label.as_str()), (Kind::Service, "nginx"));
        let (_, kind, label) = classify("node", false, Some(&meta("0::/user.slice/user-1000.slice/user@1000.service/app.slice/t3code.service", "node t3")), &names);
        assert_eq!((kind, label.as_str()), (Kind::UserService, "t3code"));
        let (_, kind, label) = classify("bash", false, Some(&meta("0::/user.slice/user-1000.slice/session-4.scope", "-bash")), &names);
        assert_eq!((kind, label.as_str()), (Kind::Program, "bash"));
        let (_, kind, _) = classify("kworker/0:1", true, None, &names);
        assert_eq!(kind, Kind::Kernel);
    }

    #[test]
    fn explains_interpreters_with_their_script() {
        let g = Group { kind: Kind::Program, label: "node".into(), main: "node".into(), cmdline: "node /srv/app/server.js --port 3000".into(), ..Default::default() };
        assert_eq!(explain(&g), "A Node.js app. Running app/server.js.");
    }
}
