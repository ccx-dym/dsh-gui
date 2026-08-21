# DSH Desktop Phase 1 Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 构建可在 Windows 10/11 x64 上运行的 Tauri 2 桌面壳，使用模拟 DSH
服务验证单实例、受管进程、探活、WebView 导航、托盘和退出生命周期。

**Architecture:** 使用无框架 TypeScript/Vite 提供本地启动页，Rust/Tauri 负责
所有系统权限和子进程管理。纯 Rust `RuntimeSupervisor` 通过类型化状态驱动 UI；
主 WebView 在服务就绪后从本地启动页导航到回环 URL。阶段 1 不下载真实 Node/DSH，
而是运行仓库内 Node 模拟服务，阶段 2 只需替换 `RuntimeLaunchSpec` 来源。

**Tech Stack:** Node.js 24 LTS、pnpm 11、TypeScript 7、Vite 8、Vitest 4、
Tauri 2.11、Rust stable MSVC、WebView2、Windows Job Object。

**Spec:** `docs/superpowers/specs/2026-08-21-dsh-desktop-design.md`

## Global Constraints

- 目标平台仅为 Windows 10/11 x64，Rust target 为 `x86_64-pc-windows-msvc`。
- 前端不引入 React/Vue/Svelte；使用 TypeScript、HTML 和 CSS。
- 使用 `pnpm` 并提交 `pnpm-lock.yaml`；使用 Cargo 并提交 `Cargo.lock`。
- DSH 或模拟服务只能绑定 `127.0.0.1`，端口由系统动态选择。
- 主窗口只创建一个长期 WebView2；远程回环页面没有通用 Tauri 权限。
- 公共 Rust 类型和函数有类型签名；关键公共函数使用中文 rustdoc，并以“参数”、
  “返回”和“错误”小节表达项目 Sphinx `:param:`、`:return:`、`:raises:` 的同等信息。
- 外部调用必须有超时和结构化错误；禁止裸捕获、拼接 shell 命令和记录敏感正文。
- 删除文件或用户数据不属于阶段 1；测试临时目录由测试框架自动回收。
- 当前构建机已有 Node.js 24.15.0、pnpm 11.19.0，缺少 Rust/MSVC 工具链；
  执行安装前必须取得用户许可。
- 生产依赖审批清单固定为：`@tauri-apps/api@2.11.1`、`tauri@2.11.5`、
  `tauri-plugin-single-instance@2.4.3`、`serde@1.0.229`、
  `serde_json@1.0.151`、`thiserror@2.0.20`、`windows@0.62.2`，以及构建依赖
  `tauri-build@2.6.3`。
- 开发依赖固定为：`@tauri-apps/cli@2.11.4`、`typescript@7.0.2`、
  `vite@8.2.2`、`vitest@4.1.11`、`jsdom@30.0.1`、`@types/node@24.13.3`。

---

## File Structure

```text
package.json                         # pnpm 脚本和前端依赖
pnpm-lock.yaml                       # 锁定 JavaScript 依赖
tsconfig.json                        # 严格 TypeScript 配置
vite.config.ts                       # 本地页面构建
vitest.config.ts                     # jsdom 单元测试
index.html                           # 启动页入口
public/icon-128.png                  # 启动页图标
src/main.ts                          # DOM 装配和 Tauri 事件订阅
src/app-state.ts                     # 前端生命周期状态归约器
src/app-state.test.ts                # 前端状态测试
src/styles.css                       # 启动、错误和重试状态样式
src-tauri/Cargo.toml                 # Rust 生产依赖及 Windows features
src-tauri/Cargo.lock                 # 锁定 Rust 依赖
src-tauri/build.rs                   # Tauri build hook
src-tauri/tauri.conf.json            # 单窗口、图标和 bundle 配置
src-tauri/capabilities/default.json  # 仅本地启动页的最小权限
src-tauri/icons/*                    # 从 assets/icons 复制的 Tauri 图标
src-tauri/src/main.rs                # Windows GUI 入口
src-tauri/src/lib.rs                 # Tauri builder 和模块装配
src-tauri/src/domain.rs              # AppPhase、RuntimeStatus、RuntimeEvent
src-tauri/src/paths.rs               # 预定义目录计算与创建
src-tauri/src/runtime/mod.rs         # RuntimeSupervisor 公共接口
src-tauri/src/runtime/command.rs     # 固定参数 RuntimeLaunchSpec
src-tauri/src/runtime/health.rs      # 有超时的回环 HTTP 探活
src-tauri/src/runtime/process.rs     # Windows Job Object 受管子进程
src-tauri/src/app_controller.rs      # 状态机、事件发送和窗口导航意图
src-tauri/src/tray.rs                # 托盘菜单与关闭窗口策略
src-tauri/tests/lifecycle.rs         # Rust 生命周期集成测试
tests/fixtures/mock-dsh.mjs          # 仅绑定回环接口的模拟 DSH 服务
scripts/smoke-desktop.ps1            # Windows 手工烟雾测试步骤
docs/development.md                  # 工具链、命令和阶段 1 限制
```

## Task 1: 建立可测试的 Tauri 2 最小项目

**Files:**

- Create: `package.json`
- Create: `tsconfig.json`
- Create: `vite.config.ts`
- Create: `vitest.config.ts`
- Create: `index.html`
- Create: `public/icon-128.png`
- Create: `src/main.ts`
- Create: `src/styles.css`
- Create: `src-tauri/Cargo.toml`
- Create: `src-tauri/build.rs`
- Create: `src-tauri/tauri.conf.json`
- Create: `src-tauri/capabilities/default.json`
- Create: `src-tauri/src/main.rs`
- Create: `src-tauri/src/lib.rs`
- Copy: `assets/icons/dsh-desktop.ico` -> `src-tauri/icons/icon.ico`
- Copy: `assets/icons/icon-32.png` -> `src-tauri/icons/32x32.png`
- Copy: `assets/icons/icon-128.png` -> `src-tauri/icons/128x128.png`
- Copy: `assets/icons/icon-256.png` -> `src-tauri/icons/128x128@2x.png`
- Copy: `assets/icons/icon-128.png` -> `public/icon-128.png`

**Interfaces:**

- Consumes: 已批准的生产/开发依赖清单和现有图标资产。
- Produces: `pnpm test`、`pnpm build`、`pnpm tauri dev`、`cargo test`
  四个稳定命令，以及标签为 `main` 的单一 WebView 窗口。

- [ ] **Step 1: 执行工具链只读检查并记录缺口**

Run:

```powershell
node --version
pnpm --version
rustc --version
cargo --version
Get-Command cl.exe -ErrorAction SilentlyContinue
```

Expected: Node 输出 `v24.15.0`，pnpm 输出 `11.19.0`；当前环境的 Rust/Cargo
检查失败，从而证明安装前置条件尚未满足。

- [ ] **Step 2: 在用户批准后安装 Rust stable MSVC 与 C++ Build Tools**

Run only after approval:

```powershell
winget install --id Rustlang.Rustup --exact
winget install --id Microsoft.VisualStudio.2022.BuildTools --exact --override "--wait --passive --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
rustup default stable-msvc
rustup target add x86_64-pc-windows-msvc
```

Expected: `rustc --version`、`cargo --version` 和 `rustup show active-toolchain`
均成功，active target 包含 `x86_64-pc-windows-msvc`。

- [ ] **Step 3: 写入最小项目清单与失败的启动页测试入口**

`package.json` 必须包含：

```json
{
  "name": "dsh-desktop",
  "version": "0.1.0",
  "private": true,
  "type": "module",
  "packageManager": "pnpm@11.19.0",
  "scripts": {
    "dev": "vite",
    "build": "tsc --noEmit && vite build",
    "test": "vitest run",
    "test:watch": "vitest",
    "tauri": "tauri"
  },
  "dependencies": {
    "@tauri-apps/api": "2.11.1"
  },
  "devDependencies": {
    "@tauri-apps/cli": "2.11.4",
    "@types/node": "24.13.3",
    "jsdom": "30.0.1",
    "typescript": "7.0.2",
    "vite": "8.2.2",
    "vitest": "4.1.11"
  }
}
```

`vite.config.ts` 固定开发地址，避免监听局域网：

```typescript
import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
  },
});
```

`vitest.config.ts` 与 `tsconfig.json` 使用严格类型和 jsdom：

```typescript
// vitest.config.ts
import { defineConfig } from "vitest/config";

export default defineConfig({
  test: { environment: "jsdom", restoreMocks: true },
});
```

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "strict": true,
    "noUncheckedIndexedAccess": true,
    "noUnusedLocals": true,
    "noUnusedParameters": true,
    "types": ["vitest/globals"]
  },
  "include": ["src", "vite.config.ts", "vitest.config.ts"]
}
```

`src-tauri/Cargo.toml` 的直接依赖必须为：

```toml
[package]
name = "dsh-desktop"
version = "0.1.0"
edition = "2024"

[lib]
name = "dsh_desktop_lib"
crate-type = ["staticlib", "cdylib", "rlib"]

[build-dependencies]
tauri-build = "=2.6.3"

[dependencies]
serde = { version = "=1.0.229", features = ["derive"] }
serde_json = "=1.0.151"
tauri = { version = "=2.11.5", features = ["tray-icon"] }
tauri-plugin-single-instance = "=2.4.3"
thiserror = "=2.0.20"

[target.'cfg(windows)'.dependencies]
windows = { version = "=0.62.2", features = [
  "Win32_Foundation",
  "Win32_System_JobObjects",
  "Win32_System_Threading"
] }
```

`src-tauri/tauri.conf.json` 必须固定单窗口、严格 CSP 和当前图标：

```json
{
  "$schema": "../node_modules/@tauri-apps/cli/config.schema.json",
  "productName": "DSH Desktop",
  "version": "0.1.0",
  "identifier": "com.community.dsh-desktop",
  "build": {
    "beforeDevCommand": "pnpm dev",
    "devUrl": "http://127.0.0.1:1420",
    "beforeBuildCommand": "pnpm build",
    "frontendDist": "../dist"
  },
  "app": {
    "windows": [
      {
        "label": "main",
        "title": "DSH Desktop",
        "width": 1180,
        "height": 780,
        "minWidth": 880,
        "minHeight": 600,
        "resizable": true
      }
    ],
    "security": {
      "capabilities": ["local-main"],
      "csp": "default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; connect-src ipc: http://ipc.localhost"
    }
  },
  "bundle": {
    "active": true,
    "targets": ["nsis"],
    "icon": [
      "icons/32x32.png",
      "icons/128x128.png",
      "icons/128x128@2x.png",
      "icons/icon.ico"
    ]
  }
}
```

`src-tauri/build.rs` 只运行官方构建 hook：

```rust
fn main() {
    tauri_build::build();
}
```

`src-tauri/capabilities/default.json` 只允许本地窗口基础权限：

```json
{
  "$schema": "../gen/schemas/desktop-schema.json",
  "identifier": "local-main",
  "description": "本地启动页的最小窗口权限",
  "local": true,
  "windows": ["main"],
  "permissions": ["core:default"]
}
```

- [ ] **Step 4: 安装并锁定依赖**

Run:

```powershell
pnpm install --frozen-lockfile=false
Set-Location src-tauri
cargo generate-lockfile
Set-Location ..
```

Expected: 生成 `pnpm-lock.yaml` 和 `src-tauri/Cargo.lock`，命令退出码为 0。

- [ ] **Step 5: 实现只显示“正在启动 DSH”的最小窗口**

`src-tauri/src/lib.rs`：

```rust
pub fn run() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("启动 DSH Desktop 失败");
}
```

`src-tauri/src/main.rs`：

```rust
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    dsh_desktop_lib::run();
}
```

`src/main.ts`：

```typescript
const root = document.querySelector<HTMLElement>("#app");

if (root === null) {
  throw new Error("缺少 #app 根节点");
}

root.innerHTML = `
  <main class="boot" aria-live="polite">
    <img src="/icon-128.png" alt="" width="96" height="96" />
    <h1>DSH Desktop</h1>
    <p>正在启动 DSH…</p>
  </main>
`;
```

- [ ] **Step 6: 运行基础构建验证**

Run:

```powershell
pnpm build
pnpm test -- --passWithNoTests
Set-Location src-tauri
cargo test
Set-Location ..
```

Expected: TypeScript/Vite 构建和 Rust 测试均退出 0。

- [ ] **Step 7: 提交最小项目**

```powershell
git add package.json pnpm-lock.yaml tsconfig.json vite.config.ts vitest.config.ts index.html src src-tauri
git commit -m "build: scaffold Tauri desktop shell"
```

## Task 2: 定义跨前后端生命周期契约

**Files:**

- Create: `src/app-state.ts`
- Create: `src/app-state.test.ts`
- Create: `src-tauri/src/domain.rs`
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**

- Consumes: Tauri `main` 窗口。
- Produces: Rust `AppPhase`、`RuntimeStatus`、`RuntimeEvent`；TypeScript
  `RuntimeStatus` 与 `reduceRuntimeEvent(status, event): RuntimeStatus`。

- [ ] **Step 1: 写前端失败测试**

`src/app-state.test.ts`：

```typescript
import { describe, expect, it } from "vitest";
import { initialRuntimeStatus, reduceRuntimeEvent } from "./app-state";

describe("reduceRuntimeEvent", () => {
  it("从启动中进入就绪态并保留 URL", () => {
    const result = reduceRuntimeEvent(initialRuntimeStatus, {
      type: "ready",
      url: "http://127.0.0.1:43127",
      elapsedMs: 820,
    });

    expect(result).toEqual({
      phase: "ready",
      url: "http://127.0.0.1:43127",
      message: "DSH 已就绪",
      elapsedMs: 820,
    });
  });

  it("失败事件不保留旧 URL", () => {
    const result = reduceRuntimeEvent(
      { ...initialRuntimeStatus, phase: "ready", url: "http://127.0.0.1:1" },
      { type: "failed", code: "health_timeout", message: "启动超时" },
    );

    expect(result.url).toBeUndefined();
    expect(result.phase).toBe("failed");
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `pnpm test -- src/app-state.test.ts`

Expected: FAIL，提示无法解析 `./app-state`。

- [ ] **Step 3: 实现 TypeScript 契约和归约器**

`src/app-state.ts`：

```typescript
export type AppPhase = "idle" | "starting" | "ready" | "failed" | "stopping";

export interface RuntimeStatus {
  phase: AppPhase;
  message: string;
  url?: string;
  elapsedMs?: number;
  errorCode?: string;
}

export type RuntimeEvent =
  | { type: "starting"; message: string }
  | { type: "ready"; url: string; elapsedMs: number }
  | { type: "failed"; code: string; message: string }
  | { type: "stopping"; message: string };

export const initialRuntimeStatus: RuntimeStatus = {
  phase: "idle",
  message: "等待启动",
};

export function reduceRuntimeEvent(
  _current: RuntimeStatus,
  event: RuntimeEvent,
): RuntimeStatus {
  switch (event.type) {
    case "starting":
      return { phase: "starting", message: event.message };
    case "ready":
      return {
        phase: "ready",
        url: event.url,
        message: "DSH 已就绪",
        elapsedMs: event.elapsedMs,
      };
    case "failed":
      return { phase: "failed", message: event.message, errorCode: event.code };
    case "stopping":
      return { phase: "stopping", message: event.message };
  }
}
```

- [ ] **Step 4: 写 Rust 序列化失败测试**

`src-tauri/src/domain.rs`：先只加入测试模块，引用尚不存在的类型：

```rust
#[cfg(test)]
mod tests {
    use super::{AppPhase, RuntimeEvent, RuntimeStatus};

    #[test]
    fn ready_event_uses_frontend_field_names() {
        let event = RuntimeEvent::Ready {
            url: "http://127.0.0.1:43127".to_owned(),
            elapsed_ms: 820,
        };
        let value = serde_json::to_value(event).expect("事件必须可序列化");
        assert_eq!(value["type"], "ready");
        assert_eq!(value["elapsedMs"], 820);
    }

    #[test]
    fn default_status_is_idle() {
        assert_eq!(RuntimeStatus::default().phase, AppPhase::Idle);
    }
}
```

- [ ] **Step 5: 运行 Rust 测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml domain::tests`

Expected: FAIL，提示 `AppPhase`、`RuntimeEvent`、`RuntimeStatus` 未定义。

- [ ] **Step 6: 实现 Rust 类型**

```rust
use serde::Serialize;

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppPhase {
    #[default]
    Idle,
    Starting,
    Ready,
    Failed,
    Stopping,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub phase: AppPhase,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum RuntimeEvent {
    Starting { message: String },
    Ready { url: String, elapsed_ms: u64 },
    Failed { code: String, message: String },
    Stopping { message: String },
}
```

- [ ] **Step 7: 运行双端测试并提交**

Run:

```powershell
pnpm test -- src/app-state.test.ts
cargo test --manifest-path src-tauri/Cargo.toml domain::tests
```

Expected: 两组测试 PASS。

```powershell
git add src/app-state.ts src/app-state.test.ts src-tauri/src/domain.rs src-tauri/src/lib.rs
git commit -m "feat: define runtime lifecycle contracts"
```

## Task 3: 实现预定义目录策略

**Files:**

- Create: `src-tauri/src/paths.rs`
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**

- Consumes: `tauri::AppHandle::path()` 返回的系统目录。
- Produces: `AppPaths::resolve(app: &AppHandle) -> Result<AppPaths, PathError>` 和
  `AppPaths::ensure_exists(&self) -> Result<(), PathError>`。

- [ ] **Step 1: 写路径策略失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::AppPaths;
    use std::path::Path;

    #[test]
    fn fixed_roots_keep_user_data_separate_from_runtime_cache() {
        let paths = AppPaths::from_roots(
            Path::new(r"C:\Users\demo\AppData\Roaming"),
            Path::new(r"C:\Users\demo\AppData\Local"),
        );

        assert!(paths.dsh_home.ends_with(r"DSH Desktop\dsh-home"));
        assert!(paths.settings.ends_with(r"DSH Desktop\settings"));
        assert!(paths.runtimes.ends_with(r"DSH Desktop\runtimes"));
        assert!(paths.webview_data.ends_with(r"DSH Desktop\webview-data"));
        assert!(!paths.dsh_home.starts_with(&paths.runtimes));
    }
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml paths::tests`

Expected: FAIL，提示 `AppPaths` 未定义。

- [ ] **Step 3: 实现路径值对象与显式目录创建**

```rust
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PathError {
    #[error("无法解析系统目录: {0}")]
    Resolve(String),
    #[error("无法创建目录 {path}: {source}")]
    Create {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    pub dsh_home: PathBuf,
    pub settings: PathBuf,
    pub logs: PathBuf,
    pub runtimes: PathBuf,
    pub skins: PathBuf,
    pub updates: PathBuf,
    pub webview_data: PathBuf,
}

impl AppPaths {
    pub fn from_roots(roaming: &Path, local: &Path) -> Self {
        let roaming_root = roaming.join("DSH Desktop");
        let local_root = local.join("DSH Desktop");
        Self {
            dsh_home: roaming_root.join("dsh-home"),
            settings: roaming_root.join("settings"),
            logs: roaming_root.join("logs"),
            runtimes: local_root.join("runtimes"),
            skins: local_root.join("skins"),
            updates: local_root.join("updates"),
            webview_data: local_root.join("webview-data"),
        }
    }

    pub fn resolve(app: &AppHandle) -> Result<Self, PathError> {
        let roaming = app.path().config_dir().map_err(|error| {
            PathError::Resolve(error.to_string())
        })?;
        let local = app.path().local_data_dir().map_err(|error| {
            PathError::Resolve(error.to_string())
        })?;
        Ok(Self::from_roots(&roaming, &local))
    }

    pub fn ensure_exists(&self) -> Result<(), PathError> {
        for path in [
            &self.dsh_home,
            &self.settings,
            &self.logs,
            &self.runtimes,
            &self.skins,
            &self.updates,
            &self.webview_data,
        ] {
            fs::create_dir_all(path).map_err(|source| PathError::Create {
                path: path.clone(),
                source,
            })?;
        }
        Ok(())
    }
}
```

- [ ] **Step 4: 验证测试与 Windows 非 ASCII 临时路径**

补充测试，用 `std::env::temp_dir().join("鲸鱼 用户")` 作为 root，仅调用
`from_roots` 并断言路径保持 Unicode，不执行删除。

Run: `cargo test --manifest-path src-tauri/Cargo.toml paths::tests`

Expected: PASS。

- [ ] **Step 5: 提交目录策略**

```powershell
git add src-tauri/src/paths.rs src-tauri/src/lib.rs
git commit -m "feat: add isolated application paths"
```

## Task 4: 实现固定命令与回环探活

**Files:**

- Create: `src-tauri/src/runtime/mod.rs`
- Create: `src-tauri/src/runtime/command.rs`
- Create: `src-tauri/src/runtime/health.rs`
- Modify: `src-tauri/src/lib.rs`
- Create: `tests/fixtures/mock-dsh.mjs`

**Interfaces:**

- Consumes: `AppPaths::dsh_home`。
- Produces: `RuntimeLaunchSpec::mock(node, script, dsh_home, port)`、
  `reserve_loopback_port() -> Result<u16, RuntimeError>`、
  `HealthProbe::wait_until_ready(port, timeout) -> Result<String, RuntimeError>`。

- [ ] **Step 1: 写命令参数失败测试**

```rust
#[test]
fn mock_spec_never_uses_shell_command_string() {
    let spec = RuntimeLaunchSpec::mock(
        PathBuf::from(r"C:\Program Files\nodejs\node.exe"),
        PathBuf::from(r"D:\dsh desktop\tests\fixtures\mock-dsh.mjs"),
        PathBuf::from(r"C:\Users\demo\AppData\Roaming\DSH Desktop\dsh-home"),
        43127,
    );

    assert_eq!(spec.program, PathBuf::from(r"C:\Program Files\nodejs\node.exe"));
    assert_eq!(spec.args, vec![
        r"D:\dsh desktop\tests\fixtures\mock-dsh.mjs",
        "--host",
        "127.0.0.1",
        "--port",
        "43127",
    ]);
    assert_eq!(spec.env.get("DSH_HOME").unwrap(), r"C:\Users\demo\AppData\Roaming\DSH Desktop\dsh-home");
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml runtime::command::tests`

Expected: FAIL，提示 `RuntimeLaunchSpec` 未定义。

- [ ] **Step 3: 实现命令值对象和动态端口**

```rust
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("运行时 I/O 失败: {0}")]
    Io(#[from] std::io::Error),
    #[error("DSH 在 {timeout_ms} ms 内未通过端口 {port} 探活")]
    HealthTimeout { port: u16, timeout_ms: u64 },
    #[error("运行时已经启动")]
    AlreadyRunning,
    #[error("Windows 进程管理失败: {0}")]
    Process(String),
    #[error("缺少 main 窗口")]
    MainWindowMissing,
    #[error("无效的本地运行时 URL: {0}")]
    InvalidUrl(String),
    #[error("Tauri 操作失败: {0}")]
    Tauri(String),
    #[error("正式构建禁止启动模拟运行时")]
    MockRuntimeDisabled,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeLaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

pub fn reserve_loopback_port() -> Result<u16, RuntimeError> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}
```

`RuntimeLaunchSpec::mock` 必须逐项构造参数，不调用 `cmd.exe /C`，并只设置
`DSH_HOME`、`NO_COLOR=1` 和测试所需变量。

- [ ] **Step 4: 写探活失败测试**

创建本地 `TcpListener`，在线程中返回：

```text
HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK
```

测试 `wait_until_ready` 返回严格的 `http://127.0.0.1:<port>`；另一个未监听
端口在 150 ms 后返回 `RuntimeError::HealthTimeout`。

- [ ] **Step 5: 实现有截止时间的 HTTP 探活**

`health.rs` 使用 `TcpStream::connect_timeout`、100 ms 单次读写超时、固定
`GET / HTTP/1.1` 请求和 50 ms 重试间隔。只接受状态行 `HTTP/1.1 200` 或
`HTTP/1.0 200`，不得依赖外部 HTTP crate。

```rust
pub trait ReadyProbe: Send + Sync {
    fn wait_until_ready(
        &self,
        port: u16,
        timeout: Duration,
    ) -> Result<String, RuntimeError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HealthProbe;

impl ReadyProbe for HealthProbe {
    fn wait_until_ready(
        &self,
        port: u16,
        timeout: Duration,
    ) -> Result<String, RuntimeError> {
        let deadline = Instant::now() + timeout;
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(RuntimeError::HealthTimeout {
                    port,
                    timeout_ms: timeout.as_millis() as u64,
                });
            }

            let attempt_timeout = (deadline - now).min(Duration::from_millis(100));
            if let Ok(mut stream) = TcpStream::connect_timeout(&address, attempt_timeout) {
                stream.set_read_timeout(Some(attempt_timeout))?;
                stream.set_write_timeout(Some(attempt_timeout))?;
                stream.write_all(
                    b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
                )?;
                let mut response = [0_u8; 64];
                let read = stream.read(&mut response)?;
                if response[..read].starts_with(b"HTTP/1.1 200")
                    || response[..read].starts_with(b"HTTP/1.0 200")
                {
                    return Ok(format!("http://127.0.0.1:{port}"));
                }
            }

            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50)),
            );
        }
    }
}
```

- [ ] **Step 6: 实现模拟 DSH 服务**

`tests/fixtures/mock-dsh.mjs`：

```javascript
import http from "node:http";
import process from "node:process";

const args = new Map();
for (let index = 2; index < process.argv.length; index += 2) {
  args.set(process.argv[index], process.argv[index + 1]);
}

const host = args.get("--host");
const port = Number(args.get("--port"));
if (host !== "127.0.0.1" || !Number.isInteger(port)) {
  throw new Error("mock-dsh 只允许显式回环 host 和整数 port");
}

const server = http.createServer((_request, response) => {
  response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
  response.end("<!doctype html><title>Mock DSH</title><h1>Mock DSH Ready</h1>");
});
server.listen(port, host, () => console.log(`READY http://${host}:${port}`));
for (const signal of ["SIGINT", "SIGTERM"]) {
  process.on(signal, () => server.close(() => process.exit(0)));
}
```

- [ ] **Step 7: 运行测试并提交**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml runtime::command::tests
cargo test --manifest-path src-tauri/Cargo.toml runtime::health::tests
node tests/fixtures/mock-dsh.mjs --host 127.0.0.1 --port 43127
```

Expected: Rust 测试 PASS；最后一条命令输出 `READY`，手工按 Ctrl+C 后退出 0。

```powershell
git add src-tauri/src/runtime tests/fixtures/mock-dsh.mjs src-tauri/src/lib.rs
git commit -m "feat: add loopback runtime launch contract"
```

## Task 5: 用 Windows Job Object 管理子进程树

**Files:**

- Create: `src-tauri/src/runtime/process.rs`
- Modify: `src-tauri/src/runtime/mod.rs`
- Create: `src-tauri/tests/lifecycle.rs`

**Interfaces:**

- Consumes: `RuntimeLaunchSpec`。
- Produces: `ManagedChild::spawn(spec) -> Result<ManagedChild, RuntimeError>`、
  `ManagedChild::id() -> u32`、`ManagedChild::try_wait()`、
  `ManagedChild::stop(grace: Duration) -> Result<StopOutcome, RuntimeError>`，以及
  `StopOutcome::{Exited, Terminated}`。

- [ ] **Step 1: 写进程树回收失败测试**

Windows-only 集成测试启动 mock 服务，取得 PID 后丢弃 `ManagedChild`，最多等待
2 秒并用 `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, ...)` 检查 PID 不再存在：

```rust
#[test]
fn dropping_managed_child_reclaims_the_process() {
    let spec = fixture_spec(free_port());
    let child = ManagedChild::spawn(&spec).expect("模拟服务应启动");
    let pid = child.id();
    drop(child);
    assert!(wait_until_process_exits(pid, Duration::from_secs(2)));
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml --test lifecycle`

Expected: FAIL，提示 `ManagedChild` 未定义。

- [ ] **Step 3: 实现 Job Object 安全句柄**

`process.rs` 必须：

1. 调用 `CreateJobObjectW(None, None)`；
2. 配置 `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` 的
   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`；
3. 使用 `std::process::Command` 的逐项参数、`stdin(null)`、`stdout(piped)`、
   `stderr(piped)` 启动子进程；
4. 用 `AssignProcessToJobObject` 绑定子进程；
5. 任一步失败时结束已启动进程并返回带 Windows error code 的 `RuntimeError`；
6. `Drop` 先关闭 Job handle，让 Windows 回收完整进程树。

安全句柄使用 `std::os::windows::io::OwnedHandle`，禁止手写可复制的裸 `HANDLE`
所有权类型。

- [ ] **Step 4: 实现限时正常停止**

`stop(grace)` 先调用 `child.kill()` 之外的正常信号 seam：阶段 1 mock 服务在
Windows 上通过关闭 Job 前先等待 `try_wait`；尚无可移植 SIGTERM 时，等待
`grace` 后调用 `TerminateJobObject(job, 1)`。方法返回 `StopOutcome::Exited` 或
`StopOutcome::Terminated`，错误保留 OS code。

- [ ] **Step 5: 运行进程集成测试**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test lifecycle -- --nocapture
```

Expected: 启动、PID 存活、Drop 后 PID 消失、限时停止四项断言 PASS。

- [ ] **Step 6: 提交进程管理**

```powershell
git add src-tauri/src/runtime/process.rs src-tauri/src/runtime/mod.rs src-tauri/tests/lifecycle.rs
git commit -m "feat: supervise runtime with Windows job object"
```

## Task 6: 实现 RuntimeSupervisor 与 AppController

**Files:**

- Modify: `src-tauri/src/runtime/mod.rs`
- Create: `src-tauri/src/app_controller.rs`
- Modify: `src-tauri/src/lib.rs`

**Interfaces:**

- Consumes: `RuntimeLaunchSpec`、`ManagedChild`、`HealthProbe`、`RuntimeEvent`。
- Produces: `RuntimeSupervisor::start(spec, timeout, sink)`、
  `RuntimeSupervisor::stop(grace)`、`AppController::start_mock_runtime()`、
  Tauri command `get_runtime_status() -> RuntimeStatus` 和 `retry_runtime()`。

- [ ] **Step 1: 写状态顺序失败测试**

使用 `VecEventSink` 和可注入的 fake process/probe：

```rust
#[test]
fn successful_start_emits_starting_then_ready() {
    let sink = VecEventSink::default();
    let supervisor = RuntimeSupervisor::for_test(
        FakeProcess::running(),
        FakeProbe::ready("http://127.0.0.1:43127"),
    );

    supervisor.start(test_spec(), Duration::from_secs(1), sink.clone())
        .expect("启动应成功");

    assert_eq!(sink.types(), vec!["starting", "ready"]);
}

#[test]
fn failed_probe_stops_child_and_emits_failure_once() {
    let process = FakeProcess::running();
    let sink = VecEventSink::default();
    let supervisor = RuntimeSupervisor::for_test(
        process.clone(),
        FakeProbe::timeout(),
    );

    assert!(supervisor.start(test_spec(), Duration::from_millis(1), sink.clone()).is_err());
    assert_eq!(process.stop_calls(), 1);
    assert_eq!(sink.types(), vec!["starting", "failed"]);
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml runtime::tests`

Expected: FAIL，提示 supervisor 和 fake seam 未定义。

- [ ] **Step 3: 实现 supervisor 单一职责状态机**

定义：

```rust
pub trait RuntimeProcess: Send {
    fn id(&self) -> u32;
    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, RuntimeError>;
    fn stop(&mut self, grace: Duration) -> Result<StopOutcome, RuntimeError>;
}

pub trait ProcessLauncher: Send + Sync {
    fn spawn(&self, spec: &RuntimeLaunchSpec) -> Result<Box<dyn RuntimeProcess>, RuntimeError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsProcessLauncher;

impl ProcessLauncher for WindowsProcessLauncher {
    fn spawn(&self, spec: &RuntimeLaunchSpec) -> Result<Box<dyn RuntimeProcess>, RuntimeError> {
        Ok(Box::new(ManagedChild::spawn(spec)?))
    }
}

pub trait RuntimeEventSink: Send + Sync + 'static {
    fn emit(&self, event: RuntimeEvent) -> Result<(), RuntimeError>;
}

pub struct RuntimeSupervisor {
    state: Mutex<SupervisorState>,
    launcher: Arc<dyn ProcessLauncher>,
    probe: Arc<dyn ReadyProbe>,
}

enum SupervisorState {
    Stopped,
    Starting,
    Running { child: Box<dyn RuntimeProcess>, url: String },
    Failed,
}
```

`start` 在专用后台线程中完成 spawn 和 probe；互斥锁只保护状态转换，不跨探活
睡眠持锁。重复 `start` 返回 `RuntimeError::AlreadyRunning`，失败路径只发一次
`Failed` 并回收 child。

- [ ] **Step 4: 实现 AppController 与 Tauri 事件 sink**

`AppController` 持有 `Arc<RuntimeSupervisor>`、`RwLock<RuntimeStatus>` 和
`AppHandle`。`TauriEventSink` 每次先更新 status，再执行：

```rust
app.emit("runtime-status", &event)?;
if let RuntimeEvent::Ready { url, .. } = &event {
    app.get_webview_window("main")
        .ok_or(RuntimeError::MainWindowMissing)?
        .navigate(url.parse().map_err(RuntimeError::InvalidUrl)?)?;
}
```

失败时导航回保存的本地 `http://tauri.localhost/`，不在远程页面注入 Tauri API。

`start_mock_runtime` 只用于开发构建：从 `DSH_DESKTOP_NODE` 读取显式 Node 路径，
未设置时使用 `node.exe`；脚本路径固定为
`PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/fixtures/mock-dsh.mjs")`。
方法先用 `AppPaths::ensure_exists` 创建目录，再申请回环端口并构造
`RuntimeLaunchSpec::mock`。release 构建若调用该方法必须返回
`RuntimeError::MockRuntimeDisabled`，不能把开发机 Node 当作生产运行时。

- [ ] **Step 5: 注册只读状态与重试命令**

```rust
#[tauri::command]
fn get_runtime_status(controller: tauri::State<'_, AppController>) -> RuntimeStatus {
    controller.status()
}

#[tauri::command]
fn retry_runtime(controller: tauri::State<'_, AppController>) -> Result<(), String> {
    controller.retry().map_err(|error| error.to_string())
}
```

命令只注册到本地 capability；远程 DSH URL 不加入 capability 的 `remote.urls`。
`Builder::setup` 在 `debug_assertions` 下调用 `start_mock_runtime`；正式构建只显示
“尚未安装兼容运行时”，等待阶段 2 提供运行时选择器。

- [ ] **Step 6: 运行状态机和编译测试**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml runtime::tests app_controller::tests
cargo check --manifest-path src-tauri/Cargo.toml
```

Expected: 状态顺序、重复启动、超时停止、状态快照与事件序列测试 PASS；check 退出 0。

- [ ] **Step 7: 提交控制器**

```powershell
git add src-tauri/src/runtime/mod.rs src-tauri/src/app_controller.rs src-tauri/src/lib.rs
git commit -m "feat: coordinate runtime lifecycle"
```

## Task 7: 连接启动页状态和重试体验

**Files:**

- Modify: `src/main.ts`
- Modify: `src/styles.css`
- Modify: `src/app-state.test.ts`
- Create: `src/main.test.ts`

**Interfaces:**

- Consumes: `runtime-status` 事件、`get_runtime_status`、`retry_runtime`。
- Produces: `renderRuntimeStatus(root, status)` 和错误态重试按钮。

- [ ] **Step 1: 写 DOM 失败测试**

```typescript
import { beforeEach, describe, expect, it } from "vitest";
import { renderRuntimeStatus } from "./main";

describe("renderRuntimeStatus", () => {
  beforeEach(() => {
    document.body.innerHTML = '<div id="app"></div>';
  });

  it("失败时显示错误码和重试按钮", () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    renderRuntimeStatus(root, {
      phase: "failed",
      message: "启动超时",
      errorCode: "health_timeout",
    });

    expect(root.textContent).toContain("启动超时");
    expect(root.textContent).toContain("health_timeout");
    expect(root.querySelector<HTMLButtonElement>("[data-action='retry']")).not.toBeNull();
  });
});
```

- [ ] **Step 2: 运行测试确认失败**

Run: `pnpm test -- src/main.test.ts`

Expected: FAIL，提示 `renderRuntimeStatus` 未导出。

- [ ] **Step 3: 实现可访问的状态渲染**

`renderRuntimeStatus` 使用 `textContent` 写入错误内容，不把后台字符串拼成 HTML。
启动态显示图标、spinner 和阶段文字；失败态只显示错误码和重试按钮。阶段 1 不显示
“打开日志”，因为尚未授予文件打开权限。

- [ ] **Step 4: 连接 Tauri invoke/listen**

```typescript
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

let status = await invoke<RuntimeStatus>("get_runtime_status");
renderRuntimeStatus(root, status);

await listen<RuntimeEvent>("runtime-status", ({ payload }) => {
  status = reduceRuntimeEvent(status, payload);
  renderRuntimeStatus(root, status);
});
```

重试按钮使用事件委托调用 `invoke("retry_runtime")`，调用期间禁用按钮；失败文本
仍通过 `textContent` 显示。

- [ ] **Step 5: 运行前端测试与构建**

Run:

```powershell
pnpm test
pnpm build
```

Expected: reducer、DOM、安全文本和重试按钮测试全部 PASS；Vite 构建退出 0。

- [ ] **Step 6: 提交启动体验**

```powershell
git add src/main.ts src/main.test.ts src/app-state.test.ts src/styles.css
git commit -m "feat: render runtime startup states"
```

## Task 8: 实现单实例、托盘和关闭策略

**Files:**

- Create: `src-tauri/src/tray.rs`
- Modify: `src-tauri/src/lib.rs`
- Modify: `src-tauri/src/app_controller.rs`
- Modify: `src-tauri/tests/lifecycle.rs`

**Interfaces:**

- Consumes: `AppController::stop()`、Tauri `main` window。
- Produces: `TrayAction::{Open, Hide, Restart, Exit}`、
  `handle_close_request(window) -> CloseDecision`、单实例聚焦回调。

- [ ] **Step 1: 写纯策略失败测试**

```rust
#[test]
fn window_close_hides_instead_of_exiting() {
    assert_eq!(close_decision(false), CloseDecision::HideToTray);
}

#[test]
fn confirmed_tray_exit_stops_runtime_then_exits() {
    let controller = FakeController::running();
    handle_tray_action(TrayAction::Exit, &controller).unwrap();
    assert_eq!(controller.calls(), vec!["stop", "exit"]);
}
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test --manifest-path src-tauri/Cargo.toml tray::tests`

Expected: FAIL，提示 tray policy 类型未定义。

- [ ] **Step 3: 实现托盘菜单**

菜单 ID 固定为 `open`、`hide`、`restart`、`exit`。左键托盘图标恢复并聚焦窗口；
菜单动作调用类型化 `TrayAction`，不根据显示文字分支。阶段 1 的 restart 先 stop，
完成后重新使用当前 mock spec 启动。

- [ ] **Step 4: 拦截窗口关闭**

监听 `RunEvent::WindowEvent { event: CloseRequested { api, .. } }`：调用
`api.prevent_close()`，再隐藏 `main` 窗口。只有 `AppController` 内部的
`exit_requested` 标志为 true 时允许真正退出。

- [ ] **Step 5: 注册 single-instance 插件**

插件必须在其他插件之前注册：

```rust
.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}))
```

第二实例不得再次启动 mock DSH。

- [ ] **Step 6: 运行 Rust 测试并手工验证**

Run:

```powershell
cargo test --manifest-path src-tauri/Cargo.toml tray::tests --test lifecycle
pnpm tauri dev
```

Manual expected:

1. 启动页出现后导航到 `Mock DSH Ready`；
2. 点击右上角关闭，窗口消失但 mock Node PID 仍存在；
3. 左键托盘图标，原窗口恢复；
4. 再运行一次 `pnpm tauri dev` 不创建第二个服务；
5. 托盘“退出”后窗口与 mock Node PID 都消失。

- [ ] **Step 7: 提交桌面生命周期**

```powershell
git add src-tauri/src/tray.rs src-tauri/src/lib.rs src-tauri/src/app_controller.rs src-tauri/tests/lifecycle.rs
git commit -m "feat: add single-instance tray lifecycle"
```

## Task 9: 完成阶段 1 文档与验证门禁

**Files:**

- Create: `scripts/smoke-desktop.ps1`
- Create: `docs/development.md`
- Modify: `package.json`

**Interfaces:**

- Consumes: 所有阶段 1 命令和行为。
- Produces: `pnpm check` 统一门禁、可复现的 Windows 手工烟雾流程和阶段 2 接口说明。

- [ ] **Step 1: 写统一检查脚本并先运行发现缺口**

在 `package.json` 增加：

```json
"check": "pnpm test && pnpm build && cargo test --manifest-path src-tauri/Cargo.toml && cargo clippy --manifest-path src-tauri/Cargo.toml --all-targets -- -D warnings"
```

Run: `pnpm check`

Expected: 若 clippy、类型或测试存在问题则失败；记录并逐项修复具体诊断，不降低规则。

- [ ] **Step 2: 编写烟雾脚本**

`scripts/smoke-desktop.ps1` 只做只读/启动检查，不删除文件。它必须：

1. 验证 `node`、`pnpm`、`cargo`、WebView2 注册表项；
2. 运行 `pnpm check`；
3. 输出 `pnpm tauri dev` 的手工五步清单；
4. 不使用 `$HOME`、通配删除或跨 PowerShell 版本传递路径。

使用以下脚本主体：

```powershell
$ErrorActionPreference = 'Stop'

foreach ($commandName in @('node', 'pnpm', 'cargo')) {
    if (-not (Get-Command $commandName -ErrorAction SilentlyContinue)) {
        throw "缺少开发命令: $commandName"
    }
}

$webViewClientId = '{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
$webViewKeys = @(
    "HKCU:\Software\Microsoft\EdgeUpdate\Clients\$webViewClientId",
    "HKLM:\Software\WOW6432Node\Microsoft\EdgeUpdate\Clients\$webViewClientId"
)
if (-not ($webViewKeys | Where-Object { Test-Path -LiteralPath $_ })) {
    throw '未检测到 Microsoft Edge WebView2 Runtime'
}

pnpm check
if ($LASTEXITCODE -ne 0) {
    throw 'pnpm check 未通过'
}

Write-Host '手工检查: 启动就绪、关闭到托盘、托盘恢复、单实例、托盘退出回收进程。'
```

- [ ] **Step 3: 编写开发文档**

`docs/development.md` 明确：

- Windows 10/11 x64、Node 24 LTS、pnpm 11、stable-msvc、C++ Build Tools；
- `pnpm install`、`pnpm check`、`pnpm tauri dev`；
- 当前只运行 mock 服务，不是可交付的真实 DSH；
- 阶段 2 接入点是 `RuntimeLaunchSpec`，不得绕过 `RuntimeSupervisor`；
- 日志不得包含 API Key、鉴权头或用户提示正文。

- [ ] **Step 4: 运行完整自动化验证**

Run:

```powershell
pnpm check
git diff --check
git status --short
```

Expected: `pnpm check` 和 `git diff --check` 退出 0；`git status --short`
只显示本任务预期的文档、脚本和 `package.json` 修改。

- [ ] **Step 5: 执行 Windows 手工烟雾测试**

Run: `pwsh -File scripts/smoke-desktop.ps1`

然后运行 `pnpm tauri dev`，逐项记录 Task 8 的五个行为。任何失败都必须保留
日志中的阶段、错误类型、耗时和 PID，但不能记录用户正文。

- [ ] **Step 6: 提交阶段 1 门禁**

```powershell
git add package.json scripts/smoke-desktop.ps1 docs/development.md
git commit -m "docs: add desktop foundation verification"
```

- [ ] **Step 7: 最终验收**

Run:

```powershell
pnpm check
git status --porcelain
git log --oneline -9
```

Expected: 全部门禁退出 0，工作区为空；提交历史包含每个任务的独立提交。阶段 1
交付物可运行 mock DSH，但不得宣称已经支持真实 DSH 下载、更新、皮肤或安装器。
