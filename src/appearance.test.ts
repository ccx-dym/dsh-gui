import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  initializeAppearance,
  renderAppearance,
  RESTORE_CONFIRMATION,
  type AppearanceBridge,
} from "./appearance";
import { createInitialSkinState, reduceSkinDraft } from "./skin-state";

const envelope = {
  revision: 2,
  settings: {
    immersive: true,
    image_digest: "b".repeat(64),
    fit: "cover" as const,
    position: "center" as const,
    blur_px: 12,
    glass_blur_px: 0,
    mask_tone: "dark" as const,
    mask_opacity_percent: 34,
    panel_opacity_percent: 82,
  },
};

function bridge(overrides: Partial<AppearanceBridge> = {}): AppearanceBridge {
  return {
    invoke: vi.fn(async (command: string) => {
      if (command === "get_skin_state") return envelope;
      if (command === "choose_skin_image") return null;
      return envelope;
    }),
    listen: vi.fn(async () => () => undefined),
    confirm: vi.fn(() => true),
    ...overrides,
  };
}

beforeEach(() => {
  document.body.innerHTML = '<div id="app"></div>';
});

describe("外观设置", () => {
  it("只渲染一个合成背景且模糊不作用于内容层", () => {
    const state = reduceSkinDraft(createInitialSkinState(envelope), {
      type: "preview-image",
      image: {
        digest: "b".repeat(64),
        format: "jpeg",
        width: 3840,
        height: 2160,
        bytes: 720_000,
        protocolUrl: `dsh-skin://localhost/${"b".repeat(64)}`,
      },
    });
    const root = document.querySelector<HTMLElement>("#app")!;

    renderAppearance(root, state);

    expect(root.querySelectorAll("[data-skin-background]")).toHaveLength(1);
    expect(root.querySelector<HTMLElement>("[data-skin-background]")?.style.filter).toBe("blur(12px)");
    expect(root.querySelector<HTMLElement>("[data-skin-preview-content]")?.style.filter).toBe("");
    expect(root.querySelector<HTMLElement>("[data-skin-background]")?.style.pointerEvents).toBe("none");
    expect(root.querySelector<HTMLElement>("[data-skin-background]")?.style.animation).toBe("none");
    expect(root.querySelector<HTMLElement>("[data-skin-background]")?.style.transition).toBe("none");
  });

  it("连续预览只 patch 稳定节点并保留 slider 焦点", () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    const first = createInitialSkinState(envelope);
    renderAppearance(root, first);
    const background = root.querySelector<HTMLElement>("[data-skin-background]")!;
    const slider = root.querySelector<HTMLInputElement>("#skin-blur")!;
    slider.focus();

    const second = reduceSkinDraft(first, { type: "visuals", blurPx: 19 });
    renderAppearance(root, second);

    expect(root.querySelector("[data-skin-background]")).toBe(background);
    expect(root.querySelector("#skin-blur")).toBe(slider);
    expect(document.activeElement).toBe(slider);
    expect(slider.value).toBe("19");
  });

  it("连续 slider 输入复用当前节点并持续更新预览", async () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    const dispose = await initializeAppearance(root, bridge());
    const slider = root.querySelector<HTMLInputElement>("#skin-blur")!;
    const background = root.querySelector<HTMLElement>("[data-skin-background]")!;
    slider.focus();

    slider.value = "18";
    slider.dispatchEvent(new Event("input", { bubbles: true }));
    expect(background.style.filter).toBe("blur(18px)");
    slider.value = "23";
    slider.dispatchEvent(new Event("input", { bubbles: true }));

    expect(root.querySelector("#skin-blur")).toBe(slider);
    expect(document.activeElement).toBe(slider);
    expect(background.style.filter).toBe("blur(23px)");
    dispose();
  });

  it("output 使用标准 for 属性关联 range", () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    renderAppearance(root, createInitialSkinState(envelope));

    expect(root.querySelector("output[for='skin-blur']")?.textContent).toBe("12px");
    expect(root.querySelector("output[for='skin-image-opacity']")?.textContent).toBe("82%");
  });

  it("毛玻璃滑块使用 0 到 32px 并只实时模糊稳定内容层", async () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    const dispose = await initializeAppearance(root, bridge());
    const slider = root.querySelector<HTMLInputElement>("#skin-glass-blur")!;
    const output = root.querySelector<HTMLOutputElement>("output[for='skin-glass-blur']")!;
    const background = root.querySelector<HTMLElement>("[data-skin-background]")!;
    const content = root.querySelector<HTMLElement>("[data-skin-preview-content]")!;

    expect(root.querySelector("label[for='skin-glass-blur']")?.textContent).toBe("毛玻璃强度");
    expect(slider.min).toBe("0");
    expect(slider.max).toBe("32");
    expect(slider.step).toBe("1");
    expect(output.textContent).toBe("0px");
    expect(content.style.getPropertyValue("backdrop-filter")).toBe("");
    expect(content.style.boxShadow).toBe("none");

    slider.focus();
    slider.value = "16";
    slider.dispatchEvent(new Event("input", { bubbles: true }));

    expect(root.querySelector("#skin-glass-blur")).toBe(slider);
    expect(document.activeElement).toBe(slider);
    expect(output.textContent).toBe("16px");
    expect(content.style.getPropertyValue("backdrop-filter")).toBe("blur(16px) saturate(1.28)");
    expect(
      (content.style as CSSStyleDeclaration & { webkitBackdropFilter: string })
        .webkitBackdropFilter,
    ).toBe("blur(16px) saturate(1.28)");
    expect(content.style.boxShadow).not.toBe("none");
    expect(background.style.filter).toBe("blur(12px)");
    expect(root.querySelectorAll("[data-skin-background]")).toHaveLength(1);
    dispose();
  });

  it("图片不透明度实时只作用于背景图片层", async () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    const dispose = await initializeAppearance(root, bridge());
    const slider = root.querySelector<HTMLInputElement>("#skin-image-opacity")!;
    const background = root.querySelector<HTMLElement>("[data-skin-background]")!;
    const content = root.querySelector<HTMLElement>("[data-skin-preview-content]")!;

    expect(root.querySelector("label[for='skin-image-opacity']")?.textContent).toBe("图片不透明度");
    expect(slider.min).toBe("0");
    expect(slider.max).toBe("100");

    slider.value = "37";
    slider.dispatchEvent(new Event("input", { bubbles: true }));

    expect(background.style.opacity).toBe("0.37");
    expect(content.style.opacity).toBe("");
    dispose();
  });

  it("关闭沉浸模式隐藏背景和遮罩并可在原节点恢复", () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    const enabled = createInitialSkinState(envelope);
    renderAppearance(root, enabled);
    const background = root.querySelector<HTMLElement>("[data-skin-background]")!;
    expect(background.hidden).toBe(false);

    const disabled = reduceSkinDraft(enabled, { type: "immersive", value: false });
    renderAppearance(root, disabled);
    expect(background.hidden).toBe(true);

    renderAppearance(root, enabled);
    expect(root.querySelector("[data-skin-background]")).toBe(background);
    expect(background.hidden).toBe(false);
  });

  it("控件具有显式标签并可由键盘聚焦", () => {
    const root = document.querySelector<HTMLElement>("#app")!;
    renderAppearance(root, createInitialSkinState(envelope));

    expect(root.querySelector("label[for='skin-fit']")?.textContent).toContain("填充方式");
    expect(root.querySelector("label[for='skin-blur']")?.textContent).toContain("背景模糊");
    expect(root.querySelector<HTMLButtonElement>("[data-action='save']")?.type).toBe("button");
    expect(root.querySelector<HTMLButtonElement>("[data-action='save']")?.disabled).toBe(false);
  });

  it("选择图片只更新预览而不自动保存", async () => {
    const selected = {
      digest: "c".repeat(64),
      format: "webp" as const,
      width: 1600,
      height: 900,
      bytes: 190_000,
      protocolUrl: `dsh-skin://localhost/${"c".repeat(64)}`,
    };
    const fake = bridge({
      invoke: vi.fn(async (command: string) =>
        command === "get_skin_state" ? envelope : command === "choose_skin_image" ? selected : envelope,
      ),
    });
    const root = document.querySelector<HTMLElement>("#app")!;
    await initializeAppearance(root, fake);

    root.querySelector<HTMLButtonElement>("[data-action='choose']")?.click();
    await vi.waitFor(() => expect(root.querySelector("[data-image-digest]")?.textContent).toContain("cccccccc"));
    expect(fake.invoke).toHaveBeenCalledWith("choose_skin_image");
    expect(fake.invoke).not.toHaveBeenCalledWith("save_skin_settings", expect.anything());
  });

  it("保存携带当前 revision 和 snake_case 完整草稿并防重复提交", async () => {
    let finish: ((value: typeof envelope) => void) | undefined;
    const pending = new Promise<typeof envelope>((resolve) => { finish = resolve; });
    const invoke = vi.fn((command: string) =>
      command === "get_skin_state" ? Promise.resolve(envelope) : command === "save_skin_settings" ? pending : Promise.resolve(null),
    );
    const root = document.querySelector<HTMLElement>("#app")!;
    await initializeAppearance(root, bridge({ invoke }));

    const save = root.querySelector<HTMLButtonElement>("[data-action='save']")!;
    save.click();
    save.click();

    expect(invoke.mock.calls.filter(([name]) => name === "save_skin_settings")).toHaveLength(1);
    expect(invoke).toHaveBeenCalledWith("save_skin_settings", {
      expectedRevision: 2,
      draft: expect.objectContaining({
        blur_px: 12,
        glass_blur_px: 0,
        panel_opacity_percent: 82,
      }),
    });
    expect(root.querySelector<HTMLButtonElement>("[data-action='save']")?.disabled).toBe(true);
    finish?.(envelope);
  });

  it("恢复默认使用固定中文确认且不声称删除图片", async () => {
    const fake = bridge();
    const root = document.querySelector<HTMLElement>("#app")!;
    await initializeAppearance(root, fake);

    root.querySelector<HTMLButtonElement>("[data-action='reset']")?.click();
    await vi.waitFor(() => expect(fake.confirm).toHaveBeenCalledWith(RESTORE_CONFIRMATION));
    expect(RESTORE_CONFIRMATION).toContain("恢复默认外观");
    expect(RESTORE_CONFIRMATION).not.toMatch(/删除|清理/);
    expect(fake.invoke).toHaveBeenCalledWith("reset_skin_settings", { expectedRevision: 2 });
  });

  it("订阅 skin-state 且桥接错误只显示固定文案", async () => {
    let handler: ((event: { payload: typeof envelope }) => void) | undefined;
    const fake = bridge({
      listen: vi.fn(async (_event, callback) => {
        handler = callback;
        return () => undefined;
      }),
    });
    const root = document.querySelector<HTMLElement>("#app")!;
    await initializeAppearance(root, fake);
    expect(fake.listen).toHaveBeenCalledWith("skin-state", expect.any(Function));
    handler?.({ payload: { ...envelope, revision: 3 } });
    await vi.waitFor(() => expect(root.querySelector("[data-revision]")?.textContent).toContain("3"));

    const failing = bridge({ invoke: vi.fn(async () => { throw new Error("C:\\secret.png token=x"); }) });
    await initializeAppearance(root, failing);
    expect(root.textContent).toContain("暂时无法读取外观设置");
    expect(root.textContent).not.toContain("secret.png");
  });

  it("dispose 统一移除 DOM、pagehide 和 bridge 监听", async () => {
    const unlisten = vi.fn();
    const invoke = vi.fn(async (command: string) =>
      command === "get_skin_state" ? envelope : null,
    );
    const fake = bridge({
      invoke,
      listen: vi.fn(async () => unlisten),
    });
    const root = document.querySelector<HTMLElement>("#app")!;
    const dispose = await initializeAppearance(root, fake);
    window.dispatchEvent(new PageTransitionEvent("pagehide"));
    root.querySelector<HTMLButtonElement>("[data-action='choose']")?.click();

    expect(unlisten).toHaveBeenCalledTimes(1);
    expect(invoke).not.toHaveBeenCalledWith("choose_skin_image");
    dispose();
    expect(unlisten).toHaveBeenCalledTimes(1);
  });

  it("初始快照失败会立即 unlisten", async () => {
    const unlisten = vi.fn();
    const fake = bridge({
      invoke: vi.fn(async () => { throw new Error("unavailable"); }),
      listen: vi.fn(async () => unlisten),
    });
    const root = document.querySelector<HTMLElement>("#app")!;

    const dispose = await initializeAppearance(root, fake);

    expect(unlisten).toHaveBeenCalledTimes(1);
    dispose();
    expect(unlisten).toHaveBeenCalledTimes(1);
  });

  it("重复 mount 会清理旧实例且不累积点击 handler", async () => {
    const oldUnlisten = vi.fn();
    const first = bridge({ listen: vi.fn(async () => oldUnlisten) });
    const secondInvoke = vi.fn(async (command: string) =>
      command === "get_skin_state" ? envelope : null,
    );
    const second = bridge({ invoke: secondInvoke });
    const root = document.querySelector<HTMLElement>("#app")!;
    await initializeAppearance(root, first);
    const dispose = await initializeAppearance(root, second);

    expect(oldUnlisten).toHaveBeenCalledTimes(1);
    root.querySelector<HTMLButtonElement>("[data-action='choose']")?.click();
    await vi.waitFor(() => expect(secondInvoke).toHaveBeenCalledWith("choose_skin_image"));
    expect(secondInvoke.mock.calls.filter(([name]) => name === "choose_skin_image")).toHaveLength(1);
    dispose();
  });

  it("重叠 mount 会取消仍在等待 listen 的旧实例", async () => {
    const oldUnlisten = vi.fn();
    let resolveListen: ((value: () => void) => void) | undefined;
    const oldListen = new Promise<() => void>((resolve) => { resolveListen = resolve; });
    const oldInvoke = vi.fn(async () => envelope);
    const root = document.querySelector<HTMLElement>("#app")!;
    const first = initializeAppearance(root, bridge({
      invoke: oldInvoke,
      listen: vi.fn(() => oldListen),
    }));
    const second = await initializeAppearance(root, bridge());
    resolveListen?.(oldUnlisten);
    await first;

    expect(oldUnlisten).toHaveBeenCalledTimes(1);
    expect(oldInvoke).not.toHaveBeenCalled();
    second();
  });
});
