# 域动态接入（Control Pipe）设计草案

> 状态：**草案，未实施** ｜ 日期：2026-11-18 ｜ 影响：`core-kernel`（内核）+ `Atelier`（启动器）
> 实施完成后，本文的规范性内容应并入 [CONSTITUTION.md](../CONSTITUTION.md) 与 [ARCHITECTURE.md](../ARCHITECTURE.md)，本文留作设计记录。

---

## 1. 问题

内核现在只在启动时读一次 `kernel_boot.json`，把里面列的域全部拉起来（`main.rs:333` → `supervisor/mod.rs:1032-1043`）。这个形态有三个后果：

| # | 现象 | 根因 |
| :-- | :--- | :--- |
| 1 | 更新一次，已安装的付费域从启动配置里消失（目录还在） | `installer.rs:1601-1617` 是**整份重建**，缺授权令牌时付费源被跳过，重建后不再包含它们 |
| 2 | 两份配置漂移：`kernel_boot.json`（端口占位 8655）与 `kernel_boot.atelier.json`（实际 54794）长期不一致 | 内核只读后者，`capabilities` 读前者；`configured_port` 优先后者（`variant_runtime.rs:276-278`） |
| 3 | 装一个新域必须停整个内核 | 域只在 `boot_from_config` 里装载一次，运行期没有外部入口 |

## 2. 目标 / 非目标

**目标**
- 内核不再解析"域清单"；以空域启动，由外部逐个接入。
- 接入/移除在**内核运行期**完成，不需要重启内核。
- 安装树成为域装配的唯一真相来源；不再有第二份会被重建的配置。

**非目标**
- 不改域侧的 SDK 与消息契约。
- 不改 `--check` / `--smoke-test` 对**开发变体**的语义（变体配置继续存在）。
- 不做域的远程接入（控制通道仅本机）。

## 3. 总体形态

```
启动器                                         内核
  │  生成管道名 + token                          │
  ├── spawn ──→ --db --root --control-pipe ────→│ 空域启动
  │                                             │ 建 8 套 kernel.* 基础设施
  │                                             │ 开控制管道，等待命令
  ├── connect + hello ─────────────────────────→│
  │                                             │
  ├── attach{spec:web} ────────────────────────→│ spawn_process_domain
  │←──────────── {ok, pid}                      │
  ├── attach{spec:engines} ────────────────────→│ …
  ├── …                                         │
  ├── snapshot ────────────────────────────────→│
  │←──── {domains:[…], routes, messages}        │
  └── 打开工作台（web 域端口）                    │
```

关键点：**引导期零域**。可观测性由内核自带的 8 套 `kernel.*` 基础设施保证（`lib.rs:117-126` 在域之前就建好），控制管道本身就是观测入口，不需要 web 域先起来。

## 4. 内核改动

### 4.1 新增命令行参数

| 参数 | 含义 | 缺省 |
| :--- | :--- | :--- |
| `--db <path>` | 账本路径 | 若给了 `--control-pipe` 则必填 |
| `--root <path>` | C4 对象根（`project_root`） | 同上 |
| `--control-pipe <name>` | 命名管道名（不含 `\\.\pipe\` 前缀） | 不启用控制通道 |
| `--control-token <token>` | 握手令牌 | 不校验（仅本机调试） |

**兼容规则**：`--control-pipe` 存在时**不读** `--config`，在内存里构造 `domains: []` 的 `KernelBootConfig`；否则保持现有行为（读 `--config`，缺省 `./kernel_boot.json`）。

### 4.2 启动流程

改造后的 headless 分支：

```rust
let control = args.find("--control-pipe");
if let Some(name) = control {
    let cfg = KernelBootConfig::from_cli(db, root);   // domains: []
    let kernel = AtelierKernel::open_with_db_and_root(&cfg.db_path, &cfg.project_root)?;
    kernel.supervisor().boot_from_config(&cfg);       // ← 必须调用
    control::spawn(kernel.clone(), name, token)?;
    shutdown_event.wait()?;
}
```

**`boot_from_config` 不能省**：它是 `files_root` 的唯一写入点（`supervisor/mod.rs:1036-1037`），而 `files_root` 决定 `ATELIER_FILES_ROOT` 的注入（`:474-478`）。不调用 → 所有域的 vfs 操作直接抛错（SDK 侧校验）。空 `domains: []` 调用它是合法的：返回空 `Vec`，只完成"记路径 + 记根"两件事。

### 4.3 控制管道模块

新增 `src/frontend/control.rs`（或 `src/control/`）：

- 一个线程 accept 连接，读行分隔 JSON，逐条处理，写回一行 JSON。
- 直接调用 `kernel.supervisor()` / `kernel.router()` / `kernel.observer()`，与内核自带的 web 前端同一层次（**不经过消息面**——控制平面不属于域，不占账本、不产生四阶段回执）。
- 单连接：已有连接时新连接被拒绝（回 `{"ok":false,"error":"busy"}`）。
- 客户端断开后回到 accept，允许重连；内核不因断开而退出。

### 4.4 `UpdateDomain` 的 spec 来源

`supervisor/mod.rs:1142-1158` 现在的逻辑是"只要 `boot_config_path` 有值就重读文件"，动态接入的域不在文件里 → 不带 spec 的更新必失败。

改为：**先读文件，文件里没有该域才落回内存规格**（`spec_from_boot_file`）。

> 注意这里和最初设想的"先内存后文件"是反的。宪法「启动配置与滚动更新」要求**文件装配的域**执行无 spec 更新时必须重读启动文件取最新规格——内存优先会直接废掉这条契约（改 `kernel_boot.json` 再 update 的流程会静默用旧规格）。正确顺序是：文件里有 → 用文件里的；文件里没有（或根本没有启动文件）→ 用内存里的。前者保住旧契约，后者让动态域可用。

### 4.5 移除内置观察台

删除 `src/frontend/web.rs`（HTTP 观察台）与 `--web-ui` / `--dashboard` 分支。

**兼容约束**：`KernelBootConfig` 的 `web_ui_bind` / `web_ui_enabled` **必须保留为被忽略的废弃字段**——配置是 `deny_unknown_fields`（`config.rs:39`），删字段会让所有现存配置（发行版、开发变体、用户装机）解析失败。等到下一个大版本再删。

## 5. 控制协议

### 5.1 帧

UTF-8、一行一个 JSON、`\n` 分隔。请求必须带 `id`（客户端自增），响应原样带回。

```jsonc
{"id":1,"op":"snapshot"}
{"id":1,"ok":true,"data":{...}}
```

错误统一形态：`{"id":n,"ok":false,"error":"<code>","message":"<人话>"}`。

### 5.2 op 一览

| op | 入参 | 出参 data | 底层 |
| :--- | :--- | :--- | :--- |
| `hello` | `token?` | `{kernel_version, pid, started_at}` | 握手 |
| `attach` | `spec`（完整 `DomainProcessSpec`） | `{domain, pid}` | `supervisor.spawn_process_domain` |
| `detach` | `domain` | `{was_running}` | `supervisor.stop_process_domain` |
| `update` | `domain`, `spec?` | `{status}` | `supervisor.update_process_domain` |
| `list` | — | `{domains:[{domain_id,pid,status,restarts}]}` | `supervisor.list_specs` + `router` 状态 |
| `snapshot` | — | `{domains, routes, messages, uptime_ms}` | `router.active_routes` + observer 计数 |
| `ping` | — | `{pong:true}` | 存活探测 |

`attach` 的 `spec` 与配置文件里的域对象**完全同构**（`DomainProcessSpec`），校验规则复用 `config.rs:199-254`（M1/C1/M6）。

### 5.3 错误码

| code | 含义 | 客户端动作 |
| :--- | :--- | :--- |
| `busy` | 已有控制连接 | 等待重试 |
| `unauthorized` | 令牌不符，或没先 `hello` | 终止，不重试 |
| `invalid_frame` | JSON 解析失败 / 缺 `id` 或 `op` / 单帧超过 4 MiB | 报告并跳过（超长帧会断开连接） |
| `unknown_op` | 不认识的 op | 报告并跳过 |
| `invalid_spec` | spec 校验失败（M1 / C1 / M6） | 报告，**不重试** |
| `spawn_failed` | 进程启动失败 | 报告；是否重试由启动器决定 |
| `already_attached` | 该 id 已有活进程 | **不要改走 attach**，改用 `update` |
| `update_failed` | 滚动更新失败（保留旧代际） | 报告；旧进程仍在跑 |
| `unknown_domain` | detach/update 目标不存在 | 视为已达成（幂等） |
| `shutting_down` | 内核正在退出 | 终止 |

### 5.4 安全

- 管道名由启动器生成，含随机后缀，避免多安装根冲突（与域端点 `\\.\pipe\atelier-<domain>-<nonce>` 同思路，`edge/endpoint.rs:12-26`）。
- `--control-token` 在 `hello` 里比对；令牌只走命令行参数（本机进程可见性而已，不是网络秘密）。
- 控制平面**只能本机**：不开放 TCP。

## 6. 启动器改动

### 6.1 spec 推导（不再写配置文件）

| 字段 | 来源 |
| :--- | :--- |
| `domain_id` | `catalog.rs` 的域清单（web/scheduler/pipelines/engines/publishers/daemons） |
| `command` | `bootspec::find_entry()` 三条规则：`<kebab>.exe` → `<kebab>/<kebab>.exe` → `<前缀>-host/<dir>.exe` |
| `args` | `[]` |
| `env` | `variant_runtime::variant_environment()` 现算（`ATELIER_ROOT` / `GCP_MODELS_DIR` / `PYTHONPATH`） |
| `ports` | `variant_runtime::assign_ports()` 现分配（仅 web 有 http） |
| `working_dir` | **必须写绝对路径**（动态 spec 不会像配置那样被补成配置目录，见 `config.rs:109-119`） |
| `auto_restart` 等 | 默认值（true / 10 / 500ms） |

"用户禁用了某域"属于用户设置，进 `launcher.json`，不进 spec 推导。

### 6.2 启动时序

1. 生成 `control_pipe = atelier-control-<rand>`、`token`。
2. spawn 内核：`--db <root>/data/atelier_kernel.db --root <root>/data --control-pipe <name> --control-token <token>`。
3. 连管道 → `hello`。
4. 逐条 `attach`（串行；失败即记录并继续，最后统一报告）。
5. `snapshot` 校验：期望的域全部 active；否则提示"部分域未就绪"。
6. 打开工作台（web 域端口，从 attach 回执/端口分配结果取）。

### 6.3 失败与重试

- `attach` 失败：写日志 + 界面标红；**不自动重试**（域起不来通常不是瞬时的）。
- 控制管道断开：重连（3 次退避），成功后 `list` 重建视图。
- 内核退出：走现有 `stop_sync` 路径，不额外处理。

### 6.4 观测

启动器界面的域状态改读 `snapshot`（而非 `/api/assembly`）。`/api/assembly` 仍然存在，但它来自 web 域，web 域没起来时不可用——控制管道是唯一在**任何时刻**都可用观测入口。

## 7. 与宪法 / 契约的关系

| 条款 | 影响 |
| :--- | :--- |
| C1 端点身份 | 不变。域仍然是各自端点的持有者 |
| C2 两层消息 | 不变。控制平面不属于域，不产生消息 |
| C3 四阶段回执 | 不变。控制命令走 supervisor API，不进账本（与旧 web_ui 前端一致，`web.rs:244` 也是直接调 supervisor） |
| C4 `files_root` | 不变，但依赖"空启动也必须调 `boot_from_config`" |
| M6 `deny_unknown_fields` | `web_ui_*` 字段保留为废弃字段，避免老配置解析失败 |
| B2 认知隔离 | 不破坏：控制平面不读域实现，只按 id 操作 |
| §139-141（无 spec 更新须重读启动文件） | **需修订**：改为"先内存、后文件"，否则动态域无法更新 |

## 8. 兼容与发版

- 内核新增参数是**增量**：老启动器照旧用 `--config`，行为不变。
- 新启动器依赖 `--control-pipe`：**老内核不认**，所以顺序必须是 **先发内核 → 再发启动器**。
- 回滚：内核保留 `--config` 路径；启动器切换开关可回退到旧流程（建议保留一个构建期 feature flag）。

## 9. 分阶段与验收

| 阶段 | 内容 | 验收 |
| :--- | :--- | :--- |
| 0 | 内核加 `--db/--root/--control-pipe` + control 模块 + `hello/attach/list` | 手工用命名管道 attach 一个 scheduler，`list` 能看到、`snapshot` 有路由 |
| 1 | `detach` / `update` / 错误码 | 接入后停掉；重启后重连 `list` 为空 |
| 2 | `UpdateDomain` 改"先内存后文件" | 动态域不带 spec 也能更新 |
| 3 | 移除 web_ui（含兼容字段保留） | `cargo check --all-targets` 通过；老配置仍能解析 |
| 4 | 启动器切新流程 | 5 域全部 active，工作台正常 |
| 5 | ~~拖拽安装 = attach 一条~~ | 已取消（2026-11-21）：安装/更新统一由启动器完成，装完整体换入并重启实例，不做窗口级拖放 |

每阶段的验证命令：`cargo test`、`atelier-kernel --check ./kernel_boot.json`、`--smoke-test`（阶段 0-2 期间仍用现有变体配置做回归）。

## 10. 风险

1. **控制平面成为单点**：启动器崩了没人能接入新域，但内核与已接入的域继续运行；重连即可恢复。
2. **零域窗口**：内核起来到第一次 attach 之间没有 UI。控制管道提供 `snapshot` 兜底。
3. **端口钉死**：内核生命周期内 `(domain, port_name)` 的监听不能改（`supervisor/mod.rs:1169-1175`），换端口要重启内核。与现状一致。
4. **`--check` 覆盖面**：生产装机的域是编译产物，本来就没有源码可扫；静态门禁的覆盖面只对开发变体有意义，变体配置保留后不受影响。
5. **重复 attach 同 id 会静默替换**（`router.rs:490-496`），启动器必须自己防重。
6. **动态 spec 的 `working_dir`** 不补默认值，写相对路径会按内核 cwd 解析——启动器必须写绝对路径。

## 11. 开工清单

| # | 内容 | 状态 |
| :-- | :--- | :--- |
| 1 | `main.rs`：参数解析 + 空配置构造 + 控制通道分支 | ✅ 已完成（`--db`/`--root` 为必填，且相对路径会被解析成绝对路径） |
| 2 | `supervisor/mod.rs`：`update_process_domain` 的 spec 来源改为"文件优先、内存兜底" | ✅ 已完成 |
| 3 | `src/frontend/control.rs`：协议实现（hello/attach/detach/update/list/snapshot/ping） | ✅ 已完成 |
| 4 | 移除 `frontend/web.rs` 与 `--web-ui` 分支，保留废弃配置字段 | ✅ 已完成 |
| 5 | 文档：`INDEX.md`、`ARCHITECTURE.md`、`VARIANT_GUIDE.md` 同步；`CONSTITUTION.md` 修订 §139-141 | ✅ 已完成（新增 `capabilities/control-plane.md`） |
| 6 | 启动器：`variant_runtime::open_workbench` 改新流程；新增控制客户端模块 | ✅ 已完成（`Atelier/src-tauri/src/control.rs` + `open_workbench_blocking`） |
| 7 | 启动器：域状态改读 `snapshot`；`installed` 改由安装树判断 | ✅ 已完成（`capabilities.rs`） |
| 8 | 启动器：拖拽安装 → 写清单（`launcher.json` 内）+ attach | 已取消（2026-11-21） |

### 11.2 启动器侧的实现决定（2026-11-18）

| 决定 | 理由 |
| :--- | :--- |
| 装配清单 = `catalog` 的 6 个 id × `bootspec::find_entry(root, "atelier-<id>")` | 域 id 是启动器自己的知识（catalog），入口路径是安装树的事实。两者相乘就是"装了什么"，不需要任何清单文件。 |
| 端口每次启动现挑，不写配置文件 | 顺便解决了"两份配置端口漂移"（病 #2）：`kernel_boot.atelier.json` 不再被生成或读取。 |
| 控制通道句柄只放内存，令牌不落盘 | 令牌写文件只是把它变成另一个要保护的秘密。启动器重启后仍能靠 `--port-relay` 子进程找到端口、靠 `/api/assembly` 看状态，唯一失去的是"运行期再接入一个域"，重开一次工作台即可恢复。 |
| 每条命令新建一次连接 | 内核的管道实例是一次性的、断开即重建。实测 20 次连续 connect→hello→snapshot→断开：20/20 成功，平均 1.6 ms。 |
| `set_domain` / `domain_activity` 补上 | 前端早就在调这两个命令，但它们从来没注册过（点了必然报错）。现在关域 = 控制通道 `detach`，不重启内核。 |
| `domain_activity` 一律要求确认 | 内核不提供"这个域还有多少在途任务"，启动器没有可信旁路。宁可多一次点击，也不猜它空闲。 |

### 11.3 仍未做

- `kernel_boot.json` 现在**只写不读**：安装器还在推导它（对开发变体和 `--check` / `--smoke-test` 有用），但启动器启动时不再读它。它是产物记录，不再是装配来源。
- ~~拖拽安装：`control::update` 已经是热更新入口，等安装流程接过来。~~ **已取消（2026-11-21）**：不做窗口级拖放；安装/更新由启动器整体换入后重启实例，`control::update` 的启动器包装已删除（内核侧协议保留）。
- ~~试用期 / 到期自动删除~~ **已取消（2026-11-21）**：授权是买断制，不做试用，也不做到期回收。

### 11.1 已验证（2026-11-18，实测输出）

零域内核 + 命名管道，端到端跑通：

```
hello          -> {"data":{"kernel_version":"0.0.2","pid":46940,"protocol":1,"uptime_ms":2372},"ok":true}
attach         -> {"data":{"domain":"ctrl_probe","pid":12364},"ok":true}
list           -> {"domains":[{"domain_id":"ctrl_probe","pid":12364,"status":"active","restarts":0}]}
snapshot       -> {"domains":[{"routes":["ctrl_probe.ping"],"status":"active"}],
                   "routes":["ctrl_probe.ping"],"messages":{"emitted":6,"dropped":0},"uptime_ms":2695}
detach         -> {"data":{"domain":"ctrl_probe","was_running":true},"ok":true}
update(nospec) -> {"data":{"status":"updated","instance_id":2,"new_pid":46920,"old_pid":13320},"ok":true}
attach(dup)    -> {"error":"already_attached"}
attach(bad)    -> {"error":"invalid_spec","message":"[M1] invalid domain_id \"bad.name\"…"}
hello(badtoken)-> {"error":"unauthorized"} + 连接被关闭
```

断线重连也验证过：客户端断开后管道实例重建，第二个连接正常握手并看到同一个内核（pid 不变）、同一个域。

`frontend/control` 上的位置：控制模块放在 `src/frontend/control.rs`，复用 `edge::endpoint` 的命名端点传输（Windows `CreateNamedPipeW` / Unix `AF_UNIX`），不新引入依赖。
