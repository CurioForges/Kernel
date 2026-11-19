use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
mod shutdown;

use atelier_kernel::{
    AtelierKernel, ContractStatus, DomainStatus, InteractiveFrontendDomain, KernelBootConfig,
    KernelToDomainFrame, check_constitution,
};
use serde_json::json;

const DEFAULT_TEMPLATE_JSON: &str = r#"{
  "db_path": "./data/atelier_kernel.db",
  "domains": [
    {
      "domain_id": "sample_py",
      "command": "python3",
      "args": ["-u", "domains/sample_domain.py"],
      "working_dir": null
    }
  ]
}
"#;

const SAMPLE_PY_DOMAIN: &str = r#"#!/usr/bin/env python3
import asyncio

from atelier_sdk import AtelierDomain

domain = AtelierDomain("sample_py")


@domain.handler("ping")
async def on_ping(msg):
    domain.logger.info("Received ping", source=msg.source_domain)
    await domain.storage_insert("last_ping", "health", msg.data)
    return {"pong": True, "from": "sample_py", "received": msg.data}


if __name__ == "__main__":
    asyncio.run(domain.run_async())
"#;

fn init_variant_skeleton(target_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = Path::new(target_dir);
    let domains_dir = root.join("domains");
    fs::create_dir_all(&domains_dir)?;

    let cfg_path = root.join("kernel_boot.json");
    if !cfg_path.exists() {
        fs::write(&cfg_path, DEFAULT_TEMPLATE_JSON)?;
    }

    let sample_py_path = domains_dir.join("sample_domain.py");
    if !sample_py_path.exists() {
        fs::write(&sample_py_path, SAMPLE_PY_DOMAIN)?;
    }

    println!(
        "Initialized Atelier Kernel variant skeleton at: {}",
        root.display()
    );
    println!("  - Config:  {}", cfg_path.display());
    println!("  - Domain:  {}", sample_py_path.display());
    println!("Run with: atelier-kernel --config {}", cfg_path.display());
    Ok(())
}

/// Boots a variant configuration, verifies that all configured domains register their handlers,
/// dispatches a live probe message to each domain, verifies the 4-stage receipt lifecycle
/// (`Sent -> Received -> Started -> Completed`), and exits cleanly.
fn run_variant_smoke_test(
    config_path: &str,
    boot_config: &KernelBootConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("[SmokeTest] Booting variant from config: {}", config_path);
    let kernel = AtelierKernel::open_with_db_and_root(&boot_config.db_path, &boot_config.project_root)?;

    if boot_config.domains.is_empty() {
        return Err("No domains declared in boot configuration".into());
    }

    let spawn_results = kernel.supervisor().boot_from_config(boot_config);
    for (domain_id, res) in spawn_results {
        match res {
            Ok(pid) => println!("[SmokeTest] Spawned domain '{}' (pid={})", domain_id, pid),
            Err(err) => {
                return Err(format!("Failed to spawn domain '{}': {}", domain_id, err).into());
            }
        }
    }

    // 1. Wait for all configured domains to register handlers and become Active
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let all_active = boot_config.domains.iter().all(|spec| {
            kernel.router().domain_status(&spec.domain_id) == Some(DomainStatus::Active)
                && kernel
                    .router()
                    .active_routes()
                    .iter()
                    .any(|r| r.starts_with(&format!("{}.", spec.domain_id)))
        });
        if all_active {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "Timed out waiting for domains to register handlers. Current routes: {:?}",
                kernel.router().active_routes()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(40));
    }

    let active_routes = kernel.router().active_routes();
    println!(
        "[SmokeTest] All configured domains Active! Registered routes: {:?}",
        active_routes
    );

    // 2. Attach probe domain and send a message to the first registered route of each domain
    let probe = kernel.attach_domain("smoke_verifier");
    let mut target_routes = Vec::new();
    for spec in &boot_config.domains {
        let prefix = format!("{}.", spec.domain_id);
        if let Some(route) = active_routes.iter().find(|r| r.starts_with(&prefix)) {
            target_routes.push(route.clone());
        }
    }

    let msg_id = probe.send(
        target_routes.clone(),
        "VariantSmokeProbe",
        json!({ "smoke_test": true }),
        json!({ "hello": "variant_verification" }),
    );
    println!(
        "[SmokeTest] Dispatched probe messages '{:?}' to targets {:?}",
        msg_id, target_routes
    );

    let mut completed_targets = HashSet::new();
    let mut logical_ids = HashSet::new();
    if msg_id.len() != target_routes.len()
        || msg_id.iter().collect::<HashSet<_>>().len() != target_routes.len()
    {
        return Err("Fan-out must allocate a distinct message ID per target".into());
    }
    let mut seen_stages_per_target: std::collections::HashMap<String, Vec<ContractStatus>> =
        std::collections::HashMap::new();

    let msg_deadline = Instant::now() + Duration::from_secs(5);
    while completed_targets.len() < target_routes.len() {
        if Instant::now() >= msg_deadline {
            return Err(format!(
                "Timed out waiting for 4-stage receipts. Seen stages: {:?}",
                seen_stages_per_target
            )
            .into());
        }
        for frame in probe.drain_frames() {
            if let KernelToDomainFrame::Receipt(r) = frame
                && msg_id.contains(&r.message_id)
            {
                if r.logical_id.is_empty() || r.logical_id == r.message_id {
                    return Err("Receipt must carry distinct logical_id and message_id".into());
                }
                logical_ids.insert(r.logical_id.clone());
                if logical_ids.len() != 1 {
                    return Err("Fan-out receipts must share the submission logical_id".into());
                }
                seen_stages_per_target
                    .entry(r.target.clone())
                    .or_default()
                    .push(r.status);
                if r.status == ContractStatus::Completed {
                    println!(
                        "[SmokeTest] Target '{}' completed with payload: {}",
                        r.target,
                        r.data.unwrap_or_default()
                    );
                    completed_targets.insert(r.target);
                } else if r.status == ContractStatus::ExecutionFailed
                    || r.status == ContractStatus::TargetInvalid
                    || r.status == ContractStatus::ContractInvalid
                {
                    return Err(format!(
                        "Probe failed for target '{}' with status {:?}: {:?}",
                        r.target, r.status, r.data
                    )
                    .into());
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }

    // Verify every target went through Sent -> Received -> Started -> Completed
    for route in &target_routes {
        let stages = seen_stages_per_target
            .get(route)
            .cloned()
            .unwrap_or_default();
        if stages
            != vec![
                ContractStatus::Sent,
                ContractStatus::Received,
                ContractStatus::Started,
                ContractStatus::Completed,
            ]
        {
            return Err(format!("Unexpected receipt stages for '{}': {:?}", route, stages).into());
        }
    }

    // Cleanly stop all domains
    for spec in &boot_config.domains {
        kernel.supervisor().stop_process_domain(&spec.domain_id);
    }

    // Verify SQLite hub_logs persistence
    thread::sleep(Duration::from_millis(250));
    let persisted_logs = kernel
        .logger()
        .query_logs(20, None, None, None, None, None)?;
    println!(
        "[SmokeTest] Verified hub_logs persistence: recorded {} structured log entries in SQLite",
        persisted_logs.len()
    );

    println!(
        "[SmokeTest] Variant smoke test PASSED (registration + 4-stage messaging + unified hub_logs verified)!"
    );
    Ok(())
}

fn print_help() {
    println!(
        "Atelier Kernel v{} — Single-Machine Multi-Domain Microkernel Runtime",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("USAGE:");
    println!("  atelier-kernel [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("  --config <path>   Path to boot configuration JSON (default: ./kernel_boot.json)");
    println!(
        "  --control-pipe <name>  Listen on a named control pipe and boot with ZERO domains;"
    );
    println!(
        "                    the launcher attaches every business domain at runtime."
    );
    println!("                    When given, --config is ignored.");
    println!("  --control-token <token>  Handshake token compared by the `hello` control op");
    println!("  --db <path>       Ledger path (required with --control-pipe)");
    println!("  --root <path>     C4 file root / project root (required with --control-pipe)");
    println!(
        "  --headless        Run without any HTTP listener (always the case; kept for CLI compatibility)"
    );
    println!(
        "  --smoke-test      Boot variant, verify domain registration & 4-stage messaging, and exit"
    );
    println!("  --cli             Run interactive CLI REPL frontend domain");
    println!("  --auto, -a        Run automated 7-stage kernel verification suite and exit");
    println!(
        "  --check [path]    Run Machine Constitution (M1-M7) check on config or domain directory"
    );
    println!("  --init <dir>      Initialize a new product variant directory with config template");
    println!("  --version, -v     Print version information");
    println!("  --help, -h        Print this help message");
}

/// 取 `--flag value` 形式的参数值。
fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|window| window[0] == name)
        .map(|window| window[1].clone())
}

/// 相对路径一律解析到内核自己的 cwd：控制通道模式下 `--db` / `--root` 由启动器传入，
/// 写相对路径会随 cwd 漂移，落出一个"看起来正常但读错了账本/文件根"的内核。
fn absolute_path(path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        return path.to_string_lossy().into_owned();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path).to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

/// `--control-pipe` 分支：零引导域启动 + 控制通道监听。
///
/// 这是"内核不解析域清单"的那条路：装配责任整体移交给启动器，
/// 域在运行期通过控制通道逐条 `attach` 进来。
fn run_control_host(
    args: &[String],
    pipe_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(db_path) = flag_value(args, "--db") else {
        return Err("--control-pipe requires an explicit --db <path>".into());
    };
    let Some(project_root) = flag_value(args, "--root") else {
        return Err("--control-pipe requires an explicit --root <path>".into());
    };
    let token = flag_value(args, "--control-token").filter(|token| !token.is_empty());

    let boot_config = KernelBootConfig::for_control_host(
        absolute_path(&db_path),
        absolute_path(&project_root),
    );
    let kernel = std::sync::Arc::new(AtelierKernel::open_with_db_and_root(
        &boot_config.db_path,
        &boot_config.project_root,
    )?);

    // 空 `domains` 也必须调用：`boot_from_config` 是 `files_root` 的唯一写入点，
    // 而 `files_root` 决定 `ATELIER_FILES_ROOT` 的注入。少了这一步，
    // 之后 attach 进来的**每一个**域做 vfs 操作都会直接失败。
    kernel.supervisor().boot_from_config(&boot_config);

    let channel = atelier_kernel::frontend::control::spawn(&kernel, pipe_name, token)?;
    println!(
        "[AtelierKernel] Control plane listening on {} (0 bootstrap domains; db={}, root={})",
        channel.path(),
        boot_config.db_path,
        boot_config.project_root
    );

    #[cfg(windows)]
    {
        let shutdown_event = shutdown::ShutdownEvent::new()?;
        shutdown_event.wait()?;
        channel.stop();
        Ok(()) // Dropping the kernel runs the existing supervisor shutdown and reclamation.
    }
    #[cfg(not(windows))]
    {
        let _ = &channel;
        loop {
            thread::sleep(Duration::from_secs(3600));
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_help();
        return Ok(());
    }

    if args.iter().any(|a| a == "--version" || a == "-v") {
        println!("atelier-kernel {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    if let Some(target_dir) = args
        .windows(2)
        .find(|w| w[0] == "--init")
        .map(|w| w[1].as_str())
    {
        return init_variant_skeleton(target_dir);
    }

    if let Some(address) = args
        .windows(2)
        .find(|w| w[0] == "--port-relay")
        .map(|w| w[1].as_str())
    {
        return atelier_kernel::services::ports::run(address.parse()?).map_err(Into::into);
    }
    // Load external startup configuration (`kernel_boot.json`)
    let config_path = args
        .windows(2)
        .find(|w| w[0] == "--config")
        .map(|w| w[1].as_str())
        .unwrap_or("./kernel_boot.json");

    // 控制通道模式：内核不再解析域清单，以**零域**启动，业务域由启动器逐条 attach。
    // 该分支刻意不读 `--config`（没有引导域），也不参与 `--smoke-test` / `--check`。
    if let Some(pipe_name) = flag_value(&args, "--control-pipe") {
        return run_control_host(&args, &pipe_name);
    }

    if let Some(check_pos) = args.iter().position(|a| a == "--check") {
        let check_target = args
            .get(check_pos + 1)
            .filter(|a| !a.starts_with('-'))
            .map(|s| s.as_str())
            .unwrap_or(config_path);
        let report = check_constitution(check_target);
        println!(
            "[ConstitutionCheck] Target: {} (configs={}, files={}, tables={})",
            report.target, report.checked_configs, report.checked_files, report.checked_tables
        );
        if report.is_ok() {
            println!("[ConstitutionCheck] PASSED — 0 violations found (M1–M7 compliant).");
            return Ok(());
        } else {
            eprintln!(
                "[ConstitutionCheck] FAILED — {} violation(s) found:",
                report.violations.len()
            );
            for v in &report.violations {
                eprintln!("  - {}", v);
            }
            std::process::exit(1);
        }
    }

    let boot_config = KernelBootConfig::load_from_file(config_path)?;

    if args.iter().any(|a| a == "--smoke-test") {
        return run_variant_smoke_test(config_path, &boot_config);
    }

    if args.iter().any(|a| a == "--auto" || a == "-a") {
        let kernel = AtelierKernel::open_with_db_and_root(&boot_config.db_path, &boot_config.project_root)?;
        let frontend = InteractiveFrontendDomain::attach_and_boot(&kernel);
        frontend.run_all_automated_demos();
        return Ok(());
    }

    if args.iter().any(|a| a == "--cli") {
        let kernel = AtelierKernel::open_with_db_and_root(&boot_config.db_path, &boot_config.project_root)?;
        let frontend = InteractiveFrontendDomain::attach_and_boot(&kernel);
        frontend.run_repl();
        return Ok(());
    }

    // `--headless` is accepted as a no-op: the kernel no longer embeds an HTTP
    // observation console, so every boot mode is headless by definition.
    let _headless = args.iter().any(|a| a == "--headless");

    // Default: Headless microkernel runtime without HTTP server port overhead
    #[cfg(windows)]
    let shutdown_event = shutdown::ShutdownEvent::new()?;
    let kernel = AtelierKernel::open_with_db_and_root(&boot_config.db_path, &boot_config.project_root)?;
    let spawned = kernel.supervisor().boot_from_config(&boot_config);
    println!(
        "[AtelierKernel] Microkernel headless runtime active ({} domain(s) supervised from {})",
        spawned.len(),
        config_path
    );
    #[cfg(windows)]
    {
        shutdown_event.wait()?;
        Ok(()) // Dropping the kernel runs the existing supervisor shutdown and reclamation.
    }
    #[cfg(not(windows))]
    loop { thread::sleep(Duration::from_secs(3600)); }
}
