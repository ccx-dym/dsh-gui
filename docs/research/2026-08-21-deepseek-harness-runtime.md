# DeepSeek Harness Windows 桌面运行时调研

调研日期：2026-08-21（Asia/Shanghai）  
官方仓库：[`deepseek-ai/deepseek-harness`](https://github.com/deepseek-ai/deepseek-harness)  
研究快照：`master` 提交 [`b150a551b8d465e31e418e1b2eaf5e79bbb7d28e`](https://github.com/deepseek-ai/deepseek-harness/tree/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e)；已发布 npm 版本以 `dsh-v0.1.1-rc.1` 标签为准。

> 范围：只使用 DeepSeek Harness 官方仓库、官方 GitHub 发布元数据、npm 与 PyPI 官方包元数据。仓库 README 明确称项目仍处于 developer preview，且会发生破坏兼容的变化，因此本文给出的启动契约应按“兼容适配层”实现，而不是视为长期稳定 API。[来源：根 README](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/README.md#developer-preview)

## 结论摘要

- **Windows 10/11 x64 可走官方 npm Web CLI 路线。** 官方根 README 的公开入口是 `npx @deepseek-ai/dsh web`；发布标签要求 Node `^22.19.0 || >=24.0.0`，官方 CI 使用 Node 24，并有真实 Windows 原生完整门禁。代码还为 Windows 启用 PowerShell/ConPTY 相关实现。[运行说明](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/README.md#run)、[Node engine](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/package.json#L6-L9)、[Windows CI](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/.github/workflows/ci.yml)
- **官方 Python SDK 的 bundled runtime 当前不支持 Windows。** 官方构建矩阵和 PyPI wheel 只有 Linux x64、Linux arm64、macOS arm64；没有 Windows wheel。因此桌面端第一条可落地兼容链应管理 npm 包，而不是依赖 `deepseek-harness-runtime-bin`。[构建矩阵](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/.github/workflows/build-exe-for-python-sdk.yml#L28-L35)、[PyPI runtime 元数据](https://pypi.org/pypi/deepseek-harness-runtime-bin/json)
- **建议把每个 DSH 版本完整安装到独立目录，固定执行其 `lib/bin.js`，不要每次启动都调用 `npx`。** npm 包声明的可执行入口为 `dsh -> lib/bin.js`；版本目录可以原子切换和快速回滚，同时避免启动时联网与全局 PATH 污染。[CLI manifest](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/apps/cli/package.json#L1-L20)
- **用户数据必须与运行时版本目录分离。** 官方使用单一 `$DSH_HOME` 根；默认是 `~/.dsh`，并存放 profile、配置、凭据、会话、附件与其他 durable storage。桌面端应显式设置 `DSH_HOME` 为用户选择的数据目录，在更新时保留它。[home-paths](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/util/home-paths/src/index.ts#L13-L95)
- **更新前必须做数据快照/兼容门禁。** 当前 session format version 是 `0`，源码明确表示不承诺兼容、遇到不同版本直接拒绝且无迁移；凭据文档另有严格版本字段。不能假设“npm 新版本安装成功”就代表旧数据可直接打开。[session version](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/core/session/src/types.ts#L16-L37)、[credentials version](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/credentials/credentials-local/src/index.ts#L160-L220)

## Windows x64 支持现状

### 已确认

1. 官方最终用户 Web UI 命令只要求先安装 Node.js，然后运行：

   ```powershell
   npx @deepseek-ai/dsh web
   ```

   默认服务地址是 `http://127.0.0.1:3080`。[根 README](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/README.md#run)

2. `dsh-v0.1.1-rc.1` 仓库根 manifest 将支持的 Node 范围固定为 `^22.19.0 || >=24.0.0`，官方发布/CI 主版本使用 Node 24。[package.json](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/package.json#L6-L9)、[发布 workflow](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/.github/workflows/release-publish.yml#L12-L18)

3. 官方 PR CI 有两条 Windows 信号：Wine 下的阻塞门禁，以及真实 Windows kernel 下的 `windows-native` 完整门禁；原生门禁在 Windows runner 上执行 `pnpm install --frozen-lockfile` 和 `pnpm run check:ci:windows-complete`。[CI workflow](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/.github/workflows/ci.yml)

4. 默认 bundle 会在 Windows 禁用 Bash sandbox/tool，启用 PowerShell sandbox/tool；workspace 还允许 `node-pty` 的 Windows ConPTY 构建脚本。[base bundle](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/bundle/base/cordis.patch.yml)、[pnpm workspace](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/pnpm-workspace.yaml#L32-L49)

### 边界与不确定项

- 官方未在 README 中列出“Windows 10/11 x64”这样的最低 Windows 版本矩阵。能确认的是当前 npm 源码/发布在 Windows runner 上持续测试，不能据此推导每个 Windows 10 build 都受官方承诺。
- 当前 npm CLI manifest 本身没有 `engines` 字段；Node 版本约束来自同一发布标签的仓库根 manifest。运行时管理器应将该范围作为对应 DSH 版本的兼容元数据，而不要依赖 npm 自动拒绝不兼容 Node。[CLI manifest](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/apps/cli/package.json)、[root manifest](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/package.json#L6-L9)
- Python single-exe runtime 是 JSON-RPC SDK agent，而不是 Web UI 宿主；其官方 targets 目前没有 Windows。因此不能把 Linux/macOS Python runtime wheel 当作 Windows WebUI 的分发来源。[runtime README](https://pypi.org/pypi/deepseek-harness-runtime-bin/json)

## 真实依赖与工具角色

| 工具 | 最终用户运行 Web UI | 从源码开发/构建 | 插件管理 | Python SDK |
|---|---:|---:|---:|---:|
| Node.js | 必需；当前版本范围 `^22.19.0 || >=24.0.0` | 必需 | 必需 | bundled wheel 模式不需要系统 Node |
| npm / npx | 官方最短安装启动入口使用 `npx` | 非主要包管理器，但 scripts 内有 `npm run` | 否 | 否 |
| pnpm | 普通 `dsh web` 启动不需要调用 pnpm | 必需；仓库固定 `pnpm@11.7.0` | 必需，`dsh plugin` 会把参数转发给 pnpm | 构建 single-exe 时需要 |
| Python | npm Web UI 不需要 | Python SDK/构建 workflow 需要 | 否 | `>=3.10` |
| uv | npm Web UI 不需要 | Python 构建、测试 workflow 工具 | 否 | 开发/构建工具，不是 SDK 的 Web runtime |

依据：[根 manifest](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/package.json#L6-L9)、[CLI reference 的 plugin forwarding](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/apps/cli/reference/README.md#plugin-management)、[Python workflow](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/.github/workflows/build-exe-for-python-sdk.yml#L109-L171)、[Python SDK pyproject](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/python/sdk/pyproject.toml)。

实际 npm Web runtime 不是单个 `@deepseek-ai/dsh` tarball 就能独立运行：CLI manifest 声明了大量 `@deepseek-ai/dsh-*`、Cordis、Commander、YAML 等运行依赖，npm 安装必须解析完整生产依赖闭包。[CLI manifest dependencies](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/apps/cli/package.json#L21-L84)

## 官方安装、启动与监听契约

### 官方命令

```powershell
# 从 npm 运行
npx @deepseek-ai/dsh web

# 从源码运行
git clone https://github.com/deepseek-ai/deepseek-harness.git
cd deepseek-harness
pnpm install
pnpm run build
pnpm dsh web
```

来源：[根 README](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/README.md#run)。

`web` 是 `--profile web` 的硬编码别名。Web app 接受：

- `--host <host>`
- `--port <port>`，允许 `0` 请求 OS 分配空闲端口
- 可重复的 `--trusted-host <authority...>`
- `--no-open`

默认 host 为 `127.0.0.1`、默认 port 为 `3080`。CLI 当前会明确拒绝 `--host 0.0.0.0`，这是远程代码执行暴露防护；桌面端不得尝试绕过。[CLI reference](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/apps/cli/reference/README.md#web-alias)、[Web 参数源码](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/bundle/web-app/src/startup.ts)、[Web bundle 配置](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/bundle/web-app/cordis.patch.yml)

服务绑定成功且完整 Loader tree settle 后，官方会打印精确格式：

```text
dsh web: http://127.0.0.1:<port>
```

这行是官方源码注释定义的 readiness signal；桌面端可以同时监听它并对已知 loopback URL 做 HTTP 探测。不要依赖未文档化的专用 `/health` 路由，因为官方当前没有提供该契约。[Web runtime 源码](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/bundle/web-app/src/index.ts)

### 建议的 `RuntimeLaunchSpec`

以下是对官方入口的固定化封装，属于本文的工程推导，不是官方额外发布的桌面 API：

```text
program = <受控 Node 24 x64 的 node.exe>
argv = [
  <runtimeDir>\node_modules\@deepseek-ai\dsh\lib\bin.js,
  "web",
  "--host", "127.0.0.1",
  "--port", <预留的动态 loopback 端口>,
  "--no-open"
]
cwd = <用户选择的工作区；未选择时使用桌面端安全默认工作区>
env = inherited environment + {
  "DSH_HOME": <桌面端独立数据根>
}
```

理由：CLI bin 路径由发布 manifest 固定；`web` 别名、端口与 `--no-open` 都是官方参数；调用目录是默认 workspace root；`DSH_HOME` 是官方单一用户数据根。[CLI manifest](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/apps/cli/package.json#L13-L19)、[CLI README](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/apps/cli/README.md#entry-modes)、[home resolver](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/util/home-paths/src/index.ts#L67-L95)

补充约束：

- 不建议把 `npx` 作为每次启动的 `program`，因为它把版本解析、下载与启动耦合在一起，难以做到离线启动和原子回滚。
- 每个兼容版本应安装到独立 `runtime/<version>/`，启动时只读取已激活版本；更新先在新目录完成安装/验证，再切换 active pointer。
- `DSH_TELEMETRY_DISABLED=1` 是官方支持的强制关闭开关，但设置它会覆盖 DSH 的可选遥测行为。是否默认设置属于桌面产品隐私决策，不应暗中加入 launch spec。[CLI behavior reference](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/apps/cli/reference/README.md#shared-deployment-behavior)

## 数据目录、环境变量与持久化

### 根目录与配置优先级

`resolveDshHome()` 的优先级是：显式 config > `$DSH_HOME` > `~/.dsh`。空白 `$DSH_HOME` 当作未设置。官方意图是“所有用户数据位于一个根下”。[home-paths 源码](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/util/home-paths/src/index.ts#L67-L95)

已确认的主要持久化项：

| 相对 `$DSH_HOME` 路径 | 内容/格式 | 更新风险 |
|---|---|---|
| `profiles/<name>/package.json`、`cordis.patch.yml`、`cordis.yml` | profile manifest、用户 patch、运行时生成/重写的空 root | profile/bundle 结构仍可能破坏兼容 |
| `cordis.patch.yml` | 所有 profile 共用的 machine-local override | patch row/config 变更可能导致新版本启动失败 |
| `settings.yaml` | Web 设置；YAML namespace sections，热加载 | namespace schema 由插件拥有，可能随版本变化 |
| `.credentials.yaml` | 严格版本化凭据文档；当前 `DOCUMENT_VERSION = 1` | 未知版本会拒绝加载；更新前必须备份 |
| `.env` | 用户级 launch environment fallback | process env 和工作区 `.env` 优先级更高 |
| `sessions/` | 按项目与 session 分目录；默认 `session.jsonl.zstd` | 当前 format version `0`，无迁移承诺 |
| `storages/*.json` | workspace、projection cache 等 JSON storage units | cache 可失效重建，但 workspace 等 domain 数据不可一概视为缓存 |
| `.anonymous-user-id` | 遥测匿名用户 ID | 删除会重置身份；不应随 runtime 更新删除 |

来源：[CLI profile reference](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/apps/cli/reference/README.md#profile-boot)、[settings-file](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/settings/settings-file/src/index.ts)、[credentials-local](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/credentials/credentials-local/src/index.ts)、[base bundle paths](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/bundle/base/cordis.patch.yml)、[web bundle storage](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/bundle/web-app/cordis.patch.yml)、[JSONL backend](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/session/session-persistence-jsonl/src/index.ts)。

### 环境变量

与桌面 Web runtime 直接相关且在官方代码/参考中确认的变量：

- `DSH_HOME`：覆盖所有 DSH 用户数据根。
- `DEEPSEEK_API_KEY`、`DEEPSEEK_BASE_URL`：模型凭据与兼容端点；managed credential store 也可由 Web UI 写入。
- `DEEPSEEK_SEARCH_BASE_URL`：DeepSeek web search endpoint override。
- `DSH_PERMISSION_MODE`：进程 fallback 权限模式。
- `DSH_TOOLS_MODE`：`native`、`code`、`both`。
- `DSH_TELEMETRY_MODE`、`DSH_TELEMETRY_OTLP_URL`、`DSH_TELEMETRY_DISABLED`：遥测选择与硬 opt-out。
- `NODE_USE_ENV_PROXY=1`：在支持它的 Node 版本上让源码/进程遵循 HTTP(S) proxy。

凭据/环境优先级为：继承的 process environment > `$DSH_HOME/.credentials.yaml` > invocation cwd 的 `.env` > `$DSH_HOME/.env`。[CLI shared behavior](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/apps/cli/reference/README.md#shared-deployment-behavior)、[credentials-local](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/credentials/credentials-local/src/index.ts#L1-L35)

## 包名、版本来源与可下载产物

### npm

- 产品 CLI 包名：`@deepseek-ai/dsh`；bin 名：`dsh`。[manifest](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/apps/cli/package.json#L1-L20)
- 2026-08-21 查询到 npm `latest`/`next` 都是 `0.1.1-rc.1`，对应 tarball `https://registry.npmjs.org/@deepseek-ai/dsh/-/dsh-0.1.1-rc.1.tgz`。[npm package metadata](https://registry.npmjs.org/@deepseek-ai%2Fdsh)
- npm tarball 只携带 CLI 自身 `lib/*.js` 与 `config`；完整可运行安装还要由 npm 解析 manifest 的生产依赖闭包。[manifest files/dependencies](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/apps/cli/package.json#L13-L84)

### GitHub

- 官方 GitHub Releases 当前有 `dsh-v0.1.0-rc.7`、`rc.8`、`dsh-v0.1.1-rc.1`，均标记 prerelease；发布页面当前没有附件资产（只有 GitHub 自动生成的 source archives）。[Releases API](https://api.github.com/repos/deepseek-ai/deepseek-harness/releases)、[rc.1 release](https://github.com/deepseek-ai/deepseek-harness/releases/tag/dsh-v0.1.1-rc.1)
- 因此当前没有可直接嵌入 Windows 桌面端的官方 DSH Web `.exe` 或 zip runtime closure。
- GitHub Actions 的 Python wheel artifacts 保留期只有 7 天，且 workflow 明确“保留 wheels 而非 standalone executable archives”；它们不适合作为桌面自动更新的长期下载源。[single-exe workflow](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/.github/workflows/build-exe-for-python-sdk.yml#L299-L304)

### PyPI

- `deepseek-harness-sdk` 当前版本 `0.1.1rc1`，需要 Python `>=3.10`，是通用 Python wheel。[PyPI SDK](https://pypi.org/pypi/deepseek-harness-sdk/json)
- 它精确依赖同版本 `deepseek-harness-runtime-bin`；后者只有 Linux x64、Linux arm64、macOS arm64 wheels，没有 Windows。[Python SDK README](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/python/sdk/README.md)、[PyPI runtime](https://pypi.org/pypi/deepseek-harness-runtime-bin/json)

## 官方版本发现接口

推荐的版本发现顺序：

1. **主接口：npm registry package document**  
   `GET https://registry.npmjs.org/@deepseek-ai%2Fdsh`  
   读取 `dist-tags.latest`、`versions[version].dist.tarball`、`dist.integrity`、`dist.shasum`。这是实际 Web CLI 包的发布源，最适合判断“可安装版本”。

2. **精确版本元数据**  
   `GET https://registry.npmjs.org/@deepseek-ai%2Fdsh/<version>`  
   安装前核对 package name、version、dist integrity 与依赖 manifest。版本字符串必须作为 URL segment 编码并严格校验 semver。

3. **辅助发布说明：GitHub Releases API**  
   `GET https://api.github.com/repos/deepseek-ai/deepseek-harness/releases`  
   用于展示 release notes、tag、发布时间与 prerelease 状态；不要把它作为唯一安装源，因为目前 release 没有二进制资产。

4. **辅助 tag 映射：GitHub Tags API**  
   `GET https://api.github.com/repos/deepseek-ai/deepseek-harness/tags`  
   当前 npm `0.1.1-rc.1` 对应 tag `dsh-v0.1.1-rc.1`。标签只能辅助源码审计，是否真的可安装仍以 npm registry 为准。[Tags API](https://api.github.com/repos/deepseek-ai/deepseek-harness/tags)

更新检查应发送合理 `User-Agent`、使用条件请求/缓存，并区分：网络失败、registry 无版本、发现 prerelease、新版本不满足当前桌面兼容规则、已下载但验证失败。官方版本目前全部为 RC，个人测试版可以提供“允许预发布”通道，但默认兼容策略不应把任意 `master` 提交当作可更新 runtime。

## 升级与持久化兼容风险

### 已确认的破坏点

1. 根 README 已明确宣布会有 compatibility-breaking changes。[developer preview](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/README.md#developer-preview)
2. session log 当前 `SESSION_FORMAT_VERSION = 0`；官方注释明确写明“不承诺兼容、不提供迁移”，不同版本会在读取时拒绝。[types.ts](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/core/session/src/types.ts#L16-L37)
3. 当前 JSONL 后端默认 zstd、packed chunks，并已从旧 flat artifact layout 迁移到按 project/session 目录；代码会拒绝相反 compression 或 legacy flat layout，而非静默转换。[JSONL format](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/session/session-persistence-jsonl/src/format.ts)、[JSONL backend](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/session/session-persistence-jsonl/src/index.ts)
4. `.credentials.yaml` 当前文档版本为 1，严格拒绝未知字段、未知 version 和早期 flat layout（错误中提供手工转换提示，但不是自动迁移）。[credentials-local](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/credentials/credentials-local/src/index.ts#L160-L220)
5. projection cache 的 per-unit `stateVersion` 不匹配时会丢弃并从 session log 重建；这类 cache 可失效，但 session log 仍是权威，不能随更新清理。[projection cache README](https://github.com/deepseek-ai/deepseek-harness/blob/b150a551b8d465e31e418e1b2eaf5e79bbb7d28e/packages/session/session-projection-cache/README.md)

### 桌面更新器门禁建议

- 安装新版本到新的 immutable runtime 目录，不覆盖当前版本。
- 切换前停止 DSH 进程，并对整个独立 `DSH_HOME` 做可恢复快照或至少对 schema-sensitive 文件做备份。
- 用新 runtime + 临时克隆的数据目录完成 boot/read smoke；不可用真实数据目录做首次试启动写入。
- 通过 readiness stdout 与 loopback HTTP 探测后才切换 active version。
- 保留前一 runtime 和数据快照；若启动/读取失败，回滚 runtime 与数据，两者必须成对处理。
- 不允许两个 DSH 版本并发写同一 `DSH_HOME`。

**不确定项：**官方当前没有为 Web CLI 发布机器可读的“支持哪些旧数据 schema / 自动迁移到什么版本”兼容矩阵，也没有桌面 updater contract。兼容判断只能保守地结合 release notes、源码审计与隔离 smoke；没有明确证据时应提示用户而非自动切换。

## 许可证与再分发

DeepSeek Harness 本体是 MIT：允许使用、修改、发布、分发、再许可与销售，但分发软件或其 substantial portions 时必须保留版权声明和 MIT permission notice。[LICENSE](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/LICENSE)

这意味着可以把兼容 DSH runtime 随个人/小范围测试版分发，但至少需要：

- 在安装包/应用的 Third-Party Notices 中保留 DSH 的版权与 MIT 文本；
- 保留实际 npm 生产依赖各自要求的 notices/licenses；
- 每次升级依赖闭包后重新生成/核对清单，不能只复制一次旧版 notices。

官方 `THIRD_PARTY_NOTICES.md` 说明：它只列直接依赖与特定官方 payload，完整 npm transitive closure 以 lockfile/安装闭包为准；依赖包含 MIT、Apache-2.0、BSD、ISC，以及需要单独查看条款的 payload。官方还明确说每个项目仍受自己的许可证约束。[Third-Party Notices](https://github.com/deepseek-ai/deepseek-harness/blob/dsh-v0.1.1-rc.1/THIRD_PARTY_NOTICES.md)

**边界：**本文只确认 DSH 仓库与其披露闭包的许可证事实，没有审查桌面端计划捆绑的 Node.js、WebView2 bootstrapper 或安装器工具链的再分发条款；它们需要在实际打包前单独做一手许可证审计。

## 对阶段 2 的直接建议

1. 实现 `NpmRegistryVersionSource`，以 npm registry 为安装事实，GitHub release 仅提供说明。
2. 实现版本化 runtime layout：`runtimes/dsh/<version>/node_modules/...`，另有 active manifest；数据始终位于独立可选的 `DSH_HOME`。
3. 第一版固定 Node 24 x64 运行线；每个 DSH 版本记录其 engine constraint、npm integrity、安装时间与 smoke 结果。
4. `RuntimeLaunchSpec` 使用固定 `node.exe + lib/bin.js + web --host 127.0.0.1 --port <dynamic> --no-open`，cwd 与 `DSH_HOME` 显式传入。
5. readiness 同时观察官方 stdout URL 行和 HTTP 根页面；超时/进程退出要保留 stderr 诊断。
6. 更新器先实现“发现、下载、验证、通知、用户确认切换、可回滚”，不要在 developer preview 阶段默认静默升级数据。

