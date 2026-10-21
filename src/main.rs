#![windows_subsystem = "windows"]

mod autostart;
mod capture;
mod cmd;
mod collector;
mod db;
mod describe;
mod fmt;
mod insights;
mod paths;
mod probe;
mod remote;
mod sshconf;
mod theme;
mod ui;
mod update;

fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// `JServer.exe --report [hours]` prints the same findings the server pages show.
fn report(hours: i64) -> Result<(), String> {
    attach_console();
    let conn = db::open(&paths::db_path()).map_err(|e| e.to_string())?;
    let to = chrono::Utc::now().timestamp();
    let from = to - hours * 3600;
    println!("\nJServer report, last {hours} h  ({})", paths::db_path().display());
    println!("Background monitor running: {}", collector::is_running());
    let mut servers = db::servers(&conn).map_err(|e| e.to_string())?;
    servers.sort_by(|a, b| a.alias.cmp(&b.alias));
    for s in servers {
        println!("\n== {}  [{}] {}", s.alias, if s.label.is_empty() { "Not checked" } else { &s.label }, s.detail);
        for i in insights::compute(&conn, &s, from, to).map_err(|e| e.to_string())? {
            println!("  [{:?}] {}\n      {}", i.level, i.title, i.detail);
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--collector") => {
            if let Err(e) = collector::run() {
                collector::log(&format!("collector error: {e}"));
            }
        }
        Some("--report") => {
            let hours = args.get(1).and_then(|h| h.parse().ok()).unwrap_or(24);
            if let Err(e) = report(hours) {
                eprintln!("report failed: {e}");
                std::process::exit(1);
            }
        }
        Some("--probe") => {
            attach_console();
            let Some(alias) = args.get(1) else {
                eprintln!("usage: JServer.exe --probe <host alias>");
                std::process::exit(2);
            };
            if let Err(e) = collector::probe_cli(alias) {
                eprintln!("{alias}: {e}");
                std::process::exit(1);
            }
        }
        Some("--keys") => {
            attach_console();
            let scan = sshconf::scan();
            for h in &scan.hosts {
                println!("host {:<20} {:<28} via {:<12} keys {:?}", h.alias, h.address(), h.proxy_jump, h.identity_files);
            }
            for k in &scan.keys {
                println!("key  {:<36} {:<8} {:>5} {} used by {:?} private {}", k.name, k.kind, k.bits, k.fingerprint, k.used_by, k.has_private);
                for f in &k.findings {
                    println!("       [{:?}] {}", f.level, f.text);
                }
            }
            for f in &scan.findings {
                println!("[{:?}] {}", f.level, f.text);
            }
            println!("ssh-agent running {} keys {} {}", scan.agent.running, scan.agent.keys, scan.agent.note);
        }
        Some("--capture") => {
            attach_console();
            let dir = std::path::PathBuf::from(args.get(1).map(String::as_str).unwrap_or("captures"));
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = ui::run_capture(dir) {
                eprintln!("capture failed: {e}");
                std::process::exit(1);
            }
        }
        Some(flag @ ("--check-update" | "--update")) => {
            attach_console();
            if let Err(e) = update::cli(flag == "--update") {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        _ => {
            if let Err(e) = ui::run() {
                collector::log(&format!("ui error: {e}"));
            }
        }
    }
}
