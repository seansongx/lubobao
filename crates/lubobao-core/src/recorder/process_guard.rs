//! 保证录播宝退出或崩溃后 ffmpeg 不会成为孤儿进程。
//!
//! `kill_on_drop` 只在 `Child` 被 drop 时生效，而 `process::exit`、`panic = "abort"`、
//! 进程被结束时都不会 drop。各系统的处理：
//!
//! - **Windows**：进程内共用一个 Job Object，设 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，
//!   ffmpeg 启动后立即加入。录播宝退出时系统关闭最后一个 Job 句柄，Job 内的进程随之结束。
//!   启动和加入之间有极短的间隙，父进程恰好在此时崩溃的情况不处理。
//! - **Linux**：在子进程 `exec` 前设 `PR_SET_PDEATHSIG = SIGKILL`。该信号在创建子进程的
//!   **线程**退出时触发；`start` 在 Tokio 工作线程上启动 ffmpeg，工作线程与运行时同寿命。
//! - **macOS**：没有对应的内核机制。正常退出时由 [`super::stop_all`] 收尾；录播宝崩溃时
//!   ffmpeg 会继续运行，所以下次启动、调度开始之前调用 [`reap_orphans`]，结束可执行文件位于
//!   录播宝自己安装的 FFmpeg 目录下的进程（桌面端单实例，命令行持有数据目录锁，此时不会有
//!   别的录播宝实例在用这个目录里的 ffmpeg）。
//! - **其他系统**：只能靠正常收尾。

use std::path::Path;

use tokio::process::{Child, Command};

/// 在启动前配置命令。
pub fn configure(command: &mut Command) {
    imp::configure(command);
}

/// 启动后把子进程绑定到父进程的生命周期。失败时返回说明，录制继续。
pub fn attach(child: &Child) -> Result<(), String> {
    imp::attach(child)
}

/// 结束上次异常退出时留下的 ffmpeg：可执行文件位于 `managed_dir`（录播宝自己安装 FFmpeg 的
/// 目录）之下的进程，发 SIGTERM，让 ffmpeg 写完文件尾再退出。返回结束的进程数。
///
/// 只在 macOS 上有事可做；Windows、Linux 由内核保证 ffmpeg 随录播宝结束，这里什么都不做。
/// 必须在调度开始录制之前、确认没有别的录播宝实例使用这个目录时调用。
pub fn reap_orphans(managed_dir: &Path) -> usize {
    imp::reap_orphans(managed_dir)
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::sync::OnceLock;

    use tokio::process::{Child, Command};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    /// Job 句柄存为整数，便于放进静态变量；进程结束前从不关闭。
    static JOB: OnceLock<Result<usize, String>> = OnceLock::new();

    fn job() -> Result<HANDLE, String> {
        JOB.get_or_init(create_job)
            .clone()
            .map(|handle| handle as HANDLE)
    }

    fn create_job() -> Result<usize, String> {
        // SAFETY: 参数均为合法指针或空指针；失败时关闭已创建的句柄。
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(format!(
                    "无法创建 Job Object：{}",
                    io::Error::last_os_error()
                ));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                let error = io::Error::last_os_error();
                CloseHandle(job);
                return Err(format!("无法设置 Job Object：{error}"));
            }
            Ok(job as usize)
        }
    }

    pub fn configure(_command: &mut Command) {}

    pub fn attach(child: &Child) -> Result<(), String> {
        let job = job()?;
        // 已经退出的进程没有句柄，也不需要绑定。
        let Some(process) = child.raw_handle() else {
            return Ok(());
        };
        // SAFETY: 两个句柄在调用期间都有效。
        let ok = unsafe { AssignProcessToJobObject(job, process as HANDLE) };
        if ok == 0 {
            return Err(format!(
                "无法把 ffmpeg 加入 Job Object，录播宝异常退出时它可能继续运行：{}",
                io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    pub fn reap_orphans(_managed_dir: &std::path::Path) -> usize {
        0
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::io;

    use tokio::process::{Child, Command};

    pub fn configure(command: &mut Command) {
        let parent = std::process::id();
        // SAFETY: 闭包只调用 async-signal-safe 的 prctl 和 getppid。
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // 父进程在 fork 与 prctl 之间已经退出。
                if libc::getppid() as u32 != parent {
                    return Err(io::Error::other("parent exited before exec"));
                }
                Ok(())
            });
        }
    }

    pub fn attach(_child: &Child) -> Result<(), String> {
        Ok(())
    }

    pub fn reap_orphans(_managed_dir: &std::path::Path) -> usize {
        0
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Path, PathBuf};

    use tokio::process::{Child, Command};

    pub fn configure(_command: &mut Command) {}

    pub fn attach(_child: &Child) -> Result<(), String> {
        Ok(())
    }

    pub fn reap_orphans(managed_dir: &Path) -> usize {
        // 目录不存在说明从没装过 FFmpeg，也就不会有残留。系统返回的进程路径已解析符号链接，
        // 这里同样取真实路径再比较（例如临时目录 /var → /private/var）。
        let Ok(dir) = std::fs::canonicalize(managed_dir) else {
            return 0;
        };
        let own = std::process::id() as libc::pid_t;
        let mut reaped = 0;
        for pid in all_pids() {
            if pid <= 0 || pid == own {
                continue;
            }
            if process_path(pid).is_some_and(|path| path.starts_with(&dir)) {
                // SAFETY: 只发信号。
                if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
                    reaped += 1;
                }
            }
        }
        reaped
    }

    fn all_pids() -> Vec<libc::pid_t> {
        // SAFETY: 空缓冲区只返回当前的进程数。
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count <= 0 {
            return Vec::new();
        }
        // 两次调用之间可能有新进程，多留一些余量。
        let mut pids: Vec<libc::pid_t> = vec![0; count as usize + 64];
        let bytes = (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: 传入的字节数与缓冲区大小一致；返回值是写入的进程数。
        let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if filled <= 0 {
            return Vec::new();
        }
        pids.truncate((filled as usize).min(pids.len()));
        pids
    }

    fn process_path(pid: libc::pid_t) -> Option<PathBuf> {
        let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: 传入的长度与缓冲区大小一致；返回值是写入的字节数。
        let len =
            unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
        if len <= 0 {
            return None;
        }
        buffer.truncate((len as usize).min(buffer.len()));
        Some(PathBuf::from(OsString::from_vec(buffer)))
    }

    #[cfg(test)]
    mod tests {
        use std::time::{Duration, Instant};

        #[test]
        fn reaps_processes_running_from_the_managed_directory_only() {
            let managed = tempfile::tempdir().unwrap();
            let sleep = managed.path().join("sleep");
            std::fs::copy("/bin/sleep", &sleep).unwrap();
            let mut child = std::process::Command::new(&sleep)
                .arg("30")
                .spawn()
                .unwrap();
            // 另一个目录下没有进程。
            let elsewhere = tempfile::tempdir().unwrap();
            assert_eq!(super::reap_orphans(elsewhere.path()), 0);
            // 子进程 exec 之前路径还是测试程序本身，等它换成目录里的程序。
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut reaped = 0;
            while reaped == 0 && Instant::now() < deadline {
                reaped = super::reap_orphans(managed.path());
                if reaped == 0 {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            assert_eq!(reaped, 1);
            let status = child.wait().unwrap();
            assert!(!status.success(), "sleep 应被 SIGTERM 结束：{status}");
        }

        #[test]
        fn missing_directory_reaps_nothing() {
            let dir = tempfile::tempdir().unwrap();
            assert_eq!(super::reap_orphans(&dir.path().join("missing")), 0);
        }
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
mod imp {
    use tokio::process::{Child, Command};

    pub fn configure(_command: &mut Command) {}

    pub fn attach(_child: &Child) -> Result<(), String> {
        Ok(())
    }

    pub fn reap_orphans(_managed_dir: &std::path::Path) -> usize {
        0
    }
}
