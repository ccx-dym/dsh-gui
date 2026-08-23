export type DesktopUpdatePhase =
  | "unavailable"
  | "checking"
  | "up_to_date"
  | "available"
  | "downloading"
  | "installing"
  | "failed";

export type DesktopUpdateErrorKind =
  | "offline"
  | "invalid_metadata"
  | "signature_invalid"
  | "install_failed";

export interface DesktopUpdateState {
  revision: number;
  phase: DesktopUpdatePhase;
  version?: string;
  notes?: string;
  publishedAt?: string;
  errorKind?: DesktopUpdateErrorKind;
}

export interface DesktopUpdateEnvelope {
  revision: number;
  state: Omit<DesktopUpdateState, "revision">;
}

export interface DesktopUpdatePresentation {
  eyebrow: string;
  heading: string;
  body: string;
  notes?: string;
  primaryAction?: {
    command: "check_desktop_update" | "install_desktop_update";
    label: string;
    confirmation?: string;
  };
  busy: boolean;
}

export function createInitialDesktopUpdateState(): DesktopUpdateState {
  return { revision: 0, phase: "unavailable" };
}

export function reduceDesktopUpdateEvent(
  current: DesktopUpdateState,
  envelope: DesktopUpdateEnvelope,
): DesktopUpdateState {
  // 桌面通道拥有自己的单调 revision；它绝不与 DSH runtime revision 比较或合并。
  return envelope.revision >= current.revision
    ? { ...envelope.state, revision: envelope.revision }
    : current;
}

export function desktopUpdatePresentation(
  state: DesktopUpdateState,
): DesktopUpdatePresentation {
  switch (state.phase) {
    case "unavailable":
      return {
        eyebrow: "桌面客户端",
        heading: "客户端更新通道尚未配置",
        body: "当前版本仍可使用，不影响已安装的 DSH。",
        primaryAction: {
          command: "check_desktop_update",
          label: "检查客户端更新",
        },
        busy: false,
      };
    case "checking":
      return {
        eyebrow: "桌面客户端",
        heading: "正在检查客户端更新",
        body: "正在读取独立签名发布通道。",
        busy: true,
      };
    case "up_to_date":
      return {
        eyebrow: "桌面客户端",
        heading: "客户端已是最新版本",
        body: "DSH runtime 会继续通过自己的通道独立更新。",
        primaryAction: {
          command: "check_desktop_update",
          label: "再次检查",
        },
        busy: false,
      };
    case "available": {
      const version = state.version ?? "新版本";
      return {
        eyebrow: "桌面客户端",
        heading: `DSH Desktop ${version} 可用`,
        body: "安装时会关闭并重新启动桌面窗口。",
        notes: state.notes,
        primaryAction: {
          command: "install_desktop_update",
          label: "更新 DSH Desktop",
          confirmation:
            `更新到 DSH Desktop ${version}？安装时会关闭并重新启动桌面窗口，` +
            "不会删除 DSH runtime 和数据。",
        },
        busy: false,
      };
    }
    case "downloading":
      return {
        eyebrow: "桌面客户端",
        heading: `正在下载 ${state.version ?? "客户端更新"}`,
        body: "下载完成后会复核独立桌面签名。",
        busy: true,
      };
    case "installing":
      return {
        eyebrow: "桌面客户端",
        heading: `正在安装 ${state.version ?? "客户端更新"}`,
        body: "安装器正在接管，DSH runtime 和用户数据保持原位。",
        busy: true,
      };
    case "failed":
      return {
        eyebrow: "桌面客户端",
        heading: "客户端更新未完成",
        body:
          state.errorKind === "offline"
            ? "当前无法连接发布通道，现有客户端和 DSH 保持不变。"
            : "更新已安全停止，现有客户端和 DSH 保持不变。",
        primaryAction: {
          command: "check_desktop_update",
          label: "重新检查",
        },
        busy: false,
      };
  }
}

export function renderDesktopUpdateState(
  target: HTMLElement,
  state: DesktopUpdateState,
  busy: boolean,
): void {
  const presentation = desktopUpdatePresentation(state);
  target.replaceChildren();
  target.dataset.desktopPhase = state.phase;

  const heading = document.createElement("h3");
  heading.textContent = presentation.heading;
  const eyebrow = document.createElement("p");
  eyebrow.className = "update-console__eyebrow";
  eyebrow.textContent = presentation.eyebrow;
  const body = document.createElement("p");
  body.className = "update-console__body";
  body.textContent = presentation.body;
  target.append(eyebrow, heading, body);

  if (presentation.notes !== undefined) {
    const notes = document.createElement("p");
    notes.className = "update-console__summary";
    notes.dataset.desktopUpdateNotes = "";
    // 发布说明虽然由签名元数据提供，仍只创建文本节点以避免 HTML 注入。
    notes.textContent = presentation.notes;
    target.append(notes);
  }
  if (presentation.primaryAction !== undefined) {
    const action = document.createElement("button");
    action.type = "button";
    action.className = "update-console__action";
    action.dataset.desktopUpdateAction = presentation.primaryAction.command;
    action.dataset.confirmation = presentation.primaryAction.confirmation ?? "";
    action.textContent = presentation.primaryAction.label;
    action.disabled = busy;
    action.setAttribute("aria-busy", String(busy));
    target.append(action);
  }
  if (presentation.busy) {
    const progress = document.createElement("div");
    progress.className = "update-console__progress";
    progress.setAttribute("role", "progressbar");
    progress.setAttribute("aria-label", presentation.heading);
    target.append(progress);
  }
}
