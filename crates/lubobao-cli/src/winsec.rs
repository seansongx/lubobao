//! Windows 命名管道的访问控制：服务端只允许当前用户连接；客户端确认管道的服务端进程属于
//! 同一个用户，防止别的用户抢先创建同名管道冒充 `run`。

use std::ffi::c_void;
use std::io;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// 进程令牌的 `TOKEN_USER`，用 u64 缓冲区保证对齐。
struct TokenUserBuf(Vec<u64>);

impl TokenUserBuf {
    fn of_process(process: HANDLE) -> io::Result<TokenUserBuf> {
        // SAFETY: 句柄由调用方保证有效；缓冲区大小由第一次调用给出。
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(process, TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                (buf.len() * 8) as u32,
                &mut len,
            );
            let error = io::Error::last_os_error();
            CloseHandle(token);
            if ok == 0 {
                return Err(error);
            }
            Ok(TokenUserBuf(buf))
        }
    }

    fn sid(&self) -> *mut c_void {
        // SAFETY: 缓冲区里是 GetTokenInformation 写入的 TOKEN_USER。
        unsafe { (*(self.0.as_ptr() as *const TOKEN_USER)).User.Sid }
    }
}

fn current_user() -> io::Result<TokenUserBuf> {
    // SAFETY: 当前进程的伪句柄总是有效。
    TokenUserBuf::of_process(unsafe { GetCurrentProcess() })
}

/// 只允许当前用户访问的安全描述符（SDDL `D:P(A;;GA;;;<当前用户 SID>)`）。
pub struct PipeSecurity {
    attributes: SECURITY_ATTRIBUTES,
    descriptor: PSECURITY_DESCRIPTOR,
}

// SAFETY: 描述符在创建后只读，只在 drop 时释放。
unsafe impl Send for PipeSecurity {}
unsafe impl Sync for PipeSecurity {}

impl PipeSecurity {
    pub fn current_user_only() -> io::Result<Box<PipeSecurity>> {
        let user = current_user()?;
        // SAFETY: 指针参数均为合法的输出位置；返回的内存用 LocalFree 释放。
        unsafe {
            let mut sid_text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.sid(), &mut sid_text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0;
            while *sid_text.add(len) != 0 {
                len += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid_text, len));
            LocalFree(sid_text as *mut c_void);

            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Box::new(PipeSecurity {
                attributes: SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                },
                descriptor,
            }))
        }
    }

    /// 给 `ServerOptions::create_with_security_attributes_raw` 用的指针。
    pub fn as_ptr(&self) -> *mut c_void {
        &self.attributes as *const SECURITY_ATTRIBUTES as *mut c_void
    }
}

impl Drop for PipeSecurity {
    fn drop(&mut self) {
        // SAFETY: 描述符由 ConvertStringSecurityDescriptorToSecurityDescriptorW 分配。
        unsafe {
            LocalFree(self.descriptor);
        }
    }
}

/// 管道的服务端进程是否与当前进程属于同一个用户。
pub fn server_is_same_user(pipe: &impl AsRawHandle) -> io::Result<bool> {
    // SAFETY: 管道句柄在调用期间有效；打开的进程句柄随即关闭。
    unsafe {
        let mut pid = 0u32;
        if GetNamedPipeServerProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return Err(io::Error::last_os_error());
        }
        let theirs = TokenUserBuf::of_process(process);
        CloseHandle(process);
        let theirs = theirs?;
        let mine = current_user()?;
        Ok(EqualSid(theirs.sid(), mine.sid()) != 0)
    }
}
