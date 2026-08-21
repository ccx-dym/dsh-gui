import { describe, expect, it } from "vitest";
import {
  createInitialUpdateState,
  reduceUpdateEvent,
  updatePresentation,
  type UpdateState,
} from "./runtime-events";

describe("updatePresentation", () => {
  it.each([
    ["uninstalled", "尚未安装", "检查兼容版本"],
    ["checking", "正在检查", undefined],
    ["official_awaiting_compatibility", "正在等待兼容", undefined],
    ["compatible_available", "兼容版本已就绪", "查看并安装"],
    ["downloading", "正在下载", undefined],
    ["verifying", "正在验证", undefined],
    ["probing", "正在隔离探活", undefined],
    ["restart_pending", "重启后安装", "确认重启后安装"],
    ["rolling_back", "正在回滚", undefined],
    ["recovery_required", "需要人工恢复", undefined],
  ] as const)("为 %s 返回安全操作文案", (phase, heading, action) => {
    const state = { ...createInitialUpdateState(), phase } as UpdateState;
    const presentation = updatePresentation(state);
    expect(presentation.heading).toContain(heading);
    expect(presentation.primaryAction?.label).toBe(action);
  });

  it("官方新版本待兼容时绝不提供强制安装操作", () => {
    const presentation = updatePresentation({
      ...createInitialUpdateState(),
      phase: "official_awaiting_compatibility",
      officialVersion: "0.2.0",
    });
    expect(presentation.primaryAction).toBeUndefined();
  });

  it("恢复必需状态不把普通更新检查伪装成恢复操作", () => {
    const presentation = updatePresentation({
      ...createInitialUpdateState(),
      phase: "recovery_required",
    });
    expect(presentation.primaryAction).toBeUndefined();
    expect(presentation.body).toContain("诊断日志");
  });

  it("仅对可安全重试的失败开放受控冷启动入口", () => {
    const presentation = updatePresentation({
      ...createInitialUpdateState(),
      phase: "recovery_required",
      errorCode: "activation_retry_available",
    });
    expect(presentation.primaryAction?.command).toBe("confirm_activation");
    expect(presentation.primaryAction?.label).toContain("新重试");
  });

  it("兼容版本确认信息包含版本、大小与兼容摘要", () => {
    const presentation = updatePresentation({
      ...createInitialUpdateState(),
      phase: "compatible_available",
      compatibleVersion: "0.1.2",
      artifactSize: 108_024_750,
      compatibilitySummary: '<img src=x onerror="alert(1)">',
    });
    expect(presentation.details).toContain("0.1.2");
    expect(presentation.details).toContain("103.0 MB");
    expect(presentation.summary).toBe('<img src=x onerror="alert(1)">');
  });
});

describe("reduceUpdateEvent", () => {
  it("忽略比快照更新事件更旧的快照", () => {
    const initial = createInitialUpdateState();
    const fromEvent = reduceUpdateEvent(initial, {
      revision: 4,
      state: { ...initial, revision: 4, phase: "restart_pending" },
    });
    const staleSnapshot = reduceUpdateEvent(fromEvent, {
      revision: 3,
      state: { ...initial, revision: 3, phase: "checking" },
    });
    expect(staleSnapshot.phase).toBe("restart_pending");
  });

  it("busy 操作失败只暴露固定安全文案并恢复可重试", () => {
    const state = reduceUpdateEvent(createInitialUpdateState(), {
      revision: 2,
      state: {
        ...createInitialUpdateState(),
        revision: 2,
        phase: "failed",
        errorCode: "update_unavailable",
      },
    });
    const presentation = updatePresentation(state);
    expect(presentation.body).toBe("暂时无法完成更新，请稍后重试。");
    expect(presentation.primaryAction?.label).toBe("重新检查");
  });
});
