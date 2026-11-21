//! 停域必须拆掉整棵进程树，而且不能被孙进程继承的管道拖住。
//!
//! 回归的是这样一个现场：域自己拉起一个孙进程（解释器/ffmpeg/浏览器都这么干），
//! 孙进程继承着域的 stdout/stderr 写端。只杀直系子进程的话，
//! ① 孙进程留在系统里；② 内核读管道的 worker 等不到 EOF，`stop_process_domain`
//! 永远返回不了。实测：修复前 detach 永不返回，修复后立刻返回、孙进程消失。

use atelier_kernel::supervisor::config::DomainProcessSpec;
use atelier_kernel::AtelierKernel;
use std::fs;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

fn work_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("atelier-process-tree-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// 在 test 进程里判断某个 pid 是否还活着（不属于本进程树，不能靠信号 0）。
#[cfg(windows)]
fn alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    unsafe {
        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let state = WaitForSingleObject(handle, 0);
        CloseHandle(handle);
        state == WAIT_TIMEOUT
    }
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// 等 pid 文件出现并读出孙进程 pid。
fn wait_for_grandchild(dir: &PathBuf) -> u32 {
    let path = dir.join("pid.txt");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("孙进程没有在 20 秒内写出 pid 文件：{}", path.display());
}

#[test]
fn stopping_a_domain_takes_the_whole_tree_and_does_not_hang() {
    let dir = work_dir("tree");
    let dir_text = dir.to_string_lossy().to_string();

    #[cfg(windows)]
    let (command, args) = (
        "powershell".to_string(),
        vec![
            "-NoProfile".to_string(),
            "-Command".to_string(),
            // 孙进程继承 stdout/stderr 写端；父进程写 pid 后自己睡 60 秒。
            format!(
                "$p = Start-Process -FilePath ping -ArgumentList '-n','60','127.0.0.1' -PassThru -NoNewWindow; \
                 Set-Content -Path pid.txt -Value $p.Id; Start-Sleep 60"
            ),
        ],
    );
    #[cfg(unix)]
    let (command, args) = (
        "sh".to_string(),
        vec![
            "-c".to_string(),
            "sleep 60 & echo $! > pid.txt; sleep 60".to_string(),
        ],
    );

    let kernel = std::sync::Arc::new(AtelierKernel::new_in_memory().unwrap());
    kernel.logger().set_console_enabled(false);
    kernel
        .spawn_process_domain(DomainProcessSpec {
            domain_id: "tree_domain".into(),
            command,
            args,
            working_dir: Some(dir_text),
            auto_restart: false,
            ..DomainProcessSpec::default()
        })
        .expect("域应该能启动");

    let grandchild = wait_for_grandchild(&dir);
    assert!(alive(grandchild), "孙进程 {grandchild} 应该已经起来了");

    // 停域放到另一个线程里：修复前这里会挂死，主线程才能用超时判定失败而不是拖死整个测试套件。
    let (done, stopped) = mpsc::channel();
    let stopping = kernel.clone();
    thread::spawn(move || {
        let _ = done.send(stopping.supervisor().stop_process_domain("tree_domain"));
    });
    let returned = stopped.recv_timeout(Duration::from_secs(15));
    assert!(
        returned.is_ok(),
        "stop_process_domain 15 秒没返回——孙进程继承的管道把 worker 拖住了"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && alive(grandchild) {
        thread::sleep(Duration::from_millis(100));
    }
    assert!(!alive(grandchild), "孙进程 {grandchild} 在停域之后仍然活着");

    let _ = fs::remove_dir_all(&dir);
}
