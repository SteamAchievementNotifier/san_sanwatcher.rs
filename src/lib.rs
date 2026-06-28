use napi_derive::napi;
use napi::{threadsafe_function::{ThreadsafeFunction,ThreadsafeFunctionCallMode},JsFunction};
use once_cell::sync::Lazy;
use std::{collections::HashSet,sync::{Arc, Mutex, atomic::{AtomicBool,Ordering}},thread, time::Duration};
pub mod log;

#[cfg(target_os="windows")]
pub mod win32 {
    pub use std::{ptr::null_mut,time::Duration,mem::size_of};
    pub use core::ffi::c_void;
    pub use windows::{core::PWSTR,Win32::{Foundation::{CloseHandle,HANDLE},System::{Diagnostics::ToolHelp::{CreateToolhelp32Snapshot,Process32FirstW,Process32NextW,PROCESSENTRY32W,TH32CS_SNAPPROCESS},Threading::{OpenProcess,QueryFullProcessImageNameW,WaitForSingleObject,PROCESS_ACCESS_RIGHTS,PROCESS_NAME_FORMAT,PROCESS_QUERY_LIMITED_INFORMATION,INFINITE}}}};
}

#[cfg(target_os="linux")]
pub mod linux {
    pub use std::{fs,path::Path,time::Duration};
}

struct WatcherState {
    install_dir: String,
    linked_game: Option<String>,
    seen: Mutex<HashSet<u32>>,
    running: AtomicBool
}

static WATCHER: Lazy<Mutex<Option<Arc<WatcherState>>>> = Lazy::new(|| Mutex::new(None));

#[derive(Debug)]
#[napi(object)]
pub struct WatchEvent {
    pub started: bool,
    pub pid: u32,
    pub exe: String
}

static CALLBACK: Lazy<Mutex<Option<Arc<ThreadsafeFunction<WatchEvent>>>>> = Lazy::new(|| Mutex::new(None));

#[napi(object)]
pub struct ActiveProcess {
    pub pid: u32,
    pub exe: String
}

fn normalize(path: &str) -> String {
    path.to_lowercase().replace("\\","/")
}

#[cfg(target_os="windows")]
fn get_active_processes_win(installdir: &str,linkedgame: Option<&str>) -> Vec<ActiveProcess> {
    use win32::*;

    let mut processes = Vec::new();

    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS,0) else {
            return processes
        };

        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;

        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let pid = entry.th32ProcessID;

                if pid != 0 {
                    if let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_ACCESS_RIGHTS(0x00100000),false,pid) {
                        let mut buffer = [0u16; 1024];
                        let mut size = buffer.len() as u32;

                        if QueryFullProcessImageNameW(process,PROCESS_NAME_FORMAT(0),PWSTR(buffer.as_mut_ptr()),&mut size).is_ok() {
                            let path = normalize(&String::from_utf16_lossy(&buffer[..size as usize]));

                            let matched = match linkedgame {
                                Some(lg) => lg == path,
                                None => path.starts_with(&installdir) || path.ends_with("sam.game.exe")
                            };

                            if matched {
                                processes.push(ActiveProcess { pid, exe: path });
                            }
                        }

                        let _ = CloseHandle(process);
                    }
                }

                if Process32NextW(snapshot,&mut entry).is_err() {
                    break
                }
            }
        }

        let _ = CloseHandle(snapshot);
    }

    processes
}

#[cfg(target_os="linux")]
fn get_active_processes_linux(installdir: &str,linkedgame: Option<&str>) -> Vec<ActiveProcess> {
    use linux::*;
    
    let mut processes = Vec::new();
    let installdir = normalize(&installdir);

    let Ok(entries) = fs::read_dir("/proc") else {
        return processes
    };

    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let pid_str = file_name.to_string_lossy();

        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue
        }

        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue
        };

        let exe_path = format!("/proc/{}/exe", pid);

        let Ok(path) = fs::read_link(&exe_path) else {
            continue
        };

        let path = normalize(&path.to_string_lossy().to_string());

        let matched = match linkedgame {
            Some(lg) => lg == path,
            None => path.starts_with(&installdir)
        };

        if matched {
            processes.push(ActiveProcess { pid, exe: path });
        }
    }

    processes
}

#[napi]
pub fn get_active_processes(installdir: String,linkedgame: Option<String>) -> Vec<ActiveProcess> {
    #[cfg(target_os="windows")] {
        return get_active_processes_win(&normalize(&installdir),linkedgame.map(|str| normalize(&str)).as_deref())
    }
    
    #[cfg(target_os="linux")] {
        return get_active_processes_linux(&normalize(&installdir),linkedgame.map(|str| normalize(&str)).as_deref())
    }
}

#[napi]
pub fn stop() {
    let mut watcher_guard = WATCHER.lock().unwrap_or_else(|e| e.into_inner());

    if let Some(watcher) = watcher_guard.take() {
        watcher.running.store(false,Ordering::SeqCst);
    }

    CALLBACK.lock().unwrap().take();
}

#[napi]
pub fn start(installdir: String,linkedgame: Option<String>,pollrate: u32,callback: JsFunction) {
    stop();

    let pollrate = Duration::from_millis(pollrate as u64);

    let threadsafe_function = callback.create_threadsafe_function(0, |ctx| Ok(vec![ctx.value])).expect("Unable to create threadsafe function");

    *CALLBACK.lock().unwrap() = Some(Arc::new(threadsafe_function));

    let watcher = Arc::new(WatcherState {
        install_dir: normalize(&installdir),
        linked_game: linkedgame.map(|str| normalize(&str)),
        seen: Mutex::new(HashSet::new()),
        running: AtomicBool::new(true)
    });

    {
        let mut guard = WATCHER.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(Arc::clone(&watcher));
    }

    #[cfg(target_os="windows")] {
        thread::spawn(move || {
            use win32::*;

            while watcher.running.load(Ordering::SeqCst) {
                for process in get_active_processes_win(&watcher.install_dir,watcher.linked_game.as_deref()) {
                    let pid = process.pid;
                    let path = process.exe;

                    let seen = {
                        let seen = watcher.seen.lock().unwrap_or_else(|e| e.into_inner());
                        seen.contains(&pid)
                    };

                    if seen {
                        continue
                    }

                    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_ACCESS_RIGHTS(0x00100000),false,pid) };

                    if let Ok(process) = process {
                        watcher.seen.lock().unwrap_or_else(|e| e.into_inner()).insert(pid);

                        if let Some(cb) = CALLBACK.lock().unwrap().clone() {
                            let result = Ok(WatchEvent {
                                started: true,
                                pid,
                                exe: path.clone()
                            });

                            let _ = cb.call(result,ThreadsafeFunctionCallMode::NonBlocking);
                        }

                        let raw_handle = process.0 as isize;
                        let watcher_clone = Arc::clone(&watcher);

                        thread::spawn(move || {
                            let process = HANDLE(raw_handle as *mut c_void);

                            unsafe { WaitForSingleObject(process,INFINITE); }

                            if let Some(cb) = CALLBACK.lock().unwrap().clone() {
                                let result = Ok(WatchEvent {
                                    started: false,
                                    pid,
                                    exe: path.clone()
                                });

                                let _ = cb.call(result,ThreadsafeFunctionCallMode::NonBlocking);
                            }

                            let _ = unsafe { CloseHandle(process) };

                            watcher_clone.seen.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
                        });
                    }
                }

                thread::sleep(pollrate);
            }
        });
    }

    #[cfg(target_os="linux")] {
        thread::spawn(move || {
            use linux::*;

            while watcher.running.load(Ordering::SeqCst) {
                for process in get_active_processes_linux(&watcher.install_dir,watcher.linked_game.as_deref()) {
                    let pid = process.pid;
                    let path = process.exe;

                    {
                        let mut seen = watcher.seen.lock().unwrap_or_else(|e| e.into_inner());

                        if seen.contains(&pid) {
                            continue
                        }

                        seen.insert(pid);
                    }

                    let watcher_clone = Arc::clone(&watcher);

                    thread::spawn(move || {
                        let proc_path = format!("/proc/{}",pid);

                        while Path::new(&proc_path).exists() {
                            thread::sleep(pollrate);
                        }

                        if let Some(cb) = CALLBACK.lock().unwrap().clone() {
                            let result = Ok(WatchEvent {
                                started: false,
                                pid,
                                exe: path.clone()
                            });

                            let _ = cb.call(result,ThreadsafeFunctionCallMode::NonBlocking);
                        }

                        watcher_clone.seen.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
                    });
                }

                thread::sleep(pollrate);
            }
        });
    }
}