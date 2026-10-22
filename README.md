# JServer

A Windows app that watches the Linux servers in your `~/.ssh/config` over SSH, keeps a history of how they perform, and tells you what is using their memory and CPU.

It is JPerformance for servers.

## What it does

- **Servers**: every host in `~/.ssh/config` gets a card with CPU, memory, disk and swap, uptime, and what is using the most memory and CPU right now. Hosts reached through a `ProxyJump` work too.
- **One server**: CPU, memory, swap, load, network and disk I/O, a memory breakdown (programs, ZFS cache, file cache, free), and a ranked list of what is using the server. Processes are grouped by what started them: a Proxmox VM ("VM 100 backend-services"), an LXC or Docker container, a systemd service, or a program. Each one gets a short explanation of what it is.
- **History**: CPU, memory, load, network and disk I/O charts for the last hour up to 30 days. Hover a chart to see the numbers on every chart at that moment, and what was running.
- **Findings**: memory running low, out-of-memory kills, disks filling up, long CPU load, waiting on disk, CPU taken by the VM host, a pending reboot, and logins that fail.
- **SSH keys**: every key in `~/.ssh` with its type, fingerprint and the hosts that use it. Flags weak keys, keys other Windows users can read, hosts that point at a missing key file, keys a server rejected, and whether ssh-agent is running.
- **Background monitoring**: `JServer.exe --collector` keeps one SSH login open per server and samples every 5 seconds, even after you close the window. Tick **Start with Windows** to keep watching after a restart. **Pause monitoring** on a server logs out of it.

## Safety

- Only read-only commands run on your servers (`/proc`, `/sys`, `df`, `docker ps`, `qm list`). Nothing is installed or written there.
- Logins use `BatchMode=yes` and `StrictHostKeyChecking=yes`: JServer never prompts and never accepts a new or changed host key.
- A login that fails is not retried on its own, so a rejected key does not get you banned by fail2ban. The server's card shows why and a Retry button.
- Only public key files (`*.pub`) are read.
- Data stays on your PC in `%APPDATA%\JServer\jserver.db` (SQLite). History older than 30 days is removed.

## Requirements

Windows 10 or 11 with the OpenSSH client (`ssh.exe`, built in), and hosts in `~/.ssh/config` that you can log in to with a key and no password prompt. Servers must run Linux.

## Build

Requires Rust (MSVC toolchain).

```
cargo build --release
```

The app is `target\release\JServer.exe`.

## Command line

```
JServer.exe                  open the app (starts background monitoring)
JServer.exe --collector      run only the background monitor
JServer.exe --probe <host>   log in once and print what JServer sees on that host
JServer.exe --keys           print the SSH key and host checks
JServer.exe --report [h]     print the findings for every server over the last h hours
JServer.exe --capture <dir>  save PNGs of each page (for checking the UI)
JServer.exe --check-update   say whether a newer release is available
JServer.exe --update         download, verify and install the newest release
```

## Notes

- CPU per group is a share of the whole server, so 100% means every core is busy.
- Memory per group is resident memory (RSS). Processes that share memory, like PostgreSQL workers, can add up to more than they really use.
- Docker container names need a login that can run `docker ps` (root or the docker group). Otherwise containers show by ID.

## License

MIT. See [LICENSE](LICENSE). IBM Plex Sans is under the SIL Open Font License (see `assets/fonts`).
