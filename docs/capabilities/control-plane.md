# 控制通道（Control Pipe）：运行期域接入

内核以**零域**启动，业务域的装配在运行期由外部逐条接入。控制通道是这条路径上唯一的入口，也是内核起来之后、任何业务域起来之前就存在的观测入口。

- 实现：`src/frontend/control.rs`
- 传输：复用 `src/edge/endpoint.rs` 的命名端点（Windows 命名管道 / Unix `AF_UNIX`），**不开放任何 TCP 端口**
- 定位：控制平面**不属于域**。它不经过消息面、不占账本、不产生四阶段回执，直接调用 supervisor / router / observer

---

## 1. 什么时候会有控制通道

只有带 `--control-pipe` 启动才有：

```bash
atelier-kernel \
  --control-pipe atelier-control-a1b2c3d4 \
  --control-token <token> \
  --db   /abs/path/data/atelier_kernel.db \
  --root /abs/path/data
```

| 参数 | 必填 | 含义 |
| :--- | :--- | :--- |
| `--control-pipe <name>` | 是 | 裸管道名（不含 `\\.\pipe\` 前缀）。Windows 实际路径是 `\\.\pipe\<name>`，Unix 是 `/tmp/<name>.sock` |
| `--control-token <token>` | 否 | 在 `hello` 里比对的握手令牌。省略即不校验（仅本机调试） |
| `--db <path>` | 是 | 账本路径。相对路径会按**内核自己的 cwd** 解析成绝对路径 |
| `--root <path>` | 是 | C4 项目根（`project_root`）。同上 |

三点必须知道：

1. **`--config` 在这个模式下被忽略**：内核不解析任何域清单。`--check` / `--smoke-test` 仍走各自的 `--config` 路径，不受影响。
2. **`--db` / `--root` 强制要求显式给出**，没有默认值。相对路径会随启动方的 cwd 漂移，静默读错账本比启动失败更糟。
3. **空域也必须在启动时"记住" C4 根**。内核内部用 `boot_from_config` 完成这件事（它是 `files_root` 的唯一写入点，而 `files_root` 决定 `ATELIER_FILES_ROOT` 的注入）。少了这一步，之后 attach 进来的**每一个**域做 vfs 操作都会直接失败。

---

## 2. 帧格式

UTF-8、一行一个 JSON、`\n` 分隔（`\r\n` 也接受）。

请求必须带 `id`（客户端自增即可）和 `op`：

```json
{"id":2,"op":"attach","spec":{...}}
```

响应原样带回 `id`：

```json
{"id":2,"ok":true,"data":{"domain":"web","pid":12364}}
{"id":2,"ok":false,"error":"invalid_spec","message":"[M1] invalid domain_id ..."}
```

- 空行被忽略。
- 单帧上限 **4 MiB**。超过上限（且始终没有换行）会被当作坏帧，回一帧 `invalid_frame` 后断开连接，避免无限输入吃光内核内存。
- 无法解析的行回 `invalid_frame` 后**继续服务**，不因此断开。

---

## 3. op 一览

| op | 入参 | `data` | 底层 |
| :--- | :--- | :--- | :--- |
| `hello` | `token?` | `{protocol, kernel_version, pid, started_at, uptime_ms}` | 握手（配了令牌时的唯一一次校验点） |
| `attach` | `spec` | `{domain, pid}` | `supervisor.spawn_process_domain` |
| `detach` | `domain` | `{domain, was_running}` | `supervisor.stop_process_domain` |
| `update` | `domain`, `spec?`, `probe_handler?`, `timeout_ms?` | `{domain, status, new_pid, old_pid, instance_id}` | `supervisor.update_process_domain` |
| `list` | — | `{domains:[{domain_id, pid, status, restarts}]}` | `list_specs` + router 状态 |
| `snapshot` | — | `{domains, routes, messages, uptime_ms}` | `list_specs` + `all_domains` + observer 计数 |
| `ping` | — | `{pong, uptime_ms}` | 存活探测 |

### 3.1 `attach` 的 `spec`

与配置文件里的域对象**完全同构**（`DomainProcessSpec`：`domain_id` / `command` / `args` / `env` / `ports` / `working_dir` / `auto_restart` / `max_restarts` / `restart_delay_ms`），并复用同一套校验：M1（`domain_id` 为 ASCII 字母数字下划线）、C1（`env` 不得含 `ATELIER_IPC_*` 等保留键）、M6（`ports` 名字唯一且端口非零、`working_dir` 必须真实存在）。

> **`working_dir` 不补默认值。** 配置文件里的域会被自动补成配置文件所在目录（`config.rs`），动态接入的 spec **不会**——写相对路径会按内核 cwd 解析。调用方必须写绝对路径。

### 3.2 重复 `attach` 会被拒

同一个 `domain_id` 已经有活进程时返回 `already_attached`，**不会**静默替换。

理由：内核层面允许同 id 重连（那是端点身份的机制），但对装配方来说"把活着的域换掉"会丢掉全部在途状态。想让新产物生效请用 `update`——它是滚动更新，候选没就绪会自动回滚，旧代际不受影响。

### 3.3 `update` 不带 `spec`

- 该域**来自启动文件**（`--config` 起的内核）→ 重读启动文件，采用文件里的最新规格。文件里删掉了这个域，或文件读不出来 → 显式失败，旧代际保留。
- 该域是**运行期动态接入**的（不在任何启动文件里）→ 用内存里的规格。
- 内核压根没有启动文件（`--control-pipe` 模式）→ 用内存里的规格。

### 3.4 `snapshot` 与 `list` 的区别

`snapshot` 以 supervisor 的**已知域**为基准，刚 attach、还没 `register_handlers` 的域也在列表里（`status` 为 `spawning`）；`routes` 来自 router，所以未注册的域是空数组。`messages` 是观察流的事件计数（`emitted` / `dropped`），**不是**业务消息数——观察流有界且允许丢弃，所以两个数一起给。

---

## 4. 错误码

| code | 含义 | 调用方动作 |
| :--- | :--- | :--- |
| `unauthorized` | 令牌不符，或没先 `hello` 就发别的 op | 终止，不重试；随后内核会关闭连接 |
| `invalid_frame` | JSON 解析失败 / 缺 `id` 或 `op` / 单帧超限 | 报告并跳过；超长帧会断开连接 |
| `unknown_op` | 不认识的 op | 报告并跳过 |
| `invalid_spec` | `spec` 反序列化或校验失败 | 报告，**不重试**（改配置才有意义） |
| `spawn_failed` | 进程启动失败 | 报告；是否重试由调用方决定 |
| `already_attached` | 该 id 已有活进程 | 改用 `update` |
| `update_failed` | 滚动更新失败，旧代际仍存活 | 报告 |
| `unknown_domain` | `detach` / `update` 的目标不存在 | 视为已达成（幂等） |
| `shutting_down` | 内核正在退出 | 终止 |

---

## 5. 连接语义

- **单连接**：一个管道实例只服务一条连接。第二条连接在内核还在服务第一条时会连接失败（Windows 侧 `ERROR_PIPE_BUSY`），调用方应退避重试。
- **允许重连**：客户端断开后内核重建管道实例并回到 accept。内核**不会**因客户端断开而退出，已接入的域继续运行。
- **内核停机则一切结束**：内核释放时控制线程随之退场；之后的连接会被拒绝。
- **令牌只在 `hello` 比一次**：`hello` 通过后同一连接后续帧不再携带令牌。令牌不符或未 `hello` 就发别的 op 会返回 `unauthorized` 并**关闭连接**。

---

## 6. 安全边界

- **只在本机**：命名管道 / Unix socket，没有 TCP 监听。
- **令牌不是网络秘密**：它通过命令行参数传递，本机任何进程都能看到同用户进程的命令行。它的作用是防止"同机另一个无权重放连接"，不是防越权进程。
- **管道名带随机后缀**（由启动方生成），避免多安装根互相撞名。

---

## 7. 与既有契约的关系

| 条款 | 影响 |
| :--- | :--- |
| C1 端点身份 | 不变。域仍各自持有自己的端点 |
| C2 两层消息 | 不变。控制平面不属于域，不产生消息 |
| C3 四阶段回执 | 不变。控制命令走 supervisor API，不进账本 |
| C4 `files_root` | 不变，但依赖"空域启动也必须完成 C4 根登记"（见 §1 第 3 点） |
| M6 显式配置 | 不变。动态 `spec` 走同一套校验 |
| B2 认知隔离 | 不破坏：控制平面按 `domain_id` 操作，不读域实现 |

设计过程与备选方案见 [archive/design-domain-attach.md](../archive/design-domain-attach.md)。
