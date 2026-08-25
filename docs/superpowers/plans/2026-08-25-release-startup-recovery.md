# Release Startup Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 正式安装首次启动时自动越过瞬时 IPC 未就绪和历史失败激活回执，进入当前有效 DSH，并避免误报更新通道未配置。

**Architecture:** 前端在初始化边界做有限重试，Rust 冷启动把“失败候选需显式重试”与“当前 deployment 可启动”拆开处理。桌面更新文案只修正状态语义，不改变状态机。

**Tech Stack:** TypeScript、Vitest、Rust、Tokio、Cargo test、Tauri 2

**Spec:** `docs/superpowers/specs/2026-08-25-release-startup-recovery-design.md`

## Global Constraints

- 只修改启动恢复所需代码，不重构无关模块。
- 不新增生产依赖，不删除用户文件，不改变更新签名和来源校验。
- 保持事件优先于快照、单调 revision 和错误正文脱敏规则。
- 使用 PowerShell 7 执行 Windows 命令。

---

### Task 1: 启动页瞬时 IPC 恢复

**Files:**
- Modify: `src/main.ts:310-378`
- Test: `src/main.test.ts:354-385`

**Interfaces:**
- Consumes: Tauri `listen<T>()` 与 `invoke<T>()` Promise 接口。
- Produces: `retryStartupSync<T>(operation: () => Promise<T>): Promise<T>`，供三个初始化任务共享。

- [ ] **Step 1: Write the failing test**

在 `src/main.test.ts` 增加行为测试：`get_runtime_status` 第一次拒绝、第二次返回完整 `{ phase: "ready", message: "DSH 已就绪", url, pid }`；使用假定时器推进重试间隔，断言真实 DOM 最终显示“DSH 已就绪”且不显示 `status_unavailable`。该测试防止删除有限重试或在第一次失败时提前渲染永久错误。

- [ ] **Step 2: Run test to verify it fails**

Run: `pnpm vitest run src/main.test.ts -t "首次 IPC 瞬时失败后自动重新同步运行状态"`

Expected: FAIL，DOM 仍显示“暂时无法读取运行状态”。

- [ ] **Step 3: Write minimal implementation**

在 `src/main.ts` 增加固定次数和固定短间隔的泛型重试函数：

```ts
async function retryStartupSync<T>(operation: () => Promise<T>): Promise<T> {
  let lastError: unknown;
  for (let attempt = 0; attempt < 3; attempt += 1) {
    try {
      return await operation();
    } catch (error) {
      lastError = error;
      if (attempt < 2) await new Promise((resolve) => window.setTimeout(resolve, 100));
    }
  }
  throw lastError;
}
```

三个初始化任务通过该边界完成初始同步；运行时监听若已收到事件则保持监听并结束同步，未收到事件才使用快照。失败正文不得进入 DOM。

- [ ] **Step 4: Run test to verify it passes**

Run: `pnpm vitest run src/main.test.ts`

Expected: PASS，既有事件排序、revision 和脱敏测试全部通过。

### Task 2: 历史失败候选不阻断有效 deployment

**Files:**
- Modify: `src-tauri/src/update_ui.rs:1203-1293`
- Test: `src-tauri/src/update_ui.rs:2390-2424`

**Interfaces:**
- Consumes: `load_single_confirmed_pending(&AppPaths)` 和 `InstallStateStore::load()`。
- Produces: `pending_bootstrap_plan(result, install_state) -> PendingBootstrapPlan`，明确区分启动当前 deployment、处理新候选和真正恢复失败。

- [ ] **Step 1: Write the failing test**

增加纯决策测试：输入 `Err("activation_retry_available")` 且当前 deployment 有效，期望计划为 `StartActiveWithRetryNotice`；输入同一错误但 deployment 损坏，期望 `RecoveryRequired`。该测试防止再次用 `?` 把历史失败候选直接传播为冷启动终止。

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test pending_retry_receipt_still_starts_authoritative_deployment --manifest-path src-tauri/Cargo.toml -- --nocapture`

Expected: FAIL，因为现有代码没有该决策分支，`activation_retry_available` 会直接返回。

- [ ] **Step 3: Write minimal implementation**

新增私有枚举与纯决策函数；`cold_bootstrap_inner` 执行 `StartActiveWithRetryNotice` 时调用 `start_active_runtime()`，随后发布 `RecoveryRequired`/`activation_retry_available` 提示并返回 `Ok(())`。`InstallStateError::NotInstalled` 保持本地页，其他错误继续返回 `activation_recovery_required`。

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test pending_retry_receipt --manifest-path src-tauri/Cargo.toml -- --nocapture`

Expected: PASS，旧失败候选仍需显式重试，但不阻断当前 deployment。

### Task 3: 修正桌面更新初始语义并完成回归验证

**Files:**
- Modify: `src/desktop-update.ts:57-71`
- Test: `src/desktop-update.test.ts`

**Interfaces:**
- Consumes: `desktopUpdatePresentation(state: DesktopUpdateState)`。
- Produces: `unavailable` 的中性“尚未检查”呈现。

- [ ] **Step 1: Write the failing test**

增加测试，传入 `{ revision: 0, phase: "unavailable" }`，断言 heading 为“尚未检查客户端更新”，正文不包含“通道尚未配置”。该测试防止把前端初始状态误报为构建配置故障。

- [ ] **Step 2: Run test to verify it fails**

Run: `pnpm vitest run src/desktop-update.test.ts -t "未检查状态不误报更新通道缺失"`

Expected: FAIL，当前 heading 是“客户端更新通道尚未配置”。

- [ ] **Step 3: Write minimal implementation**

只修改 `unavailable` 分支的 heading/body，保留“检查客户端更新”动作和其余状态分支。

- [ ] **Step 4: Run test to verify it passes**

Run: `pnpm vitest run src/desktop-update.test.ts`

Expected: PASS。

- [ ] **Step 5: Run complete verification**

Run: `pnpm test`

Run: `cargo test --manifest-path src-tauri/Cargo.toml --locked`

Run: `pnpm build`

Expected: 全部命令退出码为 0，无新增警告或失败。
