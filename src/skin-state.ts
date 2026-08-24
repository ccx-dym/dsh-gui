export type SkinFit = "cover" | "contain" | "stretch" | "center";
export type SkinPosition =
  | "top_left"
  | "top"
  | "top_right"
  | "left"
  | "center"
  | "right"
  | "bottom_left"
  | "bottom"
  | "bottom_right";
export type MaskTone = "light" | "dark";
export type SkinFormat = "png" | "jpeg" | "webp";

export interface SkinSettingsWire {
  immersive: boolean;
  image_digest: string | null;
  fit: SkinFit;
  position: SkinPosition;
  blur_px: number;
  mask_tone: MaskTone;
  mask_opacity_percent: number;
  panel_opacity_percent: number;
}

export interface SkinStateEnvelopeWire {
  revision: number;
  settings: SkinSettingsWire;
}

export interface SkinImageView {
  digest: string;
  format: SkinFormat;
  width: number;
  height: number;
  bytes: number;
  protocolUrl: string;
}

export interface SkinDraft {
  immersive: boolean;
  imageDigest: string | null;
  fit: SkinFit;
  position: SkinPosition;
  blurPx: number;
  maskTone: MaskTone;
  maskOpacityPercent: number;
  imageOpacityPercent: number;
}

export interface SkinEditorState {
  revision: number;
  saved: SkinDraft;
  draft: SkinDraft;
  previewImage: SkinImageView | null;
  busy: { kind: "choose" | "save" | "reset"; id: number } | null;
  lastOperationId: number;
  errorMessage: string | null;
}

export type SkinDraftAction =
  | { type: "immersive"; value: boolean }
  | { type: "fit"; value: SkinFit }
  | { type: "position"; value: SkinPosition }
  | { type: "tone"; value: MaskTone }
  | {
      type: "visuals";
      blurPx?: number;
      maskOpacityPercent?: number;
      imageOpacityPercent?: number;
    }
  | { type: "image-selected" | "preview-image"; image: SkinImageView }
  | { type: "state-received"; envelope: SkinStateEnvelopeWire }
  | {
      type: "operation-start";
      kind: NonNullable<SkinEditorState["busy"]>["kind"];
      id: number;
    }
  | { type: "operation-image"; id: number; image: SkinImageView | null }
  | { type: "operation-envelope"; id: number; envelope: SkinStateEnvelopeWire }
  | { type: "operation-failed"; id: number; message: string };

function clampInteger(value: number, minimum: number, maximum: number): number {
  // IPC 最终接收 u8；先去除小数并显式处理 NaN，保证任何 DOM 输入都不会越界。
  const integer = Number.isNaN(value) ? minimum : Math.trunc(value);
  return Math.min(maximum, Math.max(minimum, integer));
}

function draftFromWire(settings: SkinSettingsWire): SkinDraft {
  return {
    immersive: settings.immersive,
    imageDigest: settings.image_digest,
    fit: settings.fit,
    position: settings.position,
    blurPx: clampInteger(settings.blur_px, 0, 32),
    maskTone: settings.mask_tone,
    maskOpacityPercent: clampInteger(settings.mask_opacity_percent, 0, 80),
    // 保留旧 wire 字段名以兼容已经落盘的 schema 1 设置，领域语义改为图片不透明度。
    imageOpacityPercent: clampInteger(settings.panel_opacity_percent, 0, 100),
  };
}

export function skinDraftToWire(draft: SkinDraft): SkinSettingsWire {
  return {
    immersive: draft.immersive,
    image_digest: draft.imageDigest,
    fit: draft.fit,
    position: draft.position,
    blur_px: clampInteger(draft.blurPx, 0, 32),
    mask_tone: draft.maskTone,
    mask_opacity_percent: clampInteger(draft.maskOpacityPercent, 0, 80),
    panel_opacity_percent: clampInteger(draft.imageOpacityPercent, 0, 100),
  };
}

export function createInitialSkinState(
  envelope: SkinStateEnvelopeWire,
): SkinEditorState {
  const saved = draftFromWire(envelope.settings);
  return {
    revision: envelope.revision,
    saved,
    draft: { ...saved },
    previewImage: null,
    busy: null,
    lastOperationId: 0,
    errorMessage: null,
  };
}

export function reduceSkinDraft(
  state: SkinEditorState,
  action: SkinDraftAction,
): SkinEditorState {
  switch (action.type) {
    case "immersive":
      return { ...state, draft: { ...state.draft, immersive: action.value } };
    case "fit":
      return { ...state, draft: { ...state.draft, fit: action.value } };
    case "position":
      return { ...state, draft: { ...state.draft, position: action.value } };
    case "tone":
      return { ...state, draft: { ...state.draft, maskTone: action.value } };
    case "visuals":
      return {
        ...state,
        draft: {
          ...state.draft,
          blurPx:
            action.blurPx === undefined
              ? state.draft.blurPx
              : clampInteger(action.blurPx, 0, 32),
          maskOpacityPercent:
            action.maskOpacityPercent === undefined
              ? state.draft.maskOpacityPercent
              : clampInteger(action.maskOpacityPercent, 0, 80),
          imageOpacityPercent:
            action.imageOpacityPercent === undefined
              ? state.draft.imageOpacityPercent
              : clampInteger(action.imageOpacityPercent, 0, 100),
        },
      };
    case "image-selected":
    case "preview-image":
      return {
        ...state,
        draft: { ...state.draft, imageDigest: action.image.digest },
        previewImage: action.image,
        errorMessage: null,
      };
    case "state-received": {
      if (action.envelope.revision <= state.revision) return state;
      const saved = draftFromWire(action.envelope.settings);
      return {
        ...state,
        revision: action.envelope.revision,
        saved,
        draft: { ...saved },
        previewImage:
          state.previewImage?.digest === saved.imageDigest
            ? state.previewImage
            : null,
        errorMessage: null,
      };
    }
    case "operation-start":
      if (action.id <= state.lastOperationId) return state;
      return {
        ...state,
        busy: { kind: action.kind, id: action.id },
        lastOperationId: action.id,
        errorMessage: null,
      };
    case "operation-image":
      if (state.busy?.id !== action.id) return state;
      if (action.image === null) return { ...state, busy: null };
      return {
        ...reduceSkinDraft(state, {
          type: "image-selected",
          image: action.image,
        }),
        busy: null,
      };
    case "operation-envelope":
      if (state.busy?.id !== action.id) return state;
      return {
        ...reduceSkinDraft(state, {
          type: "state-received",
          envelope: action.envelope,
        }),
        busy: null,
      };
    case "operation-failed":
      if (state.busy?.id !== action.id) return state;
      return { ...state, errorMessage: action.message, busy: null };
  }
}
