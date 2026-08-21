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
  // 每个事件都生成完整快照，避免失败或停止后泄漏上一次就绪态的 URL。
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
