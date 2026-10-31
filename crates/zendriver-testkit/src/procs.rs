//! Find the processes of one launched Chrome, and wait for them to exit.
//!
//! Tracks one Chrome, not every Chrome on the machine: other browsers (a
//! developer's own, another test's) come and go during a run. Three sources,
//! because no single one sees the whole tree:
//!
//! - `SystemInfo.getProcessInfo`: Chrome's own list of its browser, GPU,
//!   utility and renderer processes. Doesn't list zygotes.
//! - The command line: every process Chrome forks carries the browser's
//!   `--user-data-dir=` switch (on Linux, at least), zygotes included.
//! - `--crashpad-handler-pid=N` in those command lines: the crash handler
//!   double-forks away from the tree and carries neither of the above.
//!
//! Not tracked: the crash handler's own monitor, a second
//! `chrome_crashpad_handler` that nothing ties to this Chrome but a socket.
//! It exits with the first one.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use serde_json::json;
use sysinfo::{
    Pid, Process, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind,
};
use zendriver::{Browser, CallError};

/// How often [`ChromeTree::wait_gone`] looks again.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// `(type, pid)` of each process Chrome reports for itself.
async fn chrome_process_ids(browser: &Browser) -> Result<Vec<(String, Pid)>, CallError> {
    let reply = browser
        .cdp()
        .call_raw("SystemInfo.getProcessInfo", json!({}), None)
        .await?;
    let infos = reply["processInfo"]
        .as_array()
        .expect("SystemInfo.getProcessInfo reply has a processInfo array");
    Ok(infos
        .iter()
        .map(|info| {
            let kind = info["type"].as_str().expect("processInfo type").to_string();
            let pid = info["id"]
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .expect("processInfo id is a u32");
            (kind, Pid::from_u32(pid))
        })
        .collect())
}

/// `process`'s command line as one string. Chrome on Linux rewrites its own
/// into one space-joined string, so this is the only form it reliably has.
fn command_line(process: &Process) -> String {
    let args: Vec<_> = process.cmd().iter().map(|a| a.to_string_lossy()).collect();
    args.join(" ")
}

/// The `--user-data-dir=...` switch in `line`. Its value may contain spaces,
/// so it runs up to the next ` --` switch, or the end.
fn user_data_dir_switch(line: &str) -> Option<&str> {
    let start = line.find(" --user-data-dir=")? + 1;
    let rest = &line[start..];
    Some(&rest[..rest.find(" --").unwrap_or(rest.len())])
}

/// The `N` of a `--crashpad-handler-pid=N` switch in `line`.
fn crashpad_handler_pid(line: &str) -> Option<Pid> {
    line.split(' ').find_map(|arg| {
        arg.strip_prefix("--crashpad-handler-pid=")?
            .parse()
            .ok()
            .map(Pid::from_u32)
    })
}

/// Every process on the machine, with command lines. Reads `/proc` and the
/// like for every process, so it runs off the async runtime.
async fn scan() -> System {
    tokio::task::spawn_blocking(|| {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::new()
                .with_cmd(UpdateKind::Always)
                .with_exe(UpdateKind::Always),
        );
        system
    })
    .await
    .expect("process scan")
}

/// The processes in `system`. On Linux, sysinfo also lists each thread as a
/// process with its owner's command line.
fn processes(system: &System) -> impl Iterator<Item = &Process> {
    system
        .processes()
        .values()
        .filter(|p| p.thread_kind().is_none())
}

fn describe(process: &Process) -> String {
    let zombie = if process.status() == ProcessStatus::Zombie {
        " (zombie)"
    } else {
        ""
    };
    let line: String = command_line(process).chars().take(200).collect();
    format!("pid {}{zombie}: {:?} {line}", process.pid(), process.name())
}

/// A process seen alive. The start time tells a reused PID apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Seen {
    pid: Pid,
    start_time: u64,
}

/// The process tree of one launched Chrome.
#[derive(Clone, Debug)]
pub struct ChromeTree {
    /// The browser process's `--user-data-dir=...` switch.
    user_data_dir_switch: String,
    /// Every process [`ChromeTree::snapshot`] has seen.
    seen: HashSet<Seen>,
}

impl ChromeTree {
    /// Identify the Chrome behind `browser`. Panics if it can't, since a
    /// tracker that sees nothing would pass every leak check.
    pub async fn of(browser: &Browser) -> Self {
        let browser_pid = chrome_process_ids(browser)
            .await
            .expect("SystemInfo.getProcessInfo")
            .into_iter()
            .find_map(|(kind, pid)| (kind == "browser").then_some(pid))
            .expect("SystemInfo.getProcessInfo lists no browser process");
        let system = scan().await;
        let process = system
            .process(browser_pid)
            .unwrap_or_else(|| panic!("Chrome browser process {browser_pid} isn't running"));
        let line = command_line(process);
        let switch = user_data_dir_switch(&line)
            .unwrap_or_else(|| panic!("Chrome has no --user-data-dir switch: {line}"));
        let dir = switch.trim_start_matches("--user-data-dir=");
        assert!(
            Path::new(dir).is_dir(),
            "can't tell where Chrome's --user-data-dir ends in {line:?}; took it as {dir:?}",
        );
        let mut tree = Self {
            user_data_dir_switch: switch.to_string(),
            seen: HashSet::new(),
        };
        tree.remember(&system, [browser_pid]);
        tree
    }

    /// Record every process of this Chrome alive now, and return how many
    /// there are. Call it while the browser is still up, right before
    /// closing it, so processes started since [`ChromeTree::of`] count. If
    /// CDP no longer answers, records what the command lines show.
    pub async fn snapshot(&mut self, browser: &Browser) -> usize {
        let listed = match chrome_process_ids(browser).await {
            Ok(ids) => ids.into_iter().map(|(_, pid)| pid).collect(),
            Err(e) => {
                eprintln!("ChromeTree::snapshot: no process list from Chrome ({e})");
                Vec::new()
            }
        };
        let system = scan().await;
        self.remember(&system, listed);
        self.survivors(&system).len()
    }

    /// Wait up to `within` for every process seen to exit, and for no
    /// process to carry this Chrome's profile directory. On timeout, returns
    /// a description of each survivor. A zombie counts as a survivor: it
    /// means whoever started it never reaped it.
    pub async fn wait_gone(&self, within: Duration) -> Result<(), Vec<String>> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let system = scan().await;
            let survivors = self.survivors(&system);
            if survivors.is_empty() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(survivors.into_iter().map(describe).collect());
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    /// Add `pids` and everything carrying our profile directory, plus the
    /// crash handlers they name, to [`ChromeTree::seen`].
    fn remember(&mut self, system: &System, pids: impl IntoIterator<Item = Pid>) {
        let mut pids: Vec<Pid> = pids.into_iter().collect();
        pids.extend(
            processes(system)
                .filter(|p| self.carries_profile(p))
                .map(Process::pid),
        );
        let crashpad: Vec<Pid> = pids
            .iter()
            .filter_map(|pid| system.process(*pid))
            .filter_map(|p| crashpad_handler_pid(&command_line(p)))
            .collect();
        pids.extend(crashpad);
        for pid in pids {
            if let Some(process) = system.process(pid) {
                self.seen.insert(Seen {
                    pid,
                    start_time: process.start_time(),
                });
            }
        }
    }

    /// Processes in `system` that were seen before or carry our profile
    /// directory.
    fn survivors<'a>(&self, system: &'a System) -> Vec<&'a Process> {
        processes(system)
            .filter(|p| {
                self.carries_profile(p)
                    || self.seen.contains(&Seen {
                        pid: p.pid(),
                        start_time: p.start_time(),
                    })
            })
            .collect()
    }

    fn carries_profile(&self, process: &Process) -> bool {
        user_data_dir_switch(&command_line(process)) == Some(self.user_data_dir_switch.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_data_dir_switch_keeps_spaces_in_the_path() {
        let line = "/opt/chrome --type=renderer --user-data-dir=/tmp/test profiles/zd-1 --lang=en";
        assert_eq!(
            user_data_dir_switch(line),
            Some("--user-data-dir=/tmp/test profiles/zd-1")
        );
        assert_eq!(
            user_data_dir_switch("/opt/chrome --user-data-dir=/tmp/zd-1"),
            Some("--user-data-dir=/tmp/zd-1")
        );
        assert_eq!(user_data_dir_switch("/opt/chrome --no-sandbox"), None);
    }
}
