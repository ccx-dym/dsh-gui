import { beforeEach, describe, expect, it, vi } from "vitest";

describe("启动页", () => {
  beforeEach(() => {
    document.body.innerHTML = '<div id="app"></div>';
    vi.resetModules();
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
});
