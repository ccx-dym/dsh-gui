import { describe, expect, it } from "vitest";
import {
  createInitialSkinState,
  reduceSkinDraft,
  skinDraftToWire,
  type SkinStateEnvelopeWire,
} from "./skin-state";

const savedEnvelope: SkinStateEnvelopeWire = {
  revision: 4,
  settings: {
    immersive: false,
    image_digest: null,
    fit: "cover",
    position: "center",
    blur_px: 0,
    glass_blur_px: 0,
    mask_tone: "light",
    mask_opacity_percent: 22,
    panel_opacity_percent: 88,
  },
};

describe("皮肤草稿 reducer", () => {
  it("在视觉数值进入草稿前严格收敛为 Rust 可接受的整数", () => {
    const state = reduceSkinDraft(createInitialSkinState(savedEnvelope), {
      type: "visuals",
      blurPx: 99.8,
      glassBlurPx: 99.8,
      maskOpacityPercent: -4,
      imageOpacityPercent: -1,
    });

    expect(state.draft.blurPx).toBe(32);
    expect(state.draft.glassBlurPx).toBe(32);
    expect(state.draft.maskOpacityPercent).toBe(0);
    expect(state.draft.imageOpacityPercent).toBe(0);

    const maximum = reduceSkinDraft(state, {
      type: "visuals",
      imageOpacityPercent: 101,
    });
    expect(maximum.draft.imageOpacityPercent).toBe(100);
    expect(skinDraftToWire(maximum.draft).panel_opacity_percent).toBe(100);
    expect(skinDraftToWire(maximum.draft).glass_blur_px).toBe(32);
  });

  it("选图只更新本地草稿且不改变已提交 revision", () => {
    const state = reduceSkinDraft(createInitialSkinState(savedEnvelope), {
      type: "image-selected",
      image: {
        digest: "a".repeat(64),
        format: "png",
        width: 1920,
        height: 1080,
        bytes: 240_000,
        protocolUrl: `dsh-skin://localhost/${"a".repeat(64)}`,
      },
    });

    expect(state.revision).toBe(4);
    expect(state.saved.imageDigest).toBeNull();
    expect(state.draft.imageDigest).toBe("a".repeat(64));
    expect(state.previewImage?.protocolUrl).toMatch(/^dsh-skin:\/\/localhost\//);
    expect(JSON.stringify(state)).not.toContain("C:\\");
    expect(JSON.stringify(state)).not.toContain("base64");
  });

  it("忽略倒序事件并用新 revision 同步已保存设置", () => {
    const local = reduceSkinDraft(createInitialSkinState(savedEnvelope), {
      type: "immersive",
      value: true,
    });
    const stale = reduceSkinDraft(local, {
      type: "state-received",
      envelope: { ...savedEnvelope, revision: 3 },
    });
    const current = reduceSkinDraft(stale, {
      type: "state-received",
      envelope: {
        revision: 5,
        settings: {
          ...savedEnvelope.settings,
          blur_px: 12,
          glass_blur_px: 16,
        },
      },
    });

    expect(stale).toBe(local);
    expect(current.revision).toBe(5);
    expect(current.draft.blurPx).toBe(12);
    expect(current.draft.glassBlurPx).toBe(16);
  });

  it("外部事件更新数据但不结束正在执行的操作", () => {
    for (const kind of ["choose", "save", "reset"] as const) {
      const active = reduceSkinDraft(createInitialSkinState(savedEnvelope), {
        type: "operation-start",
        kind,
        id: 7,
      });
      const updated = reduceSkinDraft(active, {
        type: "state-received",
        envelope: {
          revision: 5,
          settings: { ...savedEnvelope.settings, panel_opacity_percent: 91 },
        },
      });

      expect(updated.revision).toBe(5);
      expect(updated.draft.imageOpacityPercent).toBe(91);
      expect(updated.busy).toEqual({ kind, id: 7 });
    }
  });

  it("旧 Promise 完成或失败不能覆盖较新的操作", () => {
    const first = reduceSkinDraft(createInitialSkinState(savedEnvelope), {
      type: "operation-start",
      kind: "save",
      id: 1,
    });
    const second = reduceSkinDraft(first, {
      type: "operation-start",
      kind: "reset",
      id: 2,
    });
    const staleSuccess = reduceSkinDraft(second, {
      type: "operation-envelope",
      id: 1,
      envelope: {
        revision: 9,
        settings: { ...savedEnvelope.settings, blur_px: 31 },
      },
    });
    const staleFailure = reduceSkinDraft(staleSuccess, {
      type: "operation-failed",
      id: 1,
      message: "旧错误",
    });

    expect(staleFailure).toBe(second);
    expect(staleFailure.revision).toBe(4);
    expect(staleFailure.busy).toEqual({ kind: "reset", id: 2 });
    expect(staleFailure.errorMessage).toBeNull();
  });
});
