//! `ballpop watch`: turns notifications from desktop AI apps (Claude,
//! ChatGPT, ...) into balls.
//!
//! Unpackaged apps can read toasts but cannot subscribe to the "new toast"
//! event, and reading the toast list costs ~20 ms of CPU. So the list is only
//! read while a watched app is actually busy (it is generating a reply) and
//! for a short while after; the busy test is one GetProcessTimes call per
//! process on handles we keep open, a few microseconds every POLL_MS.

use std::collections::{HashMap, HashSet};
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};

use windows::UI::Notifications::Management::{UserNotificationListener, UserNotificationListenerAccessStatus};
use windows::UI::Notifications::NotificationKinds;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::Diagnostics::ToolHelp::*;
use windows_sys::Win32::System::Threading::*;

use crate::wide;

/// Matched against the app's display name; also the exe-name prefix used
/// to find its processes and window, and the agent name for its logo.
const AGENTS: [&str; 6] = ["claude", "chatgpt", "grok", "perplexity", "copilot", "gemini"];
const POLL_MS: u32 = 2000;
/// CPU (ms per poll, all of an agent's processes) that counts as "busy".
const BUSY_CPU_MS: u64 = 40;
/// Polls to keep reading toasts after the last busy one (toasts land at the
/// very end of a reply).
const LINGER_POLLS: u32 = 10;
/// Polls between rescans for newly started agent processes.
const RESCAN_POLLS: u32 = 15;

pub unsafe fn run() {
    let _single = CreateMutexW(null(), 0, wide("Local\\BallPopWatch").as_ptr());
    if GetLastError() == ERROR_ALREADY_EXISTS {
        return;
    }
    let Ok(listener) = UserNotificationListener::Current() else { return };
    if listener.GetAccessStatus().ok() != Some(UserNotificationListenerAccessStatus::Allowed) {
        let granted = listener.RequestAccessAsync().and_then(|op| op.join());
        if granted.ok() != Some(UserNotificationListenerAccessStatus::Allowed) {
            return;
        }
    }
    let mut seen = HashSet::new();
    new_toasts(&listener, &mut seen); // toasts from before we started don't count
    let mut procs: HashMap<u32, (HANDLE, u64)> = HashMap::new();
    let mut active_left = 0;
    for tick in 0u32.. {
        if tick % RESCAN_POLLS == 0 {
            rescan(&mut procs);
        }
        Sleep(POLL_MS);
        if cpu_since_last_poll(&mut procs) >= BUSY_CPU_MS {
            active_left = LINGER_POLLS;
        }
        if active_left == 0 {
            continue;
        }
        active_left -= 1;
        for app in new_toasts(&listener, &mut seen) {
            if let Some(agent) = AGENTS.iter().find(|a| app.contains(*a)) {
                // The app already decided this was worth a toast (e.g. a reply
                // in another chat while its window is open), so always show.
                crate::deliver(agent, crate::find_app_window(agent), "", null_mut(), true);
            }
        }
    }
}

/// Opens handles to agent processes we are not tracking yet, drops exited ones.
unsafe fn rescan(procs: &mut HashMap<u32, (HANDLE, u64)>) {
    procs.retain(|_, (h, _)| {
        let alive = WaitForSingleObject(*h, 0) == WAIT_TIMEOUT;
        if !alive {
            CloseHandle(*h);
        }
        alive
    });
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
    if snap == INVALID_HANDLE_VALUE {
        return;
    }
    let mut entry: PROCESSENTRY32W = zeroed();
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut more = Process32FirstW(snap, &mut entry) != 0;
    while more {
        let end = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
        let exe = String::from_utf16_lossy(&entry.szExeFile[..end]).to_lowercase();
        let pid = entry.th32ProcessID;
        if !procs.contains_key(&pid) && AGENTS.iter().any(|a| exe.starts_with(a)) {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid);
            if !h.is_null() {
                procs.insert(pid, (h, cpu_ms(h)));
            }
        }
        more = Process32NextW(snap, &mut entry) != 0;
    }
    CloseHandle(snap);
}

unsafe fn cpu_since_last_poll(procs: &mut HashMap<u32, (HANDLE, u64)>) -> u64 {
    procs
        .values_mut()
        .map(|(h, last)| {
            let now = cpu_ms(*h);
            let used = now.saturating_sub(*last);
            *last = now;
            used
        })
        .sum()
}

unsafe fn cpu_ms(h: HANDLE) -> u64 {
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) =
        (zeroed(), zeroed(), zeroed(), zeroed());
    if GetProcessTimes(h, &mut created, &mut exited, &mut kernel, &mut user) == 0 {
        return 0;
    }
    let ticks = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
    (ticks(kernel) + ticks(user)) / 10_000
}

/// Lower-case app names of toasts not in `seen`; `seen` becomes the current
/// set. App names are looked up only for new toasts (that lookup is costly).
fn new_toasts(listener: &UserNotificationListener, seen: &mut HashSet<u32>) -> Vec<String> {
    let Ok(list) = listener.GetNotificationsAsync(NotificationKinds::Toast).and_then(|op| op.join()) else {
        return Vec::new();
    };
    let mut current = HashSet::new();
    let mut apps = Vec::new();
    for n in list {
        let Ok(id) = n.Id() else { continue };
        current.insert(id);
        if !seen.contains(&id) {
            let app = n.AppInfo().and_then(|a| a.DisplayInfo()).and_then(|d| d.DisplayName());
            apps.push(app.map(|s| s.to_string().to_lowercase()).unwrap_or_default());
        }
    }
    *seen = current;
    apps
}
