//! Find the processes of one launched Chrome, and wait for them to exit.
//!
//! Tracks one Chrome, not every Chrome on the machine: other browsers (a
//! developer's own, another test's) come and go during a run. Three sources,
//! because no single one sees the whole tree:
//!
//! - `SystemInfo.getProcessInfo`: Chrome's own list of its browser, GPU,
//!   utility and renderer processes. Doesn't list zygotes.
//! - The command line: every process Chrome forks carries the browser's
//!   `--user-data-dir=` argument (on Linux, at least), zygotes included.
//!   Chrome on Linux rewrites its command line into one space-separated
//!   string, so a lone argument is split on spaces: a profile path with a
//!   space in it is then known by its first word, which is still unique for
//!   zendriver's temporary profiles.
//! - `--crashpad-handler-pid=N` in those command lines: the crash handler
//!   double-forks away from the tree and carries neither of the above.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::json;
use sysinfo::{
    Pid, Process, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind,
};
use zendriver::Browser;

/// How often [`ChromeTree::wait_gone`] looks again.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A process seen alive. The start time tells a reused PID apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Seen {
    pid: Pid,
    start_time: u64,
}

/// The process tree of one launched Chrome.
#[derive(Clone, Debug)]
pub struct ChromeTree {
    /// The browser process's `--user-data-dir=...` argument.
    user_data_dir_arg: String,
    /// Every process [`ChromeTree::snapshot`] has seen.
    seen: HashSet<Seen>,
}

impl ChromeTree {
    /// Identify the Chrome behind `browser`. Panics if it can't, since a
    /// tracker that sees nothing would pass every leak check.
    pub async fn of(browser: &Browser) -> Self {
        let browser_pid = chrome_process_ids(browser)
            .await
            .into_iter()
            .find_map(|(kind, pid)| (kind == "browser").then_some(pid))
            .expect("SystemInfo.getProcessInfo lists no browser process");
        let system = scan();
        let process = system
            .process(browser_pid)
            .unwrap_or_else(|| panic!("Chrome browser process {browser_pid} isn't running"));
        let user_data_dir_arg = args(process)
            .into_iter()
            .find(|arg| arg.starts_with("--user-data-dir="))
            .unwrap_or_else(|| {
                panic!(
                    "Chrome browser process {browser_pid} has no --user-data-dir argument: {:?}",
                    process.cmd()
                )
            });
        let mut tree = Self {
            user_data_dir_arg,
            seen: HashSet::new(),
        };
        tree.remember(&system, [browser_pid]);
        tree
    }

    /// Record every process of this Chrome alive now, and return how many
    /// there are. Call it while the browser is still up, right before
    /// closing it, so processes started since [`ChromeTree::of`] count.
    pub async fn snapshot(&mut self, browser: &Browser) -> usize {
        let listed: Vec<Pid> = chrome_process_ids(browser)
            .await
            .into_iter()
            .map(|(_, pid)| pid)
            .collect();
        let system = scan();
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
            let system = scan();
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
            system
                .processes()
                .values()
                .filter(|p| self.carries_profile(p))
                .map(Process::pid),
        );
        let crashpad: Vec<Pid> = pids
            .iter()
            .filter_map(|pid| system.process(*pid))
            .filter_map(crashpad_handler_pid)
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
        system
            .processes()
            .values()
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
        args(process).contains(&self.user_data_dir_arg)
    }
}

/// `(type, pid)` of each process Chrome reports for itself.
async fn chrome_process_ids(browser: &Browser) -> Vec<(String, Pid)> {
    let reply = browser
        .cdp()
        .call_raw("SystemInfo.getProcessInfo", json!({}), None)
        .await
        .expect("SystemInfo.getProcessInfo");
    let infos = reply["processInfo"]
        .as_array()
        .expect("SystemInfo.getProcessInfo reply has a processInfo array");
    infos
        .iter()
        .map(|info| {
            let kind = info["type"].as_str().expect("processInfo type").to_string();
            let pid = info["id"]
                .as_u64()
                .and_then(|id| u32::try_from(id).ok())
                .expect("processInfo id is a u32");
            (kind, Pid::from_u32(pid))
        })
        .collect()
}

/// The `N` of a `--crashpad-handler-pid=N` argument.
fn crashpad_handler_pid(process: &Process) -> Option<Pid> {
    args(process).iter().find_map(|arg| {
        arg.strip_prefix("--crashpad-handler-pid=")?
            .parse()
            .ok()
            .map(Pid::from_u32)
    })
}

/// `process`'s arguments, with a lone argument split on spaces (see the
/// module docs).
fn args(process: &Process) -> Vec<String> {
    match process.cmd() {
        [only] => only
            .to_string_lossy()
            .split(' ')
            .map(str::to_string)
            .collect(),
        many => many
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect(),
    }
}

/// Every process on the machine, with command lines.
fn scan() -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::new()
            .with_cmd(UpdateKind::Always)
            .with_exe(UpdateKind::Always),
    );
    system
}

fn describe(process: &Process) -> String {
    let zombie = if process.status() == ProcessStatus::Zombie {
        " (zombie)"
    } else {
        ""
    };
    let cmd: Vec<_> = process.cmd().iter().map(|a| a.to_string_lossy()).collect();
    let cmd: String = cmd.join(" ").chars().take(200).collect();
    format!("pid {}{zombie}: {:?} {cmd}", process.pid(), process.name())
}
