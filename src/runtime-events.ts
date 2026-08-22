export type UpdatePhase =
  | "unavailable"
  | "uninstalled"
  | "checking"
  | "up_to_date"
  | "official_available"
  | "runtime_available"
  | "desktop_required"
  | "skin_unverified"
  | "offline"
  | "downloading"
  | "verifying"
  | "probing"
  | "restart_pending"
  | "rolling_back"
  | "recovery_required"
  | "failed";

export interface UpdateState {
  revision: number;
  phase: UpdatePhase;
  currentVersion?: string;
  officialVersion?: string;
  compatibleVersion?: string;
  artifactSize?: number;
  downloadedBytes?: number;
  downloadPercent?: number;
  skinCompatible?: boolean;
  compatibilitySummary?: string;
  minimumDesktopVersion?: string;
  errorCode?: string;
  notificationsEnabled: boolean;
  shouldNotify: boolean;
}

export interface UpdateStateEnvelope {
  revision: number;
  state: UpdateState;
}

export interface UpdateAction {
  command:
    | "check_updates"
    | "install_compatible_update"
    | "confirm_activation";
  label: string;
  confirmation?: string;
}

export interface UpdatePresentation {
  eyebrow: string;
  heading: string;
  body: string;
  details?: string;
  summary?: string;
  primaryAction?: UpdateAction;
  busy: boolean;
}

export function createInitialUpdateState(): UpdateState {
  return {
    revision: 0,
    phase: "unavailable",
    notificationsEnabled: true,
    shouldNotify: false,
  };
}

export function reduceUpdateEvent(
  current: UpdateState,
  envelope: UpdateStateEnvelope,
): UpdateState {
  // revision 由 Rust 状态机单调递增；快照晚返回时不能覆盖订阅期间的新事件。
  return envelope.revision >= current.revision ? envelope.state : current;
}

function formatBytes(bytes: number): string {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function formatProgressBytes(bytes: number): string {
  const bounded = Math.max(0, bytes);
  if (bounded < 1024) return `${bounded} B`;
  if (bounded < 1024 * 1024) return `${(bounded / 1024).toFixed(1)} KB`;
  return `${(bounded / 1024 / 1024).toFixed(1)} MB`;
}

function safeDownloadPercent(percent: number | undefined): number | undefined {
  if (percent === undefined || !Number.isFinite(percent)) return undefined;
  return Math.max(0, Math.min(100, Math.trunc(percent)));
}

export function updatePresentation(state: UpdateState): UpdatePresentation {
  const retry: UpdateAction = {
    command: "check_updates",
    label: "重新检查",
  };
  switch (state.phase) {
    case "unavailable":
      return {
        eyebrow: "更新服务",
        heading: "发布通道尚未配置",
        body: "当前测试构建没有兼容发布源，已安全保持现状。",
        busy: false,
      };
    case "uninstalled":
      return {
        eyebrow: "首次使用",
        heading: "尚未安装 DSH 运行时",
        body: "先检查经过签名验证的兼容版本。",
        primaryAction: { command: "check_updates", label: "检查兼容版本" },
        busy: false,
      };
    case "checking":
      return {
        eyebrow: "版本通道",
        heading: "正在检查更新",
        body: "分别核对 DSH 官方版本和桌面端兼容清单。",
        busy: true,
      };
    case "up_to_date":
      return {
        eyebrow: "版本通道",
        heading: "当前已是兼容版本",
        body: `已安装 ${state.currentVersion ?? "当前版本"}，无需操作。`,
        primaryAction: { command: "check_updates", label: "再次检查" },
        busy: false,
      };
    case "official_available":
      return {
        eyebrow: "兼容性保护",
        heading: "官方版本已发布",
        body: `DSH ${state.officialVersion ?? "新版本"} 已发布；兼容验证完成后才会开放安装。`,
        busy: false,
      };
    case "runtime_available": {
      const version = state.compatibleVersion ?? "未知版本";
      const size = state.artifactSize === undefined ? "大小未知" : formatBytes(state.artifactSize);
      const firstInstall = state.currentVersion === undefined;
      const actionLabel = firstInstall ? `安装 DSH ${version}` : "查看并安装";
      return {
        eyebrow: "兼容版本",
        heading: firstInstall ? `安装 DSH ${version}` : "运行时更新可用",
        body: "运行中只下载并暂存；不会中断当前任务。",
        details: `${version} · ${size}`,
        summary: state.compatibilitySummary,
        primaryAction: state.artifactSize === undefined ? undefined : {
          command: "install_compatible_update",
          label: actionLabel,
          confirmation: `下载并验证 DSH ${version}（${size}）。完成后需重启才能安装/激活。`,
        },
        busy: false,
      };
    }
    case "desktop_required":
      return {
        eyebrow: "客户端兼容性",
        heading: "请先更新桌面客户端",
        body: `DSH ${state.compatibleVersion ?? state.officialVersion ?? "新版本"} 需要 DSH Desktop ${state.minimumDesktopVersion ?? "更新版本"} 或更高版本。`,
        busy: false,
      };
    case "skin_unverified": {
      const version = state.compatibleVersion ?? "当前版本";
      const size = state.artifactSize === undefined ? "大小未知" : formatBytes(state.artifactSize);
      const firstInstall = state.currentVersion === undefined;
      const actionLabel = firstInstall ? `安装 DSH ${version}` : "查看并安装";
      return {
        eyebrow: "皮肤兼容性",
        heading: firstInstall ? `安装 DSH ${version}` : "皮肤尚未验证",
        body: "安装此版本后会关闭自定义皮肤并恢复官方界面；DSH 核心功能仍可使用。",
        details: state.artifactSize === undefined ? version : `${version} · ${size}`,
        summary: state.compatibilitySummary,
        primaryAction: state.artifactSize === undefined ? undefined : {
          command: "install_compatible_update",
          label: actionLabel,
          confirmation: `下载并验证 DSH ${version}（${size}）。更新后将关闭自定义皮肤。`,
        },
        busy: false,
      };
    }
    case "offline":
      return {
        eyebrow: "网络状态",
        heading: "当前处于离线状态",
        body: "无法检查新版本；已安装的 DSH 和用户数据保持不变。",
        primaryAction: retry,
        busy: false,
      };
    case "downloading": {
      const downloaded = Math.max(0, state.downloadedBytes ?? 0);
      const percent = safeDownloadPercent(state.downloadPercent);
      const knownTotal = percent !== undefined && state.artifactSize !== undefined;
      return {
        eyebrow: "安全暂存",
        heading: "正在下载",
        body: "校验大小与 SHA-256 后才会继续。",
        details: knownTotal
          ? `${percent}% · ${formatProgressBytes(downloaded)} / ${formatProgressBytes(state.artifactSize!)}`
          : `已下载 ${formatProgressBytes(downloaded)} · 总大小未知`,
        busy: true,
      };
    }
    case "verifying":
      return { eyebrow: "安全暂存", heading: "正在验证", body: "正在复核运行时内容闭包。", busy: true };
    case "probing":
      return { eyebrow: "兼容验证", heading: "正在隔离探活", body: "使用独立数据 generation 验证启动与健康状态。", busy: true };
    case "restart_pending":
      return {
        eyebrow: "已安全暂存",
        heading: "重启后安装/激活",
        body: state.errorCode === "activation_confirmed"
          ? "已安排到下次冷启动。当前 DSH 不会被中断。"
          : "当前 DSH 继续运行；下次冷启动且 supervisor 未启动时才会切换。",
        primaryAction: state.errorCode === "activation_confirmed"
          ? undefined
          : {
              command: "confirm_activation",
              label: "确认重启后安装",
              confirmation: "确认将已验证版本安排到下次冷启动激活？如启动失败会自动回滚。",
            },
        busy: false,
      };
    case "rolling_back":
      return { eyebrow: "版本保护", heading: "正在回滚", body: "新版未通过启动验证，正在恢复上一组运行时和数据。", busy: true };
    case "recovery_required":
      return state.errorCode === "activation_retry_available"
        ? {
            eyebrow: "恢复模式",
            heading: "可安全重试",
            body: "上次隔离启动未完成；旧版本与原数据保持不变。",
            primaryAction: {
              command: "confirm_activation",
              label: "安排一次新重试",
              confirmation: "确认创建新的隔离数据 generation，并在下次冷启动重试已验证版本？",
            },
            busy: false,
          }
        : { eyebrow: "恢复模式", heading: "需要人工恢复", body: "自动恢复未能安全完成。请保留数据目录和诊断日志。", busy: false };
    case "failed":
      return { eyebrow: "更新暂停", heading: "本次更新未完成", body: "暂时无法完成更新，请稍后重试。", primaryAction: retry, busy: false };
  }
}
