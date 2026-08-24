# Windows RC 验收与证据采集设计

## 1. 背景与目标

DSH Desktop 已具备 runtime 签名校验、事务激活与回滚、桌面自更新、沉浸式皮肤和
较完整的自动化安全门禁，但 `docs/development.md` 中仍有离线重启、托盘生命周期、
真实回滚、皮肤视觉和完整进程组性能等 Windows RC 场景处于“待实测”或“部分通过”。

本设计新增一个独立的 RC 验收与证据采集流程，使每轮 Windows 发布都能：

- 在全新的审计目录中记录安装包、环境、进程和性能证据；
- 自动验证适合稳定脚本化的只读条件；
- 为必须依赖真实 GUI 的场景生成结构化人工检查清单；
- 明确区分 `passed`、`failed` 和 `not_run`，禁止把未执行项写成通过；
- 保持用户日常 DSH 数据、已安装 runtime 和系统网络配置不变。

该流程用于发现 P0 发布阻塞问题。验收失败后的产品修复不属于本设计；每个真实缺陷应
单独走 TDD 修复和回归验证。

## 2. 范围

### 2.1 本轮实现范围

1. RC 安装包的存在性、文件大小、SHA-256 和 Authenticode 状态采集。
2. Git commit、三处桌面版本、Windows 版本、CPU、内存和 WebView2 版本采集。
3. 对调用者明确指定的 DSH Desktop PID 进行只读进程树快照。
4. 在固定采样窗口内统计桌面进程及其后代进程的 CPU、Working Set 和 Private Bytes。
5. 生成脱敏 JSON 证据和 Markdown 验收报告。
6. 为离线重启、两类通知、busy 拒绝、托盘重启、真实回滚、完整退出、皮肤视觉、
   缩放和滚动响应生成默认 `not_run` 的人工检查项。
7. 为脚本的输入边界、状态语义、报告内容和错误行为增加 PowerShell 测试。
8. 在开发文档中说明标准执行顺序、隔离要求和判定规则。

### 2.2 不在本轮范围

- 自动安装或卸载 NSIS；
- 自动关闭、终止或重启真实 DSH Desktop/Node/WebView2 进程；
- 自动断开网络、修改代理、防火墙、注册表或系统电源设置；
- 读取、复制或修改 `%APPDATA%/DSH Desktop`、`%LOCALAPPDATA%/DSH Desktop Data`、
  `~/.dsh` 或用户聊天内容；
- 创建、导入或上传 Authenticode 证书和私钥；
- 代替 Windows 10/11、100%/125% 缩放下的真实人工验收；
- 自动删除审计目录、历史 runtime、快照、皮肤或用户数据。

## 3. 方案选择

采用“稳定自动化 + 少量真实人工验收”的混合方案。

纯人工方案无法保证不同 RC 的测量口径一致；全 GUI 自动化又容易受托盘布局、系统通知、
缩放和 WebView2 时序影响，并可能为了稳定测试而引入过宽的系统控制权限。混合方案仅把
文件元数据、环境信息、进程树和数值采样交给脚本，把视觉与系统交互保留为明确的人工项。

## 4. 文件与职责

### `scripts/rc-acceptance.ps1`

唯一的 RC 验收入口，负责参数校验、创建全新审计目录、调用采集模块、写入 JSON 与
Markdown。脚本只接受调用者提供的安装包路径和可选 PID，不自行扫描用户数据目录或猜测
目标进程。

### `scripts/rc-acceptance/measurement.ps1`

提供纯采集函数：版本一致性、系统/WebView2 信息、安装包摘要与签名、指定 PID 的进程树
快照和固定窗口性能采样。函数返回 PowerShell 对象，不直接写报告，不隐式修改全局状态。

### `scripts/rc-acceptance/report.ps1`

把固定 schema 的采集对象渲染为 UTF-8 JSON 与 Markdown。所有人工检查项初始状态均为
`not_run`；报告不得根据“没有观察到错误”推断为 `passed`。

### `scripts/tests/rc-acceptance.Tests.ps1`

使用仓库文件和受控测试进程验证参数、路径、哈希、状态枚举、进程边界、脱敏输出和报告
生成。测试只在新临时审计目录中写入文件，并保留目录供审计，不执行删除。

### `docs/development.md`

增加 RC 验收命令、人工步骤、证据文件说明，以及 Authenticode/Windows 10 等外部条件的
记录方式。现有历史验收结果不被脚本自动覆盖。

## 5. 命令接口

入口采用 PowerShell 7，并使用显式参数：

```powershell
pwsh -NoProfile -File scripts/rc-acceptance.ps1 `
  -Installer releases/desktop/DSH-Desktop_0.1.13_x64-setup.exe `
  -AuditDirectory C:/rc-audit/dsh-desktop-0.1.13
```

需要采集运行中实例时，调用者必须传入已经人工确认属于本轮测试安装的 PID：

```powershell
pwsh -NoProfile -File scripts/rc-acceptance.ps1 `
  -Installer releases/desktop/DSH-Desktop_0.1.13_x64-setup.exe `
  -AuditDirectory C:/rc-audit/dsh-desktop-0.1.13-running `
  -DesktopProcessId 1234 `
  -ObservationSeconds 60
```

约束如下：

- `AuditDirectory` 必须是尚不存在的新目录；拒绝工作区根、用户目录根、盘符根和符号链接/
  reparse 目标。
- `Installer` 必须是存在的普通 `.exe` 文件；只读打开后记录长度和 SHA-256。
- `DesktopProcessId` 省略时不进行进程与性能采样，对应字段为 `not_run`。
- `ObservationSeconds` 默认 60 秒，允许 5 到 300 秒，防止零时长或无界采样。
- 入口不接受 DSH_HOME、runtime 根目录或用户数据目录参数。

## 6. 数据与状态模型

JSON 顶层固定为：

```json
{
  "schema_version": 1,
  "generated_at_utc": "2026-08-24T00:00:00Z",
  "result": "not_run",
  "build": {},
  "environment": {},
  "installer": {},
  "process_observation": {},
  "checks": []
}
```

状态只允许：

- `passed`：对应检查已经执行，且存在可复核证据；
- `failed`：检查已执行，但不满足验收条件；
- `not_run`：未执行、缺少外部条件或本轮不适用。

顶层 `result` 的收敛规则：存在任一 `failed` 时为 `failed`；否则存在任一 `not_run` 时为
`not_run`；仅当所有必需项均为 `passed` 时才为 `passed`。脚本首次生成的人工检查项全部是
`not_run`，因此单次自动采集不能宣称整个 RC 已通过。

## 7. 进程与性能采集

脚本以调用者提供的桌面 PID 为唯一根节点，通过 `Win32_Process` 的 PID/ParentProcessId
关系建立采样时刻的后代集合。每个进程只记录 PID、父 PID、固定进程名和数值指标，不记录
命令行、环境变量、窗口标题或用户路径。

CPU 使用率按采样前后 `TotalProcessorTime` 差值除以采样秒数和逻辑处理器数计算；已经退出
或采样中新增的后代标记为观测变化，不把缺失值替换成零。内存同时记录：

- 桌面根进程 Working Set / Private Bytes；
- 所有存活后代的 Working Set / Private Bytes 合计；
- 进程名为 `msedgewebview2` 的后代数量及内存合计；
- 进程名为 `node` 的后代数量及内存合计。

脚本不终止任何进程。“完整退出无残留”仍由人工退出后再次运行只读进程核对完成，不能用
测试脚本主动杀进程制造通过结果。

## 8. 安全与脱敏

- JSON 和 Markdown 不写安装包绝对路径，只记录文件名、大小、摘要和签名状态。
- 不记录进程命令行、环境变量、URL、窗口标题、聊天内容或 API Key。
- PowerShell 错误使用固定类别；面向报告的错误不拼接动态异常正文。
- 所有输出只写入本轮新建审计目录；发生失败时保留已有证据，不覆盖其他目录。
- 不使用 `Remove-Item`、`.Delete()` 或任何自动清理逻辑。
- Authenticode 仅记录 `Valid`、`NotSigned`、`HashMismatch`、`NotTrusted`、`UnknownError`
  等固定分类，不导出证书私钥或完整证书链。

## 9. 人工验收流程

自动采集完成后，发布维护者使用专用 RC 测试数据依次验证：

1. 在线首次安装并进入官方 WebUI；
2. 断网后冷启动，继续使用已激活 runtime；
3. 官方新版与兼容新版通知分离；
4. busy/unknown busy 状态拒绝 runtime 切换；
5. 托盘隐藏、恢复、重启和显式退出；
6. 已有旧 active pair 时的失败回滚；
7. PNG/JPEG/WebP、四种填充、九个位置、透明度、保存重启与恢复默认；
8. 不支持 adapter 时撤销皮肤并保留官方功能；
9. 100%/125% 缩放、普通/最大化和 8K 背景滚动；
10. 默认视觉与 8K 皮肤各采样一次进程组性能。

人工结果必须包含可复核的简短证据说明；不能执行的 Windows 10、WebView2 缺失补装或
Authenticode 项保持 `not_run` 并写明外部条件，不得降级为可选通过项。

## 10. 测试策略

实施采用 TDD：

1. 先为状态收敛、目录边界、安装包元数据和脱敏报告编写失败测试；
2. 实现最小的对象采集与报告生成；
3. 使用受控子进程测试 PID 根节点、后代集合和采样中退出；
4. 验证无 PID 时保持 `not_run`，且不会扫描任意同名系统进程；
5. 运行 PowerShell 脚本测试、`git diff --check` 和完整 `pnpm check`；
6. 最后使用真实 RC 安装包执行一次仅采集验收，并保留审计目录。

## 11. 完成标准

- 新入口能在全新审计目录生成 schema 固定的 JSON 和 Markdown；
- 安装包、环境、版本和可选进程采样均有自动测试；
- 未运行的 GUI/外部条件明确保持 `not_run`；
- 报告不包含绝对安装包路径、命令行、用户数据或动态错误正文；
- 脚本不安装、卸载、终止进程、修改网络或删除文件；
- `git diff --check`、脚本测试与 `pnpm check` 全部通过；
- 开发文档给出从自动采集到人工验收的完整可重复步骤。
