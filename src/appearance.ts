import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import "./appearance.css";
import {
  createInitialSkinState,
  reduceSkinDraft,
  skinDraftToWire,
  type MaskTone,
  type SkinEditorState,
  type SkinFit,
  type SkinImageView,
  type SkinPosition,
  type SkinStateEnvelopeWire,
} from "./skin-state";

export const RESTORE_CONFIRMATION = "确定恢复默认外观吗？已导入的图片仍会保留。";

export interface AppearanceBridge {
  invoke(command: string, args?: Record<string, unknown>): Promise<unknown>;
  listen(
    event: string,
    handler: (event: { payload: SkinStateEnvelopeWire }) => void,
  ): Promise<() => void>;
  confirm(message: string): boolean;
}

const defaultBridge: AppearanceBridge = {
  invoke: (command, args) => invoke(command, args),
  listen: (event, handler) => listen<SkinStateEnvelopeWire>(event, handler),
  confirm: (message) => window.confirm(message),
};

const FIT_LABELS: ReadonlyArray<[SkinFit, string]> = [
  ["cover", "铺满窗口"],
  ["contain", "完整显示"],
  ["stretch", "拉伸填充"],
  ["center", "原始尺寸居中"],
];

const POSITION_LABELS: ReadonlyArray<[SkinPosition, string]> = [
  ["top_left", "左上"],
  ["top", "顶部"],
  ["top_right", "右上"],
  ["left", "左侧"],
  ["center", "居中"],
  ["right", "右侧"],
  ["bottom_left", "左下"],
  ["bottom", "底部"],
  ["bottom_right", "右下"],
];

const BACKGROUND_POSITIONS: Record<SkinPosition, string> = {
  top_left: "left top",
  top: "center top",
  top_right: "right top",
  left: "left center",
  center: "center center",
  right: "right center",
  bottom_left: "left bottom",
  bottom: "center bottom",
  bottom_right: "right bottom",
};

interface AppearanceViewRefs {
  page: HTMLElement;
  preview: HTMLElement;
  background: HTMLElement;
  content: HTMLElement;
  imageNote: HTMLElement;
  imageDigest: HTMLElement;
  immersive: HTMLInputElement;
  fit: HTMLSelectElement;
  position: HTMLSelectElement;
  tone: HTMLSelectElement;
  blur: HTMLInputElement;
  blurOutput: HTMLOutputElement;
  glassBlur: HTMLInputElement;
  glassBlurOutput: HTMLOutputElement;
  mask: HTMLInputElement;
  maskOutput: HTMLOutputElement;
  imageOpacity: HTMLInputElement;
  imageOpacityOutput: HTMLOutputElement;
  status: HTMLElement;
  choose: HTMLButtonElement;
  save: HTMLButtonElement;
  reset: HTMLButtonElement;
}

type WebkitBackdropStyle = CSSStyleDeclaration & {
  webkitBackdropFilter: string;
};

const appearanceViews = new WeakMap<HTMLElement, AppearanceViewRefs>();
const activeDisposers = new WeakMap<HTMLElement, () => void>();

function appendOptions<T extends string>(
  select: HTMLSelectElement,
  options: ReadonlyArray<[T, string]>,
  current: T,
): void {
  for (const [value, label] of options) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = label;
    option.selected = value === current;
    select.append(option);
  }
}

function createSlider(
  id: string,
  labelText: string,
  value: number,
  minimum: number,
  maximum: number,
  action: string,
): HTMLElement {
  const field = document.createElement("div");
  field.className = "appearance-field appearance-field--range";
  const label = document.createElement("label");
  label.htmlFor = id;
  label.textContent = labelText;
  const output = document.createElement("output");
  output.setAttribute("for", id);
  output.textContent = `${value}%`;
  if (action === "blur" || action === "glass-blur") output.textContent = `${value}px`;
  const input = document.createElement("input");
  input.id = id;
  input.type = "range";
  input.min = String(minimum);
  input.max = String(maximum);
  input.step = "1";
  input.value = String(value);
  input.dataset.control = action;
  field.append(label, output, input);
  return field;
}

function previewUrl(state: SkinEditorState): string | null {
  if (state.previewImage?.digest === state.draft.imageDigest) {
    return state.previewImage.protocolUrl;
  }
  return state.draft.imageDigest === null
    ? null
    : `dsh-skin://localhost/${state.draft.imageDigest}`;
}

function patchAppearanceView(
  refs: AppearanceViewRefs,
  state: SkinEditorState,
): void {
  refs.preview.dataset.tone = state.draft.maskTone;
  refs.preview.dataset.immersive = String(state.draft.immersive);
  refs.preview.style.setProperty("--mask-opacity", String(state.draft.maskOpacityPercent / 100));
  const imageUrl = previewUrl(state);
  refs.background.style.backgroundImage =
    imageUrl === null ? "none" : `url(${JSON.stringify(imageUrl)})`;
  refs.background.style.backgroundSize =
    state.draft.fit === "stretch"
      ? "100% 100%"
      : state.draft.fit === "center"
        ? "auto"
        : state.draft.fit;
  refs.background.style.backgroundPosition = BACKGROUND_POSITIONS[state.draft.position];
  refs.background.style.filter = `blur(${state.draft.blurPx}px)`;
  refs.background.style.transform = `scale(${1 + state.draft.blurPx / 320})`;
  refs.background.style.opacity = String(state.draft.imageOpacityPercent / 100);
  refs.background.style.pointerEvents = "none";
  refs.background.style.animation = "none";
  refs.background.style.transition = "none";
  // hidden 同时关闭背景本体及其遮罩伪元素，不留下半透明覆盖层。
  refs.background.hidden = !state.draft.immersive;
  refs.content.style.filter = "";
  const glassBlur = state.draft.glassBlurPx;
  if (glassBlur === 0) {
    refs.content.style.removeProperty("backdrop-filter");
    (refs.content.style as WebkitBackdropStyle).webkitBackdropFilter = "";
    refs.content.style.background = "transparent";
    refs.content.style.boxShadow = "none";
  } else {
    const glassFilter = `blur(${glassBlur}px) saturate(1.28)`;
    refs.content.style.setProperty("backdrop-filter", glassFilter);
    (refs.content.style as WebkitBackdropStyle).webkitBackdropFilter = glassFilter;
    refs.content.style.background =
      state.draft.maskTone === "dark"
        ? "rgba(22, 28, 38, 0.36)"
        : "rgba(255, 255, 255, 0.36)";
    refs.content.style.boxShadow =
      "inset 0 1px 0 rgba(255, 255, 255, 0.20), 0 18px 48px rgba(0, 0, 0, 0.18)";
  }

  refs.immersive.checked = state.draft.immersive;
  refs.fit.value = state.draft.fit;
  refs.position.value = state.draft.position;
  refs.tone.value = state.draft.maskTone;
  refs.blur.value = String(state.draft.blurPx);
  refs.blurOutput.textContent = `${state.draft.blurPx}px`;
  refs.glassBlur.value = String(state.draft.glassBlurPx);
  refs.glassBlurOutput.textContent = `${state.draft.glassBlurPx}px`;
  refs.mask.value = String(state.draft.maskOpacityPercent);
  refs.maskOutput.textContent = `${state.draft.maskOpacityPercent}%`;
  refs.imageOpacity.value = String(state.draft.imageOpacityPercent);
  refs.imageOpacityOutput.textContent = `${state.draft.imageOpacityPercent}%`;

  const hasImage = state.draft.imageDigest !== null;
  refs.imageNote.firstChild!.textContent = hasImage
    ? "已选择托管副本 · "
    : "尚未选择图片 · PNG / JPEG / WebP";
  refs.imageDigest.hidden = !hasImage;
  refs.imageDigest.textContent = state.draft.imageDigest?.slice(0, 8) ?? "";
  refs.status.textContent = state.errorMessage ?? `设置版本 ${state.revision}`;
  const busyKind = state.busy?.kind;
  const disabled = busyKind !== undefined;
  refs.choose.disabled = disabled;
  refs.choose.textContent = busyKind === "choose" ? "正在导入…" : "选择图片";
  refs.save.disabled = disabled;
  refs.save.textContent = busyKind === "save" ? "正在保存…" : "保存外观";
  refs.reset.disabled = disabled;
  refs.reset.textContent = busyKind === "reset" ? "正在恢复…" : "恢复默认";
}

export function renderAppearance(
  root: HTMLElement,
  state: SkinEditorState,
): void {
  const mounted = appearanceViews.get(root);
  if (mounted !== undefined && root.contains(mounted.page)) {
    patchAppearanceView(mounted, state);
    return;
  }
  root.replaceChildren();
  const page = document.createElement("main");
  page.className = "appearance";

  const heading = document.createElement("header");
  heading.className = "appearance__heading";
  const eyebrow = document.createElement("p");
  eyebrow.className = "appearance__eyebrow";
  eyebrow.textContent = "DSH DESKTOP · APPEARANCE";
  const title = document.createElement("h1");
  title.textContent = "把工作台放进你的天空";
  const introduction = document.createElement("p");
  introduction.textContent = "选择一张本地图片，在独立预览中调整；保存前不会改变 DSH。";
  heading.append(eyebrow, title, introduction);

  const layout = document.createElement("div");
  layout.className = "appearance__layout";
  const preview = document.createElement("section");
  preview.className = "skin-preview";
  preview.setAttribute("aria-label", "皮肤实时预览");
  preview.dataset.tone = state.draft.maskTone;
  preview.dataset.immersive = String(state.draft.immersive);
  preview.style.setProperty("--mask-opacity", String(state.draft.maskOpacityPercent / 100));

  const background = document.createElement("div");
  background.className = "skin-preview__background";
  background.dataset.skinBackground = "";
  background.setAttribute("aria-hidden", "true");
  const imageUrl = previewUrl(state);
  if (imageUrl !== null) {
    background.style.backgroundImage = `url(${JSON.stringify(imageUrl)})`;
  }
  background.style.backgroundSize =
    state.draft.fit === "stretch"
      ? "100% 100%"
      : state.draft.fit === "center"
        ? "auto"
        : state.draft.fit;
  background.style.backgroundPosition = BACKGROUND_POSITIONS[state.draft.position];
  background.style.filter = `blur(${state.draft.blurPx}px)`;
  background.style.transform = `scale(${1 + state.draft.blurPx / 320})`;
  background.style.opacity = String(state.draft.imageOpacityPercent / 100);
  background.style.pointerEvents = "none";
  background.style.animation = "none";
  background.style.transition = "none";
  background.hidden = !state.draft.immersive;

  const content = document.createElement("div");
  content.className = "skin-preview__content";
  content.dataset.skinPreviewContent = "";
  const previewBrand = document.createElement("span");
  previewBrand.className = "skin-preview__brand";
  previewBrand.textContent = "DSH";
  const previewCopy = document.createElement("div");
  const previewTitle = document.createElement("strong");
  previewTitle.textContent = "沉浸工作台";
  const previewDescription = document.createElement("span");
  previewDescription.textContent = "对话、工具和审批仍在清晰的内容层上";
  previewCopy.append(previewTitle, previewDescription);
  content.append(previewBrand, previewCopy);
  preview.append(background, content);

  const editor = document.createElement("section");
  editor.className = "appearance-editor";
  editor.setAttribute("aria-labelledby", "appearance-editor-title");
  const editorTitle = document.createElement("h2");
  editorTitle.id = "appearance-editor-title";
  editorTitle.textContent = "外观参数";

  const choose = document.createElement("button");
  choose.type = "button";
  choose.className = "appearance-editor__choose";
  choose.dataset.action = "choose";
  choose.disabled = state.busy !== null;
  choose.textContent = state.busy?.kind === "choose" ? "正在导入…" : "选择图片";

  const imageNote = document.createElement("p");
  imageNote.className = "appearance-editor__image-note";
  const imageNoteText = document.createTextNode("");
  const digest = document.createElement("code");
  digest.dataset.imageDigest = "";
  imageNote.append(imageNoteText, digest);

  const immersiveField = document.createElement("div");
  immersiveField.className = "appearance-field appearance-field--switch";
  const immersiveLabel = document.createElement("label");
  immersiveLabel.htmlFor = "skin-immersive";
  immersiveLabel.textContent = "启用沉浸模式";
  const immersive = document.createElement("input");
  immersive.id = "skin-immersive";
  immersive.type = "checkbox";
  immersive.checked = state.draft.immersive;
  immersive.dataset.control = "immersive";
  immersiveField.append(immersiveLabel, immersive);

  const fitField = document.createElement("div");
  fitField.className = "appearance-field";
  const fitLabel = document.createElement("label");
  fitLabel.htmlFor = "skin-fit";
  fitLabel.textContent = "填充方式";
  const fit = document.createElement("select");
  fit.id = "skin-fit";
  fit.dataset.control = "fit";
  appendOptions(fit, FIT_LABELS, state.draft.fit);
  fitField.append(fitLabel, fit);

  const positionField = document.createElement("div");
  positionField.className = "appearance-field";
  const positionLabel = document.createElement("label");
  positionLabel.htmlFor = "skin-position";
  positionLabel.textContent = "画面位置";
  const position = document.createElement("select");
  position.id = "skin-position";
  position.dataset.control = "position";
  appendOptions(position, POSITION_LABELS, state.draft.position);
  positionField.append(positionLabel, position);

  const toneField = document.createElement("div");
  toneField.className = "appearance-field";
  const toneLabel = document.createElement("label");
  toneLabel.htmlFor = "skin-tone";
  toneLabel.textContent = "可读性遮罩";
  const tone = document.createElement("select");
  tone.id = "skin-tone";
  tone.dataset.control = "tone";
  appendOptions(tone, [["light", "浅色"], ["dark", "深色"]] satisfies ReadonlyArray<[MaskTone, string]>, state.draft.maskTone);
  toneField.append(toneLabel, tone);

  const status = document.createElement("p");
  status.className = "appearance-editor__status";
  status.setAttribute("role", "status");
  status.setAttribute("aria-live", "polite");
  status.dataset.revision = "";
  status.textContent = state.errorMessage ?? `设置版本 ${state.revision}`;

  const actions = document.createElement("div");
  actions.className = "appearance-editor__actions";
  const reset = document.createElement("button");
  reset.type = "button";
  reset.dataset.action = "reset";
  reset.disabled = state.busy !== null;
  reset.textContent = state.busy?.kind === "reset" ? "正在恢复…" : "恢复默认";
  const save = document.createElement("button");
  save.type = "button";
  save.className = "appearance-editor__save";
  save.dataset.action = "save";
  save.disabled = state.busy !== null;
  save.textContent = state.busy?.kind === "save" ? "正在保存…" : "保存外观";
  actions.append(reset, save);

  editor.append(
    editorTitle,
    choose,
    imageNote,
    immersiveField,
    fitField,
    positionField,
    createSlider("skin-blur", "背景模糊", state.draft.blurPx, 0, 32, "blur"),
    createSlider(
      "skin-glass-blur",
      "毛玻璃强度",
      state.draft.glassBlurPx,
      0,
      32,
      "glass-blur",
    ),
    toneField,
    createSlider("skin-mask", "遮罩强度", state.draft.maskOpacityPercent, 0, 80, "mask"),
    createSlider(
      "skin-image-opacity",
      "图片不透明度",
      state.draft.imageOpacityPercent,
      0,
      100,
      "image-opacity",
    ),
    status,
    actions,
  );
  layout.append(preview, editor);
  page.append(heading, layout);
  root.append(page);
  const refs: AppearanceViewRefs = {
    page,
    preview,
    background,
    content,
    imageNote,
    imageDigest: digest,
    immersive,
    fit,
    position,
    tone,
    blur: root.querySelector<HTMLInputElement>("#skin-blur")!,
    blurOutput: root.querySelector<HTMLOutputElement>("output[for='skin-blur']")!,
    glassBlur: root.querySelector<HTMLInputElement>("#skin-glass-blur")!,
    glassBlurOutput: root.querySelector<HTMLOutputElement>("output[for='skin-glass-blur']")!,
    mask: root.querySelector<HTMLInputElement>("#skin-mask")!,
    maskOutput: root.querySelector<HTMLOutputElement>("output[for='skin-mask']")!,
    imageOpacity: root.querySelector<HTMLInputElement>("#skin-image-opacity")!,
    imageOpacityOutput: root.querySelector<HTMLOutputElement>("output[for='skin-image-opacity']")!,
    status,
    choose,
    save,
    reset,
  };
  appearanceViews.set(root, refs);
  patchAppearanceView(refs, state);
}

function renderUnavailable(root: HTMLElement): void {
  root.replaceChildren();
  const message = document.createElement("main");
  message.className = "appearance appearance--unavailable";
  message.setAttribute("role", "alert");
  const title = document.createElement("strong");
  title.textContent = "暂时无法读取外观设置";
  const description = document.createElement("span");
  description.textContent = "请关闭窗口后重试。";
  message.append(title, description);
  root.append(message);
}

export async function initializeAppearance(
  root: HTMLElement,
  bridge: AppearanceBridge = defaultBridge,
): Promise<() => void> {
  activeDisposers.get(root)?.();
  let state: SkinEditorState | null = null;
  let pendingEvent: SkinStateEnvelopeWire | null = null;
  let unlisten: (() => void) | null = null;
  let disposed = false;
  let handlersMounted = false;
  let operationId = 0;
  const dispatch = (action: Parameters<typeof reduceSkinDraft>[1]): void => {
    if (disposed || state === null) return;
    state = reduceSkinDraft(state, action);
    renderAppearance(root, state);
  };

  const onInput = (event: Event): void => {
    if (!(event.target instanceof HTMLInputElement)) return;
    const value = Number(event.target.value);
    if (event.target.dataset.control === "blur") dispatch({ type: "visuals", blurPx: value });
    if (event.target.dataset.control === "glass-blur") {
      dispatch({ type: "visuals", glassBlurPx: value });
    }
    if (event.target.dataset.control === "mask") dispatch({ type: "visuals", maskOpacityPercent: value });
    if (event.target.dataset.control === "image-opacity") {
      dispatch({ type: "visuals", imageOpacityPercent: value });
    }
  };

  const onChange = (event: Event): void => {
    if (!(event.target instanceof HTMLInputElement || event.target instanceof HTMLSelectElement)) return;
    const control = event.target.dataset.control;
    if (control === "immersive" && event.target instanceof HTMLInputElement) {
      dispatch({ type: "immersive", value: event.target.checked });
    }
    if (control === "fit") dispatch({ type: "fit", value: event.target.value as SkinFit });
    if (control === "position") dispatch({ type: "position", value: event.target.value as SkinPosition });
    if (control === "tone") dispatch({ type: "tone", value: event.target.value as MaskTone });
  };

  const startOperation = (
    kind: NonNullable<SkinEditorState["busy"]>["kind"],
  ): number => {
    operationId += 1;
    dispatch({ type: "operation-start", kind, id: operationId });
    return operationId;
  };

  const onClick = (event: Event): void => {
    if (!(event.target instanceof Element) || state === null || state.busy !== null) return;
    const button = event.target.closest<HTMLButtonElement>("button[data-action]");
    if (button === null || !root.contains(button)) return;
    const action = button.dataset.action;
    if (action === "choose") {
      const id = startOperation("choose");
      void bridge.invoke("choose_skin_image")
        .then((result) => dispatch({ type: "operation-image", id, image: result as SkinImageView | null }))
        .catch(() => dispatch({ type: "operation-failed", id, message: "无法导入这张图片，请检查格式和尺寸" }));
    }
    if (action === "save") {
      const submitted = state;
      const id = startOperation("save");
      void bridge.invoke("save_skin_settings", {
        expectedRevision: submitted.revision,
        draft: skinDraftToWire(submitted.draft),
      }).then((result) => dispatch({ type: "operation-envelope", id, envelope: result as SkinStateEnvelopeWire }))
        .catch(() => dispatch({ type: "operation-failed", id, message: "保存失败，请重新读取设置后再试" }));
    }
    if (action === "reset" && bridge.confirm(RESTORE_CONFIRMATION)) {
      const expectedRevision = state.revision;
      const id = startOperation("reset");
      void bridge.invoke("reset_skin_settings", { expectedRevision })
        .then((result) => dispatch({ type: "operation-envelope", id, envelope: result as SkinStateEnvelopeWire }))
        .catch(() => dispatch({ type: "operation-failed", id, message: "恢复默认失败，请稍后再试" }));
    }
  };

  const dispose = (): void => {
    if (disposed) return;
    disposed = true;
    unlisten?.();
    unlisten = null;
    if (handlersMounted) {
      root.removeEventListener("input", onInput);
      root.removeEventListener("change", onChange);
      root.removeEventListener("click", onClick);
      window.removeEventListener("pagehide", dispose);
      handlersMounted = false;
    }
    if (activeDisposers.get(root) === dispose) activeDisposers.delete(root);
  };
  // 初始化开始即登记，确保第二次 mount 能取消仍在等待 bridge 的旧实例。
  activeDisposers.set(root, dispose);

  try {
    // 先订阅再取快照，避免设置事件恰好发生在初始化窗口中而丢失。
    const registeredUnlisten = await bridge.listen("skin-state", ({ payload }) => {
      if (state === null) {
        if (pendingEvent === null || payload.revision > pendingEvent.revision) pendingEvent = payload;
      }
      else dispatch({ type: "state-received", envelope: payload });
    });
    if (disposed) {
      registeredUnlisten();
      return dispose;
    }
    unlisten = registeredUnlisten;
    const snapshot = (await bridge.invoke("get_skin_state")) as SkinStateEnvelopeWire;
    if (disposed) return dispose;
    state = createInitialSkinState(snapshot);
    if (pendingEvent !== null) {
      state = reduceSkinDraft(state, {
        type: "state-received",
        envelope: pendingEvent,
      });
    }
    renderAppearance(root, state);
  } catch {
    if (!disposed) {
      dispose();
      renderUnavailable(root);
    }
    return dispose;
  }

  root.addEventListener("input", onInput);
  root.addEventListener("change", onChange);
  root.addEventListener("click", onClick);
  window.addEventListener("pagehide", dispose);
  handlersMounted = true;
  return dispose;
}
