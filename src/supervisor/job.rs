//! 域的进程树边界。
//!
//! 停域时只对直系子进程 `kill()` 会留下孙进程（域自己再拉起的解释器、浏览器、
//! ffmpeg 之类），而这些孙进程往往继承着域的 stdout/stderr 管道写端——内核里
//! 读管道的 worker 等不到 EOF，`stop` / `detach` 就会一直挂在这里。实测复现：
//! 一个自己 `Popen` 孙进程的 python 域，detach 后孙进程还活着、detach 永远不返回。
//!
//! 所以每个域在 spawn 之后立刻挂到一个进程树边界上：
//!
//! - **Windows**：Job Object + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`。停域时
//!   `TerminateJobObject` 一次带走整棵树；内核自己崩掉时句柄被系统回收，
//!   子进程同样会被带走。挂 Job 失败（例如 CI runner 的 Job 不允许嵌套）时退回
//!   快照遍历：按父子关系把整棵子树一个个 TerminateProcess。
//! - **Unix**：spawn 时 `setsid()` 建独立进程组，停域时 `kill(-pgid, SIGKILL)`。
//!
//! 这个模块只负责"边界"和"拆掉边界"，不碰管道、不碰状态机。

#[cfg(windows)]
mod imp {
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, PROCESS_TERMINATE,
    };

    /// 一个域的进程树边界。`job` 为空说明没挂上 Job，只能靠退出时快照遍历。
    pub struct DomainTree {
        pid: u32,
        job: Option<OwnedHandle>,
    }

    impl DomainTree {
        /// 给刚 spawn 出来的域进程挂上 Job Object。
        ///
        /// 这里不返回错误：挂 Job 失败（受限的父 Job、老系统）不是启动失败，
        /// 退回按 pid 遍历子孙进程即可，行为从"强一致"降级为"尽力而为"。
        pub fn attach(child: &Child) -> Self {
            let pid = child.id();
            let job = unsafe { create_job() }
                .ok()
                .and_then(|job| unsafe { assign(&job, child) }.ok().map(|_| job));
            Self { pid, job }
        }

        /// 没有可挂的进程（探针子进程自己就是边界）。
        #[allow(dead_code)] // 仅测试里的探针构造需要它
        pub fn detached(pid: u32) -> Self {
            Self { pid, job: None }
        }

        /// 拆掉整棵树。可重复调用：Job 已经被拆或进程已退出时直接返回。
        pub fn terminate(&self) {
            if let Some(job) = &self.job {
                unsafe {
                    let _ = TerminateJobObject(job.as_raw_handle() as HANDLE, 1);
                }
                return;
            }
            unsafe { terminate_descendants(self.pid) };
        }
    }

    unsafe fn create_job() -> io::Result<OwnedHandle> {
        let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = unsafe { OwnedHandle::from_raw_handle(raw as _) };
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                raw,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const core::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    unsafe fn assign(job: &OwnedHandle, child: &Child) -> io::Result<()> {
        let ok = unsafe { AssignProcessToJobObject(job.as_raw_handle() as HANDLE, child.as_raw_handle() as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// 按父子关系收集 `root` 的全部后代，先杀叶子再杀根。
    unsafe fn terminate_descendants(root: u32) {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return;
        }
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        if unsafe { Process32FirstW(snapshot, &mut entry) } != 0 {
            loop {
                pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
                    break;
                }
            }
        }
        unsafe { CloseHandle(snapshot) };

        let mut ordered: Vec<u32> = Vec::new();
        let mut frontier = vec![root];
        while let Some(parent) = frontier.pop() {
            for (pid, ppid) in &pairs {
                if *ppid == parent && *pid != 0 && !ordered.contains(pid) {
                    ordered.push(*pid);
                    frontier.push(*pid);
                }
            }
        }
        ordered.push(root);
        for pid in ordered {
            let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
            if !handle.is_null() {
                unsafe {
                    let _ = TerminateProcess(handle, 1);
                    CloseHandle(handle);
                }
            }
        }
    }
}

#[cfg(unix)]
mod imp {
    use std::process::Child;

    pub struct DomainTree {
        pid: u32,
    }

    impl DomainTree {
        pub fn attach(child: &Child) -> Self {
            Self { pid: child.id() }
        }

        pub fn detached(pid: u32) -> Self {
            Self { pid }
        }

        /// `setsid()` 之后进程组 id 就是域进程自己的 pid，杀整个组即杀整棵树。
        pub fn terminate(&self) {
            unsafe {
                libc::kill(-(self.pid as i32), libc::SIGKILL);
            }
        }
    }
}

#[cfg(not(any(windows, unix)))]
compile_error!("DomainTree 只为 Windows 与 Unix 实现；新增平台需要先补进程树边界");

pub use imp::DomainTree;
