import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import process from "node:process";

const EXACT_SEMVER =
  /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|\d*[A-Za-z-][0-9A-Za-z-]*))*)?$/;
const UTC_SECONDS = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2}):(\d{2})Z$/;
const MAX_COMPATIBILITY_SUMMARY_CHARS = 512;

function assertExactSemver(value, field) {
  if (typeof value !== "string" || !EXACT_SEMVER.test(value)) {
    throw new Error(`${field} 必须是 exact semver`);
  }
}

function assertArtifactUrl(value) {
  let parsed;
  try {
    parsed = new URL(value);
  } catch {
    throw new Error("artifactUrl 必须是有效 URL");
  }
  if (
    parsed.protocol !== "https:" ||
    parsed.username ||
    parsed.password ||
    parsed.search ||
    parsed.hash
  ) {
    throw new Error("artifactUrl 必须使用无凭据、查询或片段的 HTTPS URL");
  }
  return parsed.href;
}

function assertVerifiedAt(value) {
  const match = typeof value === "string" ? UTC_SECONDS.exec(value) : null;
  if (!match || match[1] === "0000") {
    throw new Error("verifiedAt 必须是有效的秒精度 UTC 时间");
  }
  const milliseconds = Date.parse(value);
  if (
    !Number.isFinite(milliseconds) ||
    new Date(milliseconds).toISOString().replace(".000Z", "Z") !== value
  ) {
    throw new Error("verifiedAt 必须是有效的秒精度 UTC 时间");
  }
}

function assertCompatibilitySummary(value) {
  const length = typeof value === "string" ? [...value].length : 0;
  if (
    typeof value !== "string" ||
    value.trim().length === 0 ||
    length > MAX_COMPATIBILITY_SUMMARY_CHARS ||
    /[\p{Cc}\p{Cs}]/u.test(value)
  ) {
    throw new Error("compatibilitySummary 必须为 1 到 512 个无控制字符的字符");
  }
}

/**
 * 从已生成的 runtime ZIP 创建发布清单，制品大小与摘要始终从本地 bytes 计算。
 *
 * @param {object} input 清单输入。
 * @param {string} input.zipPath 本地 runtime ZIP 路径。
 * @param {string} input.dshVersion exact DSH semver。
 * @param {string} input.nodeVersion exact Node.js semver。
 * @param {string} input.minimumDesktopVersion 最低桌面端 exact semver。
 * @param {string} input.artifactUrl 不含凭据、查询或片段的 HTTPS 制品地址。
 * @param {string} input.verifiedAt 秒精度 UTC 验证时间。
 * @param {string} input.compatibilitySummary 1 到 512 字符的兼容性摘要。
 * @return {Promise<object>} 可直接序列化的 runtime manifest v1。
 * @raises {Error} 输入无效或 ZIP 无法读取时抛出。
 */
export async function createRuntimeManifest(input) {
  if (!input || typeof input !== "object") {
    throw new Error("manifest 输入不能为空");
  }
  assertExactSemver(input.dshVersion, "dshVersion");
  assertExactSemver(input.nodeVersion, "nodeVersion");
  assertExactSemver(input.minimumDesktopVersion, "minimumDesktopVersion");
  const artifactUrl = assertArtifactUrl(input.artifactUrl);
  assertVerifiedAt(input.verifiedAt);
  assertCompatibilitySummary(input.compatibilitySummary);
  if (typeof input.zipPath !== "string" || input.zipPath.length === 0) {
    throw new Error("zipPath 不能为空");
  }

  // 只信任实际发布 bytes，不允许调用方手填 size 或 digest，避免清单与 ZIP 漂移。
  const bytes = await readFile(input.zipPath);
  if (bytes.length === 0) {
    throw new Error("zipPath 指向的制品不能为空");
  }

  return {
    schema: 1,
    dsh_version: input.dshVersion,
    node_version: input.nodeVersion,
    minimum_desktop_version: input.minimumDesktopVersion,
    platform: "windows",
    arch: "x86_64",
    artifact: {
      url: artifactUrl,
      size: bytes.length,
      sha256: createHash("sha256").update(bytes).digest("hex"),
    },
    verified_at: input.verifiedAt,
    compatibility_summary: input.compatibilitySummary,
  };
}

function parseCliArguments(arguments_) {
  const names = new Map([
    ["--zip", "zipPath"],
    ["--dsh-version", "dshVersion"],
    ["--node-version", "nodeVersion"],
    ["--minimum-desktop-version", "minimumDesktopVersion"],
    ["--artifact-url", "artifactUrl"],
    ["--verified-at", "verifiedAt"],
    ["--compatibility-summary", "compatibilitySummary"],
    ["--output", "outputPath"],
  ]);
  const parsed = {};
  for (let index = 0; index < arguments_.length; index += 2) {
    const name = arguments_[index];
    const field = names.get(name);
    const value = arguments_[index + 1];
    if (!field || value === undefined || value.startsWith("--") || field in parsed) {
      throw new Error("CLI 参数无效或不完整");
    }
    parsed[field] = value;
  }
  if (Object.keys(parsed).length !== names.size) {
    throw new Error("CLI 缺少必填参数");
  }
  return parsed;
}

async function runCli() {
  try {
    const { outputPath, ...input } = parseCliArguments(process.argv.slice(2));
    const manifest = await createRuntimeManifest(input);
    // 固定属性顺序、无缩进且仅一个 LF；签名端直接签署这些原始 UTF-8 bytes。
    await writeFile(outputPath, `${JSON.stringify(manifest)}\n`, {
      encoding: "utf8",
      flag: "wx",
    });
    console.log("runtime manifest written");
  } catch (error) {
    console.error(error instanceof Error ? error.message : "runtime manifest 生成失败");
    process.exitCode = 1;
  }
}

if (process.argv[1] && fileURLToPath(import.meta.url) === process.argv[1]) {
  await runCli();
}
