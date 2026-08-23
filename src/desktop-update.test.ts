import { describe, expect, it } from "vitest";
import {
  createInitialDesktopUpdateState,
  desktopUpdatePresentation,
  reduceDesktopUpdateEvent,
  renderDesktopUpdateState,
} from "./desktop-update";

describe("桌面客户端更新", () => {
  it("与 DSH runtime 使用独立 revision 并拒绝旧快照", () => {
    const current = {
      revision: 4,
      phase: "available" as const,
      version: "0.1.1",
    };
    expect(
      reduceDesktopUpdateEvent(current, {
        revision: 3,
        state: { phase: "up_to_date" },
      }),
    ).toBe(current);
  });

  it("更新确认明确保留 DSH runtime 和用户数据", () => {
    const presentation = desktopUpdatePresentation({
      revision: 2,
      phase: "available",
      version: "0.1.1",
      notes: "修复启动问题",
    });
    expect(presentation.heading).toBe("DSH Desktop 0.1.1 可用");
    expect(presentation.primaryAction?.label).toBe("更新 DSH Desktop");
    expect(presentation.primaryAction?.confirmation).toContain("不会删除 DSH runtime 和数据");
  });

  it("发布说明只作为纯文本渲染", () => {
    const root = document.createElement("section");
    renderDesktopUpdateState(root, {
      revision: 2,
      phase: "available",
      version: "0.1.1",
      notes: '<img src=x onerror="alert(1)">',
    }, false);
    expect(root.querySelector("[data-desktop-update-notes]")?.textContent).toBe(
      '<img src=x onerror="alert(1)">',
    );
    expect(root.querySelector("img")).toBeNull();
  });

  it("初始状态可安全呈现未配置通道", () => {
    expect(createInitialDesktopUpdateState()).toEqual({
      revision: 0,
      phase: "unavailable",
    });
  });
});
