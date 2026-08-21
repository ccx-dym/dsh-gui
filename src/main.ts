import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  initialRuntimeStatus,
  reduceRuntimeEvent,
  type RuntimeEvent,
  type RuntimeStatus,
} from "./app-state";

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

// 事件只绑定在稳定的根节点上；状态重绘会替换按钮，但不会累积监听器。
root.addEventListener("click", (event: MouseEvent) => {
  if (!(event.target instanceof Element)) {
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
    renderRuntimeStatus(root, {
      phase: "failed",
      message: "重试失败，请稍后再试",
      errorCode: "retry_failed",
    });
  });
});

renderRuntimeStatus(root, { phase: "starting", message: "正在启动 DSH…" });

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

void initializeRuntimeStatus(root);
