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
//! - **macOS 等其他系统**：没有对应的内核机制。应用应在 `RunEvent::ExitRequested` 时调用
//!   [`super::stop_all`] 正常收尾。录播宝崩溃时 ffmpeg 仍会继续录制，直到源站断开（下播）
//!   或读写超时；后续可在启动时按记录的 PID 清理残留进程。

use tokio::process::{Child, Command};

/// 在启动前配置命令。
pub fn configure(command: &mut Command) {
    imp::configure(command);
}

/// 启动后把子进程绑定到父进程的生命周期。失败时返回说明，录制继续。
pub fn attach(child: &Child) -> Result<(), String> {
    imp::attach(child)
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
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use tokio::process::{Child, Command};

    pub fn configure(_command: &mut Command) {}

    pub fn attach(_child: &Child) -> Result<(), String> {
        Ok(())
    }
}
