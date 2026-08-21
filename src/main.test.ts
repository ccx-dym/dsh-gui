import { beforeEach, describe, expect, it, vi } from "vitest";
import type { RuntimeEvent } from "./app-state";

const tauriMocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauriMocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: tauriMocks.listen }));

beforeEach(() => {
  document.body.innerHTML = '<div id="app"></div>';
  vi.resetModules();
  tauriMocks.invoke.mockReset();
  tauriMocks.listen.mockReset();
  tauriMocks.invoke.mockResolvedValue({
    phase: "starting",
    message: "正在启动 DSH…",
  });
  tauriMocks.listen.mockResolvedValue(() => undefined);
});

describe("renderRuntimeStatus", () => {
  it("失败时显示错误码和重试按钮", async () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    const { renderRuntimeStatus } = await import("./main");
    renderRuntimeStatus(root, {
      phase: "failed",
      message: "启动超时",
      errorCode: "health_timeout",
    });

    expect(root.textContent).toContain("启动超时");
    expect(root.textContent).toContain("health_timeout");
    expect(
      root.querySelector<HTMLButtonElement>("[data-action='retry']"),
    ).not.toBeNull();
  });

  it("把后台消息和错误码作为纯文本呈现", async () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    const { renderRuntimeStatus } = await import("./main");
    renderRuntimeStatus(root, {
      phase: "failed",
      message: '<img src=x onerror="alert(1)">',
      errorCode: '<svg onload="alert(2)">',
    });

    expect(root.querySelector("[data-status-message]")?.textContent).toBe(
      '<img src=x onerror="alert(1)">',
    );
    expect(root.querySelector("[data-error-code]")?.textContent).toBe(
      '<svg onload="alert(2)">',
    );
    expect(root.querySelector("img[src='x'], svg")).toBeNull();
  });

  it("启动中提供可访问的实时状态和加载指示", async () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    const { renderRuntimeStatus } = await import("./main");
    renderRuntimeStatus(root, {
      phase: "starting",
      message: "正在启动本地服务",
    });

    const liveStatus = root.querySelector("[role='status']");
    expect(liveStatus?.getAttribute("aria-live")).toBe("polite");
    expect(liveStatus?.getAttribute("aria-atomic")).toBe("true");
    expect(root.querySelector("[data-loading-indicator]")).not.toBeNull();
  });
});

describe("启动页", () => {
  it("在 DSH 启动期间向用户显示启动状态", async () => {
    await import("./main");

    const root = document.querySelector<HTMLElement>("#app");
    expect(root?.querySelector("h1")?.textContent).toBe("DSH Desktop");
    expect(root?.querySelector("p")?.textContent).toBe("正在启动 DSH…");
  });

  it("在启动页根节点缺失时拒绝继续装配", async () => {
    document.body.innerHTML = "";

    await expect(import("./main")).rejects.toThrow("缺少 #app 根节点");
  });

  it("启动时读取并呈现当前运行状态", async () => {
    tauriMocks.invoke.mockResolvedValueOnce({
      phase: "starting",
      message: "正在检查运行环境",
    });

    await import("./main");

    await vi.waitFor(() => {
      expect(
        document.querySelector("[data-status-message]")?.textContent,
      ).toBe("正在检查运行环境");
    });
    expect(tauriMocks.invoke).toHaveBeenCalledWith("get_runtime_status");
  });

  it("收到运行事件后归约并呈现新状态", async () => {
    let statusListener:
      | ((event: { payload: RuntimeEvent }) => void)
      | undefined;
    tauriMocks.listen.mockImplementationOnce(
      async (
        _eventName: string,
        handler: (event: { payload: RuntimeEvent }) => void,
      ) => {
        statusListener = handler;
        return () => undefined;
      },
    );

    await import("./main");
    await vi.waitFor(() => expect(statusListener).toBeTypeOf("function"));
    statusListener?.({
      payload: {
        type: "failed",
        code: "health_timeout",
        message: "健康检查超时",
      },
    });

    expect(
      document.querySelector("[data-status-message]")?.textContent,
    ).toBe("健康检查超时");
    expect(document.querySelector("[data-error-code]")?.textContent).toBe(
      "health_timeout",
    );
  });

  it("通过根节点事件委托调用运行时重试", async () => {
    tauriMocks.invoke
      .mockResolvedValueOnce({
        phase: "failed",
        message: "启动超时",
        errorCode: "health_timeout",
      })
      .mockResolvedValueOnce(undefined);

    await import("./main");
    const retry = await vi.waitFor(() => {
      const button = document.querySelector<HTMLButtonElement>(
        "[data-action='retry']",
      );
      expect(button).not.toBeNull();
      return button!;
    });
    retry.click();

    await vi.waitFor(() => {
      expect(tauriMocks.invoke).toHaveBeenCalledWith("retry_runtime");
    });
  });

  it("运行时重试未完成时禁用按钮", async () => {
    const pendingRetry = new Promise<void>(() => undefined);
    tauriMocks.invoke
      .mockResolvedValueOnce({
        phase: "failed",
        message: "启动超时",
        errorCode: "health_timeout",
      })
      .mockReturnValueOnce(pendingRetry);

    await import("./main");
    const retry = await vi.waitFor(() => {
      const button = document.querySelector<HTMLButtonElement>(
        "[data-action='retry']",
      );
      expect(button).not.toBeNull();
      return button!;
    });
    retry.click();

    expect(retry.disabled).toBe(true);
    expect(retry.getAttribute("aria-busy")).toBe("true");
  });

  it("运行时重试失败时显示错误并恢复按钮", async () => {
    tauriMocks.invoke
      .mockResolvedValueOnce({
        phase: "failed",
        message: "启动超时",
        errorCode: "health_timeout",
      })
      .mockRejectedValueOnce('<img src=x onerror="alert(1)">');

    await import("./main");
    const retry = await vi.waitFor(() => {
      const button = document.querySelector<HTMLButtonElement>(
        "[data-action='retry']",
      );
      expect(button).not.toBeNull();
      return button!;
    });
    retry.click();

    await vi.waitFor(() => {
      expect(
        document.querySelector("[data-status-message]")?.textContent,
      ).toBe('重试失败：<img src=x onerror="alert(1)">');
    });
    const restored = document.querySelector<HTMLButtonElement>(
      "[data-action='retry']",
    );
    expect(restored?.disabled).toBe(false);
    expect(restored?.hasAttribute("aria-busy")).toBe(false);
    expect(document.querySelector("img[src='x']")).toBeNull();
  });
});
