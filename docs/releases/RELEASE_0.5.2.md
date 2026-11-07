# Atelier Kernel v0.5.2 发布说明

**域进程实例退出时回收 active progress**：域停止、崩溃、滚动更新或重启后，内核不再保留该实例遗留的 `queued/running` 进度，避免 `query_progress` 和 SSE 初始快照长期出现孤儿进度卡。

## 变更

- `LogRecord` 增加内核内部字段 `source_instance`，只用于活动进度归属，不序列化给域或前端，也不进入 `hub_logs`；
- 域日志帧到达时，由路由器写入当前认证进程实例 ID；
- `KernelLogHub` 新增按实例清理与按域清理：
  - `clear_instance_progress(domain, instance_id)`
  - `clear_domain_progress(domain)`（in-process 域）；
- supervisor 在以下路径清理对应实例：
  - 域停止 / 崩溃回收；
  - 自动重启；
  - 滚动更新成功后的旧实例；
  - 更新候选失败、ingress 切换失败、commit 失败；
  - retiring 旧实例最终回收；
  - 内核 shutdown 回收；
- in-process 域停止时按域清理。

清理只移除旧实例的进度；新实例刚写入的同域进度不会被误删。终态仍沿用原 `(source_domain, key)` 收口语义。

## 兼容性

- 无 SDK API、消息结构或 `hub_logs` schema 变更；
- `source_instance` 是内核内部字段，不进入日志 JSON 或历史查询；
- 升级内核运行时后需要重启一次内核，旧进程内存中的 active progress 会随旧内核退出消失。

## 验证

- `cargo test --lib`：51 passed；
- 新增 `instance_cleanup_removes_only_the_restarted_generation`，覆盖按实例清理且不误删其它实例；
- `cargo build --release` 生成新运行时二进制；
- 本机短管线任务（S04/S05/S08 进度）验收通过。

## 升级方式

```bash
pip install --upgrade atelier-sdk==0.5.2
npm install atelier-sdk@0.5.2
```

内核运行时包与 SDK 版本号必须一致。
