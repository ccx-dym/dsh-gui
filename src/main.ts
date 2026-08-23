import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  initialRuntimeStatus,
  reduceRuntimeEvent,
  type RuntimeEvent,
  type RuntimeStatus,
} from "./app-state";
import {
  createInitialUpdateState,
  reduceUpdateEvent,
  updatePresentation,
  type UpdateState,
  type UpdateStateEnvelope,
} from "./runtime-events";
import {
  createInitialDesktopUpdateState,
  reduceDesktopUpdateEvent,
  renderDesktopUpdateState,
  type DesktopUpdateEnvelope,
} from "./desktop-update";

export function renderRuntimeStatus(
  root: HTMLElement,
  status: RuntimeStatus,
): void {
  root.replaceChildren();

  const main = document.createElement("main");
  main.className = `boot boot--${status.phase}`;

  const atmosphere = document.createElement("div");
  atmosphere.className = "boot__atmosphere";
  atmosphere.setAttribute("aria-hidden", "true");

  const horizon = document.createElement("div");
  horizon.className = "boot__horizon";
  horizon.setAttribute("aria-hidden", "true");
  for (let waveIndex = 0; waveIndex < 3; waveIndex += 1) {
    const wave = document.createElement("span");
    wave.className = `boot__wave boot__wave--${waveIndex + 1}`;
    horizon.append(wave);
  }

  const content = document.createElement("section");
  content.className = "boot__content";
  content.setAttribute("aria-labelledby", "boot-title");

  const icon = document.createElement("img");
  icon.src = "/icon-128.png";
  icon.alt = "";
  icon.width = 96;
  icon.height = 96;
  icon.className = "boot__whale";

  const title = document.createElement("h1");
  title.id = "boot-title";
  title.textContent = "DSH Desktop";

  const statusRegion = document.createElement("div");
  statusRegion.className = "boot__status";
  statusRegion.setAttribute("role", "status");
  statusRegion.setAttribute("aria-live", "polite");
  statusRegion.setAttribute("aria-atomic", "true");

  if (
    status.phase === "starting" ||
    status.phase === "stopping"
  ) {
    const loadingIndicator = document.createElement("span");
    loadingIndicator.className = "boot__spinner";
    loadingIndicator.dataset.loadingIndicator = "";
    loadingIndicator.setAttribute("aria-hidden", "true");
    statusRegion.append(loadingIndicator);
  }

  const message = document.createElement("p");
  message.className = "boot__message";
  message.dataset.statusMessage = "";
  message.textContent = status.message;

  statusRegion.append(message);
  content.append(icon, title, statusRegion);

  if (status.phase === "failed") {
    const actions = document.createElement("div");
    actions.className = "boot__failure";
    const errorCode = document.createElement("code");
    errorCode.className = "boot__error-code";
    errorCode.dataset.errorCode = "";
    errorCode.textContent = status.errorCode ?? "unknown_error";
    const retry = document.createElement("button");
    retry.className = "boot__retry";
    retry.type = "button";
    retry.dataset.action = "retry";
    retry.textContent = "重新启动";
    actions.append(errorCode, retry);
    content.append(actions);
  }

  main.append(atmosphere, horizon, content);
  root.append(main);
}

const root = document.querySelector<HTMLElement>("#app");

if (root === null) {
  throw new Error("缺少 #app 根节点");
}

export function initializeDesktop(root: HTMLElement): void {
const runtimeRoot = document.createElement("div");
runtimeRoot.className = "desktop__runtime";
const updateRoot = document.createElement("aside");
updateRoot.className = "update-console";
updateRoot.setAttribute("aria-label", "更新中心");
const desktopUpdateRoot = document.createElement("section");
desktopUpdateRoot.className = "update-console__section update-console__section--desktop";
desktopUpdateRoot.setAttribute("aria-label", "桌面客户端更新");
const runtimeUpdateRoot = document.createElement("section");
runtimeUpdateRoot.className = "update-console__section update-console__section--runtime";
runtimeUpdateRoot.setAttribute("aria-label", "DSH 运行时更新");
updateRoot.append(desktopUpdateRoot, runtimeUpdateRoot);
root.replaceChildren(runtimeRoot, updateRoot);

let updateState = createInitialUpdateState();
let updateBusy = false;
let desktopUpdateState = createInitialDesktopUpdateState();
let desktopUpdateBusy = false;

async function resyncUpdateFailure(): Promise<void> {
  try {
    const snapshot = await invoke<UpdateStateEnvelope>("get_update_state");
    updateState = reduceUpdateEvent(updateState, snapshot);
  } catch {
    // bridge 整体不可用时只改变安全文案，不伪造 Rust 拥有的单调 revision。
    updateState = {
      ...updateState,
      phase: "failed",
      errorCode: "update_unavailable",
      shouldNotify: false,
    };
  }
}

function renderUpdateState(target: HTMLElement, state: UpdateState): void {
  const presentation = updatePresentation(state);
  target.replaceChildren();
  target.dataset.phase = state.phase;

  const rail = document.createElement("div");
  rail.className = "update-console__rail";
  const signal = document.createElement("span");
  signal.className = "update-console__signal";
  signal.setAttribute("aria-hidden", "true");
  const eyebrow = document.createElement("p");
  eyebrow.className = "update-console__eyebrow";
  eyebrow.textContent = presentation.eyebrow;
  const heading = document.createElement("h2");
  heading.textContent = presentation.heading;
  const body = document.createElement("p");
  body.className = "update-console__body";
  body.textContent = presentation.body;
  rail.append(signal, eyebrow, heading, body);

  if (presentation.details !== undefined) {
    const details = document.createElement("p");
    details.className = "update-console__details";
    details.textContent = presentation.details;
    rail.append(details);
  }
  if (presentation.summary !== undefined) {
    const summary = document.createElement("p");
    summary.className = "update-console__summary";
    summary.dataset.updateSummary = "";
    // 兼容摘要来自已签名清单，但仍只按文本节点呈现，保持双重防线。
    summary.textContent = presentation.summary;
    rail.append(summary);
  }
  if (presentation.primaryAction !== undefined) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = "update-console__action";
    button.dataset.updateAction = presentation.primaryAction.command;
    button.dataset.confirmation = presentation.primaryAction.confirmation ?? "";
    button.textContent = presentation.primaryAction.label;
    button.disabled = updateBusy;
    button.setAttribute("aria-busy", String(updateBusy));
    rail.append(button);
  }
  if (presentation.busy) {
    const progress = document.createElement("div");
    progress.className = "update-console__progress";
    progress.setAttribute("role", "progressbar");
    progress.setAttribute("aria-label", presentation.heading);
    progress.setAttribute("aria-valuemax", "100");
    if (state.downloadPercent !== undefined && Number.isFinite(state.downloadPercent)) {
      const percent = Math.max(0, Math.min(100, Math.trunc(state.downloadPercent)));
      progress.setAttribute("aria-valuenow", String(percent));
    }
    rail.append(progress);
  }
  target.append(rail);
}

async function resyncDesktopUpdateFailure(): Promise<void> {
  try {
    const snapshot = await invoke<DesktopUpdateEnvelope>(
      "get_desktop_update_state",
    );
    desktopUpdateState = reduceDesktopUpdateEvent(desktopUpdateState, snapshot);
  } catch {
    // 动态 bridge 错误不得进入 DOM；保留 revision，仅显示固定失败类别。
    desktopUpdateState = {
      revision: desktopUpdateState.revision,
      phase: "failed",
      errorKind: "install_failed",
    };
  }
}

// 事件只绑定在稳定的根节点上；状态重绘会替换按钮，但不会累积监听器。
root.addEventListener("click", (event: MouseEvent) => {
  if (!(event.target instanceof Element)) {
    return;
  }
  const desktopAction = event.target.closest<HTMLButtonElement>(
    "button[data-desktop-update-action]",
  );
  if (desktopAction !== null && root.contains(desktopAction)) {
    if (desktopUpdateBusy) return;
    const confirmation = desktopAction.dataset.confirmation;
    if (confirmation && !window.confirm(confirmation)) return;
    desktopUpdateBusy = true;
    renderDesktopUpdateState(
      desktopUpdateRoot,
      desktopUpdateState,
      desktopUpdateBusy,
    );
    const command = desktopAction.dataset.desktopUpdateAction!;
    const args = command === "install_desktop_update"
      ? { expectedRevision: desktopUpdateState.revision }
      : undefined;
    void invoke<DesktopUpdateEnvelope>(command, args)
      .then((envelope) => {
        desktopUpdateState = reduceDesktopUpdateEvent(
          desktopUpdateState,
          envelope,
        );
      })
      .catch(resyncDesktopUpdateFailure)
      .finally(() => {
        desktopUpdateBusy = false;
        renderDesktopUpdateState(
          desktopUpdateRoot,
          desktopUpdateState,
          desktopUpdateBusy,
        );
      });
    return;
  }
  const updateAction = event.target.closest<HTMLButtonElement>(
    "button[data-update-action]",
  );
  if (updateAction !== null && root.contains(updateAction)) {
    if (updateBusy) return;
    const confirmation = updateAction.dataset.confirmation;
    if (confirmation && !window.confirm(confirmation)) return;
    updateBusy = true;
    updateAction.disabled = true;
    updateAction.setAttribute("aria-busy", "true");
    renderUpdateState(runtimeUpdateRoot, updateState);
    const command = updateAction.dataset.updateAction!;
    void invoke<UpdateStateEnvelope>(command, {
      expectedRevision: updateState.revision,
    })
      .then((envelope) => {
        updateState = reduceUpdateEvent(updateState, envelope);
      })
      .catch(resyncUpdateFailure)
      .finally(() => {
        updateBusy = false;
        renderUpdateState(runtimeUpdateRoot, updateState);
      });
    return;
  }
  const retry = event.target.closest<HTMLButtonElement>(
    "button[data-action='retry']",
  );
  if (retry === null || !root.contains(retry)) {
    return;
  }
  retry.disabled = true;
  retry.setAttribute("aria-busy", "true");
  retry.textContent = "正在重试…";
  void invoke<void>("retry_runtime").catch(() => {
    renderRuntimeStatus(runtimeRoot, {
      phase: "failed",
      message: "重试失败，请稍后再试",
      errorCode: "retry_failed",
    });
  });
});

renderRuntimeStatus(runtimeRoot, { phase: "starting", message: "正在启动 DSH…" });
renderDesktopUpdateState(desktopUpdateRoot, desktopUpdateState, false);
renderUpdateState(runtimeUpdateRoot, updateState);

async function initializeRuntimeStatus(root: HTMLElement): Promise<void> {
  let status = initialRuntimeStatus;
  let hasReceivedRuntimeEvent = false;
  try {
    // 先建立订阅以封闭启动竞态窗口；订阅期间的新事件优先于随后返回的旧快照。
    await listen<RuntimeEvent>("runtime-status", ({ payload }) => {
      hasReceivedRuntimeEvent = true;
      status = reduceRuntimeEvent(status, payload);
      renderRuntimeStatus(root, status);
    });
    const snapshot = await invoke<RuntimeStatus>("get_runtime_status");
    if (!hasReceivedRuntimeEvent) {
      status = snapshot;
      renderRuntimeStatus(root, status);
    }
  } catch {
    if (!hasReceivedRuntimeEvent) {
      renderRuntimeStatus(root, {
        phase: "failed",
        message: "暂时无法读取运行状态",
        errorCode: "status_unavailable",
      });
    }
  }
}

async function initializeUpdateState(): Promise<void> {
  try {
    // 与运行时状态一致：先订阅，再取 revision 快照；两者统一择新而非依赖布尔标记。
    await listen<UpdateStateEnvelope>("update-state", ({ payload }) => {
      updateState = reduceUpdateEvent(updateState, payload);
      renderUpdateState(runtimeUpdateRoot, updateState);
    });
    const snapshot = await invoke<UpdateStateEnvelope>("get_update_state");
    updateState = reduceUpdateEvent(updateState, snapshot);
    renderUpdateState(runtimeUpdateRoot, updateState);
  } catch {
    updateState = {
      ...createInitialUpdateState(),
      phase: "unavailable",
      errorCode: "update_unavailable",
    };
    renderUpdateState(runtimeUpdateRoot, updateState);
  }
}

async function initializeDesktopUpdateState(): Promise<void> {
  try {
    await listen<DesktopUpdateEnvelope>("desktop-update-state", ({ payload }) => {
      desktopUpdateState = reduceDesktopUpdateEvent(desktopUpdateState, payload);
      renderDesktopUpdateState(
        desktopUpdateRoot,
        desktopUpdateState,
        desktopUpdateBusy,
      );
    });
    const snapshot = await invoke<DesktopUpdateEnvelope>(
      "get_desktop_update_state",
    );
    desktopUpdateState = reduceDesktopUpdateEvent(desktopUpdateState, snapshot);
  } catch {
    desktopUpdateState = createInitialDesktopUpdateState();
  }
  renderDesktopUpdateState(
    desktopUpdateRoot,
    desktopUpdateState,
    desktopUpdateBusy,
  );
}

void initializeRuntimeStatus(runtimeRoot);
void initializeUpdateState();
void initializeDesktopUpdateState();
}

// appearance 是预定义的本地窗口入口；分派发生在任何运行时命令之前，避免设置 UI 混入主启动页。
const view = new URLSearchParams(window.location.search).get("view");
if (view === "appearance") {
  void import("./appearance").then(({ initializeAppearance }) =>
    initializeAppearance(root),
  );
} else {
  initializeDesktop(root);
}
