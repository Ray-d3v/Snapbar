use std::collections::{HashMap, HashSet};

use windows::Win32::{
    Foundation::{CloseHandle, FILETIME},
    System::Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
};

use xcap::Window;

#[derive(Default)]
pub(super) struct ProcessNameCache {
    entries: HashMap<u32, (u64, String)>,
}

impl ProcessNameCache {
    pub(super) fn app_name(&mut self, window: &Window) -> String {
        let Ok(pid) = window.pid() else {
            return window.app_name().unwrap_or_default();
        };
        let Some(creation_time) = process_creation_time(pid) else {
            return window.app_name().unwrap_or_default();
        };

        lookup_cached_name(&mut self.entries, pid, creation_time, || {
            window.app_name().unwrap_or_default()
        })
    }

    pub(super) fn retain_windows(&mut self, windows: &[Window]) {
        let pids: HashSet<u32> = windows
            .iter()
            .filter_map(|window| window.pid().ok())
            .collect();
        self.entries.retain(|pid, _| pids.contains(pid));
    }
}

fn lookup_cached_name<F>(
    entries: &mut HashMap<u32, (u64, String)>,
    pid: u32,
    creation_time: u64,
    mut loader: F,
) -> String
where
    F: FnMut() -> String,
{
    if let Some((cached_creation_time, name)) = entries.get(&pid)
        && *cached_creation_time == creation_time
    {
        return name.clone();
    }

    let name = loader();
    entries.remove(&pid);
    if !name.is_empty() {
        entries.insert(pid, (creation_time, name.clone()));
    }
    name
}

fn process_creation_time(pid: u32) -> Option<u64> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut creation_time = FILETIME::default();
    let mut exit_time = FILETIME::default();
    let mut kernel_time = FILETIME::default();
    let mut user_time = FILETIME::default();
    let result = unsafe {
        GetProcessTimes(
            process,
            &mut creation_time,
            &mut exit_time,
            &mut kernel_time,
            &mut user_time,
        )
    };
    unsafe {
        let _ = CloseHandle(process);
    }
    result.ok()?;
    Some((u64::from(creation_time.dwHighDateTime) << 32) | u64::from(creation_time.dwLowDateTime))
}

#[cfg(test)]
mod tests {
    use super::lookup_cached_name;
    use std::{cell::Cell, collections::HashMap};

    #[test]
    fn same_process_creation_uses_loader_once() {
        let mut entries = HashMap::new();
        let loads = Cell::new(0);
        assert_eq!(
            lookup_cached_name(&mut entries, 7, 11, || {
                loads.set(loads.get() + 1);
                "teams.exe".to_string()
            }),
            "teams.exe"
        );
        assert_eq!(
            lookup_cached_name(&mut entries, 7, 11, || {
                loads.set(loads.get() + 1);
                "reloaded.exe".to_string()
            }),
            "teams.exe"
        );
        assert_eq!(loads.get(), 1);
    }

    #[test]
    fn changed_creation_time_reloads_name() {
        let mut entries = HashMap::new();
        let loads = Cell::new(0);
        assert_eq!(
            lookup_cached_name(&mut entries, 7, 11, || {
                loads.set(loads.get() + 1);
                "old.exe".to_string()
            }),
            "old.exe"
        );
        assert_eq!(
            lookup_cached_name(&mut entries, 7, 22, || {
                loads.set(loads.get() + 1);
                "new.exe".to_string()
            }),
            "new.exe"
        );
        assert_eq!(loads.get(), 2);
        assert_eq!(entries.get(&7), Some(&(22, "new.exe".to_string())));
    }

    #[test]
    fn empty_result_is_not_cached_and_retries() {
        let mut entries = HashMap::new();
        let loads = Cell::new(0);
        assert_eq!(
            lookup_cached_name(&mut entries, 7, 11, || {
                loads.set(loads.get() + 1);
                String::new()
            }),
            ""
        );
        assert_eq!(
            lookup_cached_name(&mut entries, 7, 11, || {
                loads.set(loads.get() + 1);
                "teams.exe".to_string()
            }),
            "teams.exe"
        );
        assert_eq!(loads.get(), 2);
    }
}
