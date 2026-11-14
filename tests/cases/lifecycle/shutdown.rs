//! The host stop request must finish the kernel's own domain cleanup.
use std::{fs, process::{Child, Command, Stdio}, thread, time::{Duration, Instant}};
use serde_json::json;
use windows_sys::Win32::{
    Foundation::{CloseHandle, WAIT_OBJECT_0},
    System::Threading::{EVENT_MODIFY_STATE, OpenEventW, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject},
};

struct Host(Child);
impl Drop for Host {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}

#[test]
fn headless_stop_signal_reclaims_domain_before_host_exits() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tmp/shutdown").join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&root).unwrap();
    let pid_file = root.join("domain.pid");
    let code = format!(r#"import os,time
r=open(os.environ['ATELIER_IPC_IN'],'rb',buffering=0)
w=open(os.environ['ATELIER_IPC_OUT'],'wb',buffering=0)
open({},'w').write(str(os.getpid()))
time.sleep(120)
"#, serde_json::to_string(&pid_file.to_string_lossy()).unwrap());
    let config = root.join("boot.json");
    fs::write(&config, json!({"db_path":root.join("kernel.db"),"web_ui_enabled":false,"domains":[{"domain_id":"probe","command":"python","args":["-c",code],"auto_restart":false}]}).to_string()).unwrap();
    let mut host = Host(Command::new(env!("CARGO_BIN_EXE_atelier-kernel")).args(["--headless", "--config"]).arg(&config).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(15);
    while !pid_file.exists() {
        assert!(host.0.try_wait().unwrap().is_none(), "kernel exited before domain start");
        assert!(Instant::now() < deadline, "domain did not start");
        thread::sleep(Duration::from_millis(20));
    }
    let pid: u32 = fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    let name: Vec<u16> = format!("Local\\AtelierKernelShutdown-{}", host.0.id()).encode_utf16().chain(Some(0)).collect();
    unsafe {
        let domain = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        assert!(!domain.is_null(), "cannot observe the domain process");
        let event = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        assert!(!event.is_null(), "kernel stop event was not created");
        assert_ne!(SetEvent(event), 0);
        CloseHandle(event);
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = host.0.try_wait().unwrap() { break status; }
            assert!(Instant::now() < deadline, "kernel did not exit");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success());
        assert_eq!(WaitForSingleObject(domain, 0), WAIT_OBJECT_0, "domain still alive after host exit");
        CloseHandle(domain);
    }
}
