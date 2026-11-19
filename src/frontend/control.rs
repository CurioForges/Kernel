//! 控制通道：内核**运行期**的域接入 / 移除 / 观测入口。
//!
//! 形态：命名端点（Windows `\\.\pipe\<name>` / Unix `AF_UNIX`）上的行分隔 JSON ——
//! 一行一个请求，一行一个响应，请求的 `id` 原样带回。
//!
//! 控制平面**不属于域**：不经过消息面、不占账本、不产生四阶段回执，直接调用
//! supervisor / router / observer（与已移除的内置 HTTP 观察台同一层次）。
//!
//! 它存在的理由：内核以**零域**启动后，这是任何时刻都可用（不需要 web 域先起来）
//! 的装配与观测入口；域清单不再由启动配置决定，而由外部逐条 `attach`。

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::AtelierKernel;
use crate::core::protocol::DomainStatus;
use crate::edge::endpoint;
use crate::supervisor::config::{DomainProcessSpec, SupervisorError};

/// 单帧上限。超过即视为坏帧：拒绝没有 `\n` 的无限输入把内核内存吃光。
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

/// 单次管道读取的块大小。
const READ_CHUNK: usize = 8 * 1024;

/// 无 spec 滚动更新的默认超时（毫秒）。
const DEFAULT_UPDATE_TIMEOUT_MS: u64 = 5_000;

/// 内核接受第一条控制命令的时刻（`--control-pipe` 分支里初始化）。
static STARTED_AT: OnceLock<Instant> = OnceLock::new();

/// 与 [`STARTED_AT`] 对应的 RFC3339 时间戳，供 `hello` 回报。
static STARTED_ISO: OnceLock<String> = OnceLock::new();

/// 控制通道的存活句柄。丢弃它不会立刻停掉监听线程（它只持有 `Weak`，
/// 内核被释放后线程会在下一次循环自行退场），但 [`ControlChannel::stop`]
/// 会让线程在两次连接之间尽快退出。
pub struct ControlChannel {
    path: String,
    stop: Arc<AtomicBool>,
}

impl ControlChannel {
    /// 端点路径（Windows 是 `\\.\pipe\<name>`，Unix 是 `/tmp/<name>.sock`）。
    pub fn path(&self) -> &str {
        &self.path
    }

    /// 请求监听线程退出。阻塞中的 `accept` 要到下一次连接才会被唤醒——
    /// 命名管道的 accept 在 Windows 上不可轮询，不做额外线程来强行打断它。
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// 端点完整路径。`--control-pipe` 传的是**裸名字**（不含 `\\.\pipe\` 前缀），
/// 与域端点的命名风格一致，但名字由启动器生成（含随机后缀，避免多安装根冲突）。
fn pipe_path(name: &str) -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\{name}")
    }
    #[cfg(unix)]
    {
        format!("/tmp/{name}.sock")
    }
}

/// 启动控制通道：在后台线程里 accept → 服务 → 断线后重建管道等待重连。
///
/// 只持有 `Weak<AtelierKernel>`：内核一旦释放（主线程退出、`AtelierKernel::drop`
/// 触发 supervisor 停机），线程的 `upgrade()` 就会失败并退场，不会拖住关机。
pub fn spawn(
    kernel: &Arc<AtelierKernel>,
    pipe_name: &str,
    token: Option<String>,
) -> io::Result<ControlChannel> {
    let path = pipe_path(pipe_name);
    let _ = STARTED_AT.set(Instant::now());
    let _ = STARTED_ISO.set(chrono::Utc::now().to_rfc3339());

    let bind_path = path.clone();
    let weak = Arc::downgrade(kernel);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let expected_token = token.filter(|token| !token.is_empty());

    std::thread::Builder::new()
        .name("atelier-control".into())
        .spawn(move || accept_loop(&bind_path, weak, expected_token, thread_stop))?;

    Ok(ControlChannel { path, stop })
}

fn accept_loop(
    path: &str,
    weak: Weak<AtelierKernel>,
    expected_token: Option<String>,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::Acquire) || weak.strong_count() == 0 {
            return;
        }
        // 管道实例是一次性的：一个实例只服务一条连接，断开后重建才能重连。
        let mut listener = match endpoint::bind(path) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("[ControlPipe] bind failed on {path}: {error}");
                std::thread::sleep(Duration::from_millis(200));
                continue;
            }
        };
        match listener.accept() {
            Ok(stream) => serve(stream, &weak, expected_token.as_deref()),
            Err(error) => {
                eprintln!("[ControlPipe] accept failed: {error}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// 一条连接的服务循环。返回即表示连接结束（对端关闭、坏帧、或内核停机）。
fn serve(
    mut stream: endpoint::EndpointStream,
    weak: &Weak<AtelierKernel>,
    expected_token: Option<&str>,
) {
    let mut session = Session::new(expected_token);
    let mut buffer: Vec<u8> = Vec::new();

    loop {
        let line = match read_frame(&mut stream, &mut buffer) {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                // 超长帧：回一帧再断开，否则客户端只会看到静默。
                let _ = write_frame(
                    &mut stream,
                    &error_response(Value::Null, "invalid_frame", error.to_string()),
                );
                return;
            }
            Err(_) => return,
        };

        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                if write_frame(
                    &mut stream,
                    &error_response(Value::Null, "invalid_frame", error.to_string()),
                )
                .is_err()
                {
                    return;
                }
                continue;
            }
        };

        let Some(kernel) = weak.upgrade() else {
            let _ = write_frame(
                &mut stream,
                &error_response(request_id(&request), "shutting_down", "kernel is shutting down"),
            );
            return;
        };

        let (response, next) = dispatch(&kernel, &mut session, &request);
        if write_frame(&mut stream, &response).is_err() {
            return;
        }
        if next == Next::Close {
            return;
        }
    }
}

/// 读完一行。返回 `Ok(None)` 表示对端关闭；`InvalidData` 表示帧超过上限。
fn read_frame(
    stream: &mut endpoint::EndpointStream,
    buffer: &mut Vec<u8>,
) -> io::Result<Option<String>> {
    loop {
        if let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = buffer.drain(..=position).collect();
            line.pop(); // 分隔用的 '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if line.iter().all(|byte| byte.is_ascii_whitespace()) {
                continue; // 空行不属于协议
            }
            return Ok(Some(String::from_utf8_lossy(&line).into_owned()));
        }
        if buffer.len() > MAX_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame exceeds {MAX_FRAME_BYTES} bytes without a newline"),
            ));
        }
        let mut chunk = [0u8; READ_CHUNK];
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(None),
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if is_peer_closed(&error) => return Ok(None),
            Err(error) => return Err(error),
        }
    }
}

/// 对端关闭在 Windows 上是 `ERROR_BROKEN_PIPE`(109) / `ERROR_NO_DATA`(232)，
/// 不是 `Ok(0)` —— 必须显式识别，否则断线会被当成致命错误反复刷日志。
fn is_peer_closed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
    ) || matches!(error.raw_os_error(), Some(109 | 232))
}

fn write_frame(stream: &mut endpoint::EndpointStream, frame: &Value) -> io::Result<()> {
    let mut encoded = serde_json::to_vec(frame).map_err(io::Error::other)?;
    encoded.push(b'\n');
    stream.write_all(&encoded)?;
    stream.flush()
}

/// 连接内的会话状态。令牌只在 `hello` 里比对一次，之后的帧沿用判定结果。
struct Session {
    expected_token: Option<String>,
    authed: bool,
}

impl Session {
    fn new(expected_token: Option<&str>) -> Self {
        Self {
            expected_token: expected_token.map(str::to_string),
            // 没有配置令牌（本机调试）时不设门槛。
            authed: expected_token.is_none(),
        }
    }
}

/// 处理完当前帧之后连接的走向。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    Continue,
    Close,
}

fn dispatch(kernel: &AtelierKernel, session: &mut Session, request: &Value) -> (Value, Next) {
    let Some(id) = request.get("id").cloned() else {
        return (
            error_response(
                Value::Null,
                "invalid_frame",
                "every request needs an \"id\" correlation field",
            ),
            Next::Continue,
        );
    };
    let Some(op) = request.get("op").and_then(Value::as_str) else {
        return (
            error_response(id, "invalid_frame", "every request needs a string \"op\" field"),
            Next::Continue,
        );
    };

    if op == "hello" {
        if let Some(expected) = session.expected_token.as_deref() {
            if request.get("token").and_then(Value::as_str) != Some(expected) {
                session.authed = false;
                return (
                    error_response(id, "unauthorized", "control token mismatch"),
                    Next::Close,
                );
            }
        }
        session.authed = true;
        return (ok_response(id, hello_payload()), Next::Continue);
    }

    if !session.authed {
        return (
            error_response(id, "unauthorized", "call `hello` with a valid token first"),
            Next::Close,
        );
    }

    match op {
        "ping" => (ok_response(id, json!({ "pong": true, "uptime_ms": uptime_ms() })), Next::Continue),
        "attach" => attach(kernel, id, request),
        "detach" => detach(kernel, id, request),
        "update" => update(kernel, id, request),
        "list" => (ok_response(id, list_domains(kernel)), Next::Continue),
        "snapshot" => (ok_response(id, snapshot(kernel)), Next::Continue),
        _ => (
            error_response(
                id,
                "unknown_op",
                format!("unsupported control op {op:?}; expected hello|attach|detach|update|list|snapshot|ping"),
            ),
            Next::Continue,
        ),
    }
}

fn attach(kernel: &AtelierKernel, id: Value, request: &Value) -> (Value, Next) {
    if kernel.supervisor().is_shutting_down() {
        return (
            error_response(id, "shutting_down", "kernel is shutting down"),
            Next::Continue,
        );
    }
    let Some(raw) = request.get("spec") else {
        return (
            error_response(id, "invalid_spec", "attach requires a \"spec\" object"),
            Next::Continue,
        );
    };
    let spec: DomainProcessSpec = match serde_json::from_value(raw.clone()) {
        Ok(spec) => spec,
        Err(error) => {
            return (
                error_response(
                    id,
                    "invalid_spec",
                    format!("\"spec\" does not match DomainProcessSpec: {error}"),
                ),
                Next::Continue,
            )
        }
    };
    if let Err(reason) = spec.validate() {
        return (error_response(id, "invalid_spec", reason), Next::Continue);
    }

    let domain = spec.domain_id.clone();
    let supervisor = kernel.supervisor();
    if supervisor.get_spec(&domain).is_some()
        && supervisor.get_pid(&domain).is_some()
        && !supervisor.is_stopped(&domain)
    {
        // 同 id 再 attach 会静默替换掉活进程，丢掉全部在途状态。
        // 想让新产物生效请用 `update`（滚动更新，失败保留旧代际）。
        return (
            error_response(
                id,
                "already_attached",
                format!("domain {domain:?} is already running; use `update` to replace it"),
            ),
            Next::Continue,
        );
    }

    match supervisor.spawn_process_domain(spec) {
        Ok(pid) => (
            ok_response(id, json!({ "domain": domain, "pid": pid })),
            Next::Continue,
        ),
        Err(error) => (error_response(id, "spawn_failed", error.to_string()), Next::Continue),
    }
}

fn detach(kernel: &AtelierKernel, id: Value, request: &Value) -> (Value, Next) {
    let Some(domain) = request.get("domain").and_then(Value::as_str) else {
        return (
            error_response(id, "invalid_frame", "detach requires a string \"domain\" field"),
            Next::Continue,
        );
    };
    let supervisor = kernel.supervisor();
    if supervisor.get_spec(domain).is_none() {
        return (
            error_response(id, "unknown_domain", format!("no such domain {domain:?}")),
            Next::Continue,
        );
    }
    let was_running = supervisor.stop_process_domain(domain);
    (
        ok_response(id, json!({ "domain": domain, "was_running": was_running })),
        Next::Continue,
    )
}

fn update(kernel: &AtelierKernel, id: Value, request: &Value) -> (Value, Next) {
    let Some(domain) = request.get("domain").and_then(Value::as_str) else {
        return (
            error_response(id, "invalid_frame", "update requires a string \"domain\" field"),
            Next::Continue,
        );
    };
    let spec = match request.get("spec") {
        None | Some(Value::Null) => None,
        Some(raw) => match serde_json::from_value::<DomainProcessSpec>(raw.clone()) {
            Ok(mut spec) => {
                if spec.domain_id.is_empty() {
                    spec.domain_id = domain.to_string();
                }
                if spec.domain_id != domain {
                    return (
                        error_response(
                            id,
                            "invalid_spec",
                            format!(
                                "spec.domain_id {:?} does not match the updated domain {domain:?}",
                                spec.domain_id
                            ),
                        ),
                        Next::Continue,
                    );
                }
                if let Err(reason) = spec.validate() {
                    return (error_response(id, "invalid_spec", reason), Next::Continue);
                }
                Some(spec)
            }
            Err(error) => {
                return (
                    error_response(
                        id,
                        "invalid_spec",
                        format!("\"spec\" does not match DomainProcessSpec: {error}"),
                    ),
                    Next::Continue,
                )
            }
        },
    };
    let probe_handler = request.get("probe_handler").and_then(Value::as_str);
    let timeout_ms = request
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_UPDATE_TIMEOUT_MS);

    match kernel.supervisor().update_process_domain(
        domain,
        spec,
        probe_handler,
        Duration::from_millis(timeout_ms),
    ) {
        Ok(result) => (
            ok_response(
                id,
                json!({
                    "domain": result.domain_id,
                    "status": result.status,
                    "new_pid": result.new_pid,
                    "old_pid": result.old_pid,
                    "instance_id": result.instance_id,
                }),
            ),
            Next::Continue,
        ),
        Err(SupervisorError::UnknownDomainSpec(_)) => (
            error_response(id, "unknown_domain", format!("no such domain {domain:?}")),
            Next::Continue,
        ),
        Err(error) => (
            error_response(id, "update_failed", error.to_string()),
            Next::Continue,
        ),
    }
}

fn list_domains(kernel: &AtelierKernel) -> Value {
    let supervisor = kernel.supervisor();
    let domains: Vec<Value> = supervisor
        .list_specs()
        .into_iter()
        .map(|spec| {
            let domain = spec.domain_id;
            json!({
                "domain_id": domain,
                "pid": supervisor.get_pid(&domain),
                "status": status_value(supervisor.router().domain_status(&domain)),
                "restarts": supervisor.restart_count(&domain),
            })
        })
        .collect();
    json!({ "domains": domains })
}

fn snapshot(kernel: &AtelierKernel) -> Value {
    let supervisor = kernel.supervisor();
    let router = kernel.router();
    // 以 supervisor 的已知域为准：刚 attach、还没 register_handlers 的域不在 router 里，
    // 但启动器需要看到它们（spawning 也是状态）。
    let observed: std::collections::BTreeMap<String, (DomainStatus, Vec<String>)> = router
        .all_domains()
        .into_iter()
        .map(|(domain, status, routes)| (domain, (status, routes)))
        .collect();

    let domains: Vec<Value> = supervisor
        .list_specs()
        .into_iter()
        .map(|spec| {
            let domain = spec.domain_id;
            let (status, routes) = match observed.get(&domain) {
                Some((status, routes)) => (status_value(Some(*status)), routes.clone()),
                None => (Value::Null, Vec::new()),
            };
            json!({
                "domain_id": domain,
                "pid": supervisor.get_pid(&domain),
                "status": status,
                "restarts": supervisor.restart_count(&domain),
                "routes": routes,
            })
        })
        .collect();

    let observer = kernel.observer();
    json!({
        "domains": domains,
        "routes": router.active_routes(),
        // 观察流计数（不是业务消息计数）：有界、允许丢弃，所以两个数一起给。
        "messages": {
            "emitted": observer.emitted_total(),
            "dropped": observer.dropped_total(),
        },
        "uptime_ms": uptime_ms(),
    })
}

fn hello_payload() -> Value {
    json!({
        // 协议版本：帧格式或 op 语义不兼容变更时递增。
        "protocol": 1,
        "kernel_version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "started_at": STARTED_ISO.get().map(String::as_str),
        "uptime_ms": uptime_ms(),
    })
}

fn status_value(status: Option<DomainStatus>) -> Value {
    match status {
        Some(status) => serde_json::to_value(status).unwrap_or(Value::Null),
        None => Value::Null,
    }
}

fn uptime_ms() -> u64 {
    STARTED_AT
        .get()
        .map_or(0, |started| started.elapsed().as_millis() as u64)
}

fn request_id(request: &Value) -> Value {
    request.get("id").cloned().unwrap_or(Value::Null)
}

fn ok_response(id: Value, data: Value) -> Value {
    json!({ "id": id, "ok": true, "data": data })
}

fn error_response(id: Value, code: &str, message: impl Into<String>) -> Value {
    json!({ "id": id, "ok": false, "error": code, "message": message.into() })
}
