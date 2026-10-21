//! Reads ~/.ssh: the hosts in ~/.ssh/config and the key pairs next to it.
//! Only public key files (*.pub) are opened. Private keys are never read; for
//! them JServer only checks that the file exists and who Windows lets read it.

use crate::cmd;
use crate::paths;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Host {
    pub alias: String,
    pub hostname: String,
    pub user: String,
    pub port: u16,
    pub identity_files: Vec<String>,
    pub proxy_jump: String,
}

impl Host {
    /// `user@host:port`, the way you would type it.
    pub fn address(&self) -> String {
        let mut s = String::new();
        if !self.user.is_empty() {
            s.push_str(&self.user);
            s.push('@');
        }
        s.push_str(&self.hostname);
        if self.port != 22 {
            s.push_str(&format!(":{}", self.port));
        }
        s
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Info,
    Warn,
    Danger,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Key {
    pub name: String,
    pub bits: u32,
    pub fingerprint: String,
    pub comment: String,
    pub kind: String,
    pub error: String,
    pub has_private: bool,
    /// When the .pub file was last written (unix seconds).
    pub created: i64,
    pub used_by: Vec<String>,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Debug, Default)]
pub struct Agent {
    pub running: bool,
    pub keys: usize,
    pub note: String,
}

#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub ssh_dir: PathBuf,
    pub hosts: Vec<Host>,
    pub keys: Vec<Key>,
    pub agent: Agent,
    /// Problems that are about the config rather than one key.
    pub findings: Vec<Finding>,
}

fn config_path() -> PathBuf {
    paths::ssh_dir().join("config")
}

fn read_config(path: &Path, depth: u32, out: &mut Vec<String>) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        let line = line.trim();
        let (key, rest) = line.split_once(|c: char| c.is_whitespace() || c == '=').unwrap_or((line, ""));
        let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim();
        if key.eq_ignore_ascii_case("Host") {
            for a in rest.split_whitespace() {
                let a = a.trim_matches('"');
                if !a.contains(['*', '?', '!']) && !out.iter().any(|x| x == a) {
                    out.push(a.to_string());
                }
            }
        } else if key.eq_ignore_ascii_case("Include") && depth < 4 {
            for inc in rest.split_whitespace() {
                let p = expand(inc.trim_matches('"'));
                let p = if p.is_absolute() { p } else { paths::ssh_dir().join(p) };
                read_config(&p, depth + 1, out);
            }
        }
    }
}

/// Host aliases from ~/.ssh/config, without wildcard patterns.
pub fn aliases() -> Vec<String> {
    let mut out = Vec::new();
    read_config(&config_path(), 0, &mut out);
    out
}

fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        let home = paths::ssh_dir();
        return home.parent().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(p));
    }
    PathBuf::from(p)
}

fn same_file(a: &str, b: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    norm(&expand(a)) == norm(b)
}

/// The settings ssh would actually use for an alias (`ssh -G`).
pub fn resolve(alias: &str) -> Host {
    let out = cmd::run("ssh", &["-G", alias], Duration::from_secs(8));
    let get = |k: &str| {
        out.stdout
            .lines()
            .find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix(' ')))
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let ids = out.stdout.lines().filter_map(|l| l.strip_prefix("identityfile ")).map(|s| s.trim().to_string()).collect();
    let jump = get("proxyjump");
    Host {
        alias: alias.to_string(),
        hostname: Some(get("hostname")).filter(|h| !h.is_empty()).unwrap_or_else(|| alias.to_string()),
        user: get("user"),
        port: get("port").parse().unwrap_or(22),
        identity_files: ids,
        proxy_jump: if jump == "none" { String::new() } else { jump },
    }
}

/// Who besides you can read a private key file. OpenSSH refuses keys that
/// other users can read, so this is worth flagging.
fn loose_permissions(private: &Path) -> Option<String> {
    let out = cmd::run("icacls", &[&private.to_string_lossy()], Duration::from_secs(5));
    if !out.ok {
        return None;
    }
    let wide = ["Everyone:", "BUILTIN\\Users:", "NT AUTHORITY\\Authenticated Users:"];
    let who: Vec<&str> = wide.iter().filter(|w| out.stdout.contains(*w)).map(|w| w.trim_end_matches(':')).collect();
    if who.is_empty() { None } else { Some(who.join(", ")) }
}

fn key(pub_path: &Path, hosts: &[Host]) -> Key {
    let name = pub_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let private = pub_path.with_extension("");
    let out = cmd::run("ssh-keygen", &["-l", "-f", &pub_path.to_string_lossy()], Duration::from_secs(8));
    // "256 SHA256:abc comment words (ED25519)"
    let line = out.stdout.trim();
    let parsed = (|| {
        let (bits, rest) = line.split_once(' ')?;
        let (fp, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let open = rest.rfind('(')?;
        let kind = rest[open + 1..].trim_end_matches(')').to_string();
        Some((bits.parse::<u32>().ok()?, fp.to_string(), rest[..open].trim().to_string(), kind))
    })();
    let created = std::fs::metadata(pub_path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let has_private = private.exists();
    let used_by: Vec<String> = hosts
        .iter()
        .filter(|h| h.identity_files.iter().any(|f| same_file(f, &private)))
        .map(|h| h.alias.clone())
        .collect();
    let (bits, fingerprint, comment, kind) = parsed.clone().unwrap_or((0, String::new(), String::new(), "unknown".into()));

    let mut findings = Vec::new();
    if parsed.is_none() {
        findings.push(Finding { level: Level::Danger, text: "ssh-keygen could not read this public key.".into() });
    }
    match kind.as_str() {
        "DSA" => findings.push(Finding { level: Level::Danger, text: "DSA keys are no longer accepted by current OpenSSH. Make a new ed25519 key.".into() }),
        "RSA" if bits < 2048 => findings.push(Finding { level: Level::Danger, text: format!("RSA {bits} is too short to be safe. Make a new ed25519 key.") }),
        "RSA" if bits < 3072 => findings.push(Finding { level: Level::Info, text: format!("RSA {bits} still works, but ed25519 is shorter and stronger.") }),
        _ => {}
    }
    if !has_private {
        findings.push(Finding { level: Level::Warn, text: "Only the public half is here, so this PC cannot log in with it.".into() });
    } else if let Some(who) = loose_permissions(&private) {
        findings.push(Finding {
            level: Level::Danger,
            text: format!("The private key can be read by {who}. ssh refuses keys like this; remove those users in the file's Properties > Security."),
        });
    }
    if used_by.is_empty() && has_private {
        findings.push(Finding { level: Level::Info, text: "Not used by any host in ~/.ssh/config.".into() });
    }
    Key {
        name,
        bits,
        fingerprint,
        comment,
        kind,
        error: if parsed.is_none() { (out.stderr + &out.stdout).trim().chars().take(200).collect() } else { String::new() },
        has_private,
        created,
        used_by,
        findings,
    }
}

fn agent() -> Agent {
    let out = cmd::run("ssh-add", &["-l"], Duration::from_secs(5));
    if out.ok {
        return Agent { running: true, keys: out.stdout.lines().filter(|l| !l.trim().is_empty()).count(), note: String::new() };
    }
    let text = (out.stderr + &out.stdout).to_lowercase();
    if text.contains("no identities") {
        return Agent { running: true, keys: 0, note: String::new() };
    }
    let svc = cmd::run("sc", &["qc", "ssh-agent"], Duration::from_secs(5));
    let note = if svc.stdout.contains("DISABLED") { "The OpenSSH Authentication Agent service is disabled in Windows." } else { "" };
    Agent { running: false, keys: 0, note: note.into() }
}

/// Reads ~/.ssh and checks every key. Takes a few seconds (one ssh -G per host).
pub fn scan() -> Scan {
    let dir = paths::ssh_dir();
    let has_config = config_path().exists();
    let hosts: Vec<Host> = std::thread::scope(|s| {
        let handles: Vec<_> = aliases().into_iter().map(|a| s.spawn(move || resolve(&a))).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    let mut pubs: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "pub")).collect())
        .unwrap_or_default();
    pubs.sort();
    let keys: Vec<Key> = std::thread::scope(|s| {
        let hosts = &hosts;
        let handles: Vec<_> = pubs.iter().map(|p| s.spawn(move || key(p, hosts))).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    let mut findings = Vec::new();
    if !has_config {
        findings.push(Finding { level: Level::Info, text: format!("There is no {}. Add Host entries there and JServer will watch them.", config_path().display()) });
    }
    for h in &hosts {
        for f in &h.identity_files {
            let p = expand(f);
            // ssh -G lists the default key names even when they do not exist; only flag ones set in config.
            let default = ["id_rsa", "id_ecdsa", "id_ecdsa_sk", "id_ed25519", "id_ed25519_sk", "id_xmss", "id_dsa"]
                .iter()
                .any(|d| p.file_name().is_some_and(|n| n == *d));
            if !p.exists() && !default {
                findings.push(Finding { level: Level::Danger, text: format!("{} uses {}, which does not exist.", h.alias, f) });
            }
        }
    }
    Scan { ssh_dir: dir, hosts, keys, agent: agent(), findings }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_host_aliases_without_patterns() {
        let dir = std::env::temp_dir().join(format!("jserver-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config");
        std::fs::write(&p, "Host a b\n  HostName 1.2.3.4\nHost *.lan !x\nhost=c\nHost a\n").unwrap();
        let mut out = Vec::new();
        read_config(&p, 0, &mut out);
        assert_eq!(out, vec!["a", "b", "c"]);
        let _ = std::fs::remove_dir_all(dir);
    }
}
