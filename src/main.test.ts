import { beforeEach, describe, expect, it, vi } from "vitest";
import type { RuntimeEvent } from "./app-state";

const tauriMocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: tauriMocks.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: tauriMocks.listen }));

beforeEach(() => {
  window.history.replaceState({}, "", "/");
  document.body.innerHTML = '<div id="app"></div>';
  vi.resetModules();
  tauriMocks.invoke.mockReset();
  tauriMocks.listen.mockReset();
  tauriMocks.invoke.mockImplementation(async (command: string) =>
    command === "get_desktop_update_state"
      ? {
          revision: 0,
          state: { phase: "unavailable" },
        }
      : command === "get_update_state"
      ? {
          revision: 0,
          state: {
            revision: 0,
            phase: "unavailable",
            notificationsEnabled: true,
            shouldNotify: false,
          },
        }
      : { phase: "starting", message: "正在启动 DSH…" },
  );
  tauriMocks.listen.mockResolvedValue(() => undefined);
});

describe("更新控制台", () => {
  it("分别显示桌面客户端和 DSH runtime 更新", async () => {
    tauriMocks.invoke.mockImplementation(async (command: string) => {
      if (command === "get_desktop_update_state") {
        return {
          revision: 2,
          state: {
            phase: "available",
            version: "0.1.1",
            notes: "客户端兼容更新",
          },
        };
      }
      if (command === "get_update_state") {
        return {
          revision: 3,
          state: {
            revision: 3,
            phase: "runtime_available",
            compatibleVersion: "0.1.1-rc.2",
            artifactSize: 1024,
            notificationsEnabled: true,
            shouldNotify: false,
          },
        };
      }
      return { phase: "idle", message: "等待" };
    });

    await import("./main");
    await vi.waitFor(() => {
      expect(document.body.textContent).toContain("DSH Desktop 0.1.1 可用");
      expect(document.body.textContent).toContain("0.1.1-rc.2");
    });
    expect(document.querySelectorAll(".update-console__section")).toHaveLength(2);
  });

  it("桌面安装确认后独立防止重复提交", async () => {
    const pending = new Promise(() => undefined);
    vi.spyOn(window, "confirm").mockReturnValue(true);
    tauriMocks.invoke.mockImplementation((command: string) => {
      if (command === "get_desktop_update_state") {
        return Promise.resolve({
          revision: 7,
          state: { phase: "available", version: "0.1.1" },
        });
      }
      if (command === "install_desktop_update") return pending;
      if (command === "get_update_state") {
        return Promise.resolve({
          revision: 0,
          state: {
            revision: 0,
            phase: "unavailable",
            notificationsEnabled: true,
            shouldNotify: false,
          },
        });
      }
      return Promise.resolve({ phase: "idle", message: "等待" });
    });

    await import("./main");
    const button = await vi.waitFor(() =>
      document.querySelector<HTMLButtonElement>("[data-desktop-update-action]"),
    );
    button?.click();
    button?.click();
    expect(window.confirm).toHaveBeenCalledWith(
      expect.stringContaining("不会删除 DSH runtime 和数据"),
    );
    expect(
      tauriMocks.invoke.mock.calls.filter(
        ([name]) => name === "install_desktop_update",
      ),
    ).toEqual([
      ["install_desktop_update", { expectedRevision: 7 }],
    ]);
  });

  it("下载状态呈现真实进度并限制可访问百分比", async () => {
    tauriMocks.invoke.mockImplementation(async (command: string) =>
      command === "get_update_state"
        ? {
            revision: 2,
            state: {
              revision: 2,
              phase: "downloading",
              downloadedBytes: 150,
              downloadPercent: 100,
              artifactSize: 100,
              notificationsEnabled: true,
              shouldNotify: false,
            },
          }
        : { phase: "idle", message: "等待" },
    );

    await import("./main");
    const progress = await vi.waitFor(() =>
      document.querySelector<HTMLElement>("[role='progressbar']"),
    );
    expect(progress?.getAttribute("aria-valuenow")).toBe("100");
    expect(progress?.getAttribute("aria-valuemax")).toBe("100");
    expect(document.querySelector(".update-console__details")?.textContent).toBe(
      "100% · 150 B / 100 B",
    );
  });

  it("不由可能被节流的 WebView 定时器承担后台检查", async () => {
    const interval = vi.spyOn(window, "setInterval");
    await import("./main");
    expect(interval).not.toHaveBeenCalled();
    interval.mockRestore();
  });

  it("先监听更新事件再读取快照", async () => {
    const order: string[] = [];
    tauriMocks.listen.mockImplementation(async (name: string) => {
      if (name === "update-state") order.push("update-listen");
      return () => undefined;
    });
    tauriMocks.invoke.mockImplementation(async (command: string) => {
      if (command === "get_update_state") {
        order.push("update-snapshot");
        return {
          revision: 0,
          state: {
            revision: 0,
            phase: "uninstalled",
            notificationsEnabled: true,
            shouldNotify: false,
          },
        };
      }
      return { phase: "idle", message: "尚未安装兼容运行时" };
    });

    await import("./main");
    await vi.waitFor(() => expect(order).toHaveLength(2));
    expect(order).toEqual(["update-listen", "update-snapshot"]);
  });

  it("安装前明确确认并防止重复提交", async () => {
    const pending = new Promise(() => undefined);
    vi.spyOn(window, "confirm").mockReturnValue(true);
    tauriMocks.invoke.mockImplementation((command: string) => {
      if (command === "get_update_state") {
        return Promise.resolve({
          revision: 1,
          state: {
            revision: 1,
            phase: "runtime_available",
            currentVersion: "0.1.0",
            compatibleVersion: "0.1.2",
            artifactSize: 108_024_750,
            compatibilitySummary: "Windows 10/11 x64 验证通过",
            notificationsEnabled: true,
            shouldNotify: false,
          },
        });
      }
      if (command === "install_compatible_update") return pending;
      return Promise.resolve({ phase: "idle", message: "等待" });
    });
    await import("./main");
    const button = await vi.waitFor(() =>
      document.querySelector<HTMLButtonElement>("[data-update-action]"),
    );
    expect(button?.textContent).toBe("查看并安装");
    button?.click();
    button?.click();
    expect(window.confirm).toHaveBeenCalledWith(
      expect.stringContaining("0.1.2"),
    );
    expect(tauriMocks.invoke.mock.calls.filter(([name]) => name === "install_compatible_update")).toHaveLength(1);
    expect(button?.disabled).toBe(true);
  });

  it("release summary 只作为纯文本渲染", async () => {
    tauriMocks.invoke.mockImplementation(async (command: string) =>
      command === "get_update_state"
        ? {
            revision: 1,
            state: {
              revision: 1,
              phase: "runtime_available",
              compatibleVersion: "0.1.2",
              artifactSize: 10,
              compatibilitySummary: '<img src=x onerror="alert(1)">',
              notificationsEnabled: true,
              shouldNotify: false,
            },
          }
        : { phase: "idle", message: "等待" },
    );
    await import("./main");
    await vi.waitFor(() => expect(document.querySelector("[data-update-summary]")).not.toBeNull());
    expect(document.querySelector("[data-update-summary]")?.textContent).toBe('<img src=x onerror="alert(1)">');
    expect(document.querySelector("img[src='x']")).toBeNull();
  });
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

  it("等待安装兼容运行时时不显示加载指示", async () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    const { renderRuntimeStatus } = await import("./main");
    renderRuntimeStatus(root, {
      phase: "idle",
      message: "尚未安装兼容运行时",
    });

    expect(root.querySelector("[data-loading-indicator]")).toBeNull();
    expect(root.querySelector("[data-status-message]")?.textContent).toBe(
      "尚未安装兼容运行时",
    );
  });
});

describe("启动页", () => {
  it("appearance 视图只装配本地外观编辑器", async () => {
    window.history.replaceState({}, "", "/?view=appearance");
    tauriMocks.invoke.mockImplementation(async (command: string) => {
      if (command === "get_skin_state") {
        return {
          revision: 0,
          settings: {
            immersive: false,
            image_digest: null,
            fit: "cover",
            position: "center",
            blur_px: 0,
            mask_tone: "light",
            mask_opacity_percent: 22,
            panel_opacity_percent: 88,
          },
        };
      }
      throw new Error(`不应调用 ${command}`);
    });

    await import("./main");

    await vi.waitFor(() => {
      expect(document.querySelector(".appearance h1")?.textContent).toContain("工作台");
    });
    expect(tauriMocks.invoke).toHaveBeenCalledWith("get_skin_state", undefined);
    expect(tauriMocks.invoke).not.toHaveBeenCalledWith("get_runtime_status");
    expect(document.querySelector(".desktop__runtime")).toBeNull();
  });

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

  it("在请求状态快照前建立运行事件监听", async () => {
    const callOrder: string[] = [];
    tauriMocks.listen.mockImplementationOnce(async () => {
      callOrder.push("listen");
      return () => undefined;
    });
    tauriMocks.invoke.mockImplementationOnce(async () => {
      callOrder.push("invoke");
      return { phase: "starting", message: "正在启动 DSH…" };
    });

    await import("./main");
    await vi.waitFor(() => expect(callOrder).toHaveLength(2));

    expect(callOrder).toEqual(["listen", "invoke"]);
  });

  it("订阅期间的新事件不被较旧状态快照覆盖", async () => {
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
    tauriMocks.invoke.mockImplementationOnce(async () => {
      statusListener?.({
        payload: {
          type: "failed",
          code: "health_timeout",
          message: "健康检查超时",
        },
      });
      return { phase: "starting", message: "较旧的启动快照" };
    });

    await import("./main");

    await vi.waitFor(() => {
      expect(
        document.querySelector("[data-status-message]")?.textContent,
      ).toBe("健康检查超时");
    });
    expect(document.querySelector("[data-error-code]")?.textContent).toBe(
      "health_timeout",
    );
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

  it("运行时重试失败时不显示 bridge 返回的敏感正文", async () => {
    let retryRequested = false;
    tauriMocks.invoke.mockImplementation(async (command: string) => {
      if (command === "get_runtime_status") {
        return {
        phase: "failed",
        message: "启动超时",
        errorCode: "health_timeout",
        };
      }
      if (command === "get_update_state") {
        return {
          revision: 0,
          state: {
            revision: 0,
            phase: "unavailable",
            notificationsEnabled: true,
            shouldNotify: false,
          },
        };
      }
      if (command === "retry_runtime" && !retryRequested) {
        retryRequested = true;
        throw "Authorization: Bearer sk-proj-secret https://host/?api_key=x C:\\用户\\私密.json";
      }
      return undefined;
    });

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
      ).toBe("重试失败，请稍后再试");
    });
    const restored = document.querySelector<HTMLButtonElement>(
      "[data-action='retry']",
    );
    expect(restored?.disabled).toBe(false);
    expect(restored?.hasAttribute("aria-busy")).toBe(false);
    expect(document.body.textContent).not.toContain("Authorization");
    expect(document.body.textContent).not.toContain("api_key");
    expect(document.body.textContent).not.toContain("用户");
  });

  it("状态初始化失败时不显示 bridge 返回的敏感正文", async () => {
    tauriMocks.invoke.mockRejectedValueOnce(
      new Error(
        "AKIAIOSFODNN7EXAMPLE https://host/?token=x C:\\用户\\私密.json",
      ),
    );

    await import("./main");

    await vi.waitFor(() => {
      expect(
        document.querySelector("[data-status-message]")?.textContent,
      ).toBe("暂时无法读取运行状态");
    });
    expect(document.body.textContent).not.toContain("AKIA");
    expect(document.body.textContent).not.toContain("token");
    expect(document.body.textContent).not.toContain("用户");
  });
});
