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
