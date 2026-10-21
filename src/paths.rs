use std::path::PathBuf;

fn appdata() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// `%APPDATA%\JServer`, created on first use.
pub fn data_dir() -> PathBuf {
    let dir = appdata().join("JServer");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn db_path() -> PathBuf {
    data_dir().join("jserver.db")
}

pub fn lock_path() -> PathBuf {
    data_dir().join("collector.lock")
}

/// `~/.ssh`, where ssh itself looks for config and keys.
pub fn ssh_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
}
