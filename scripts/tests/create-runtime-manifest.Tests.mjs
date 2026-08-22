import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import test from "node:test";
import Ajv2020 from "ajv/dist/2020.js";
import addFormats from "ajv-formats";

import { createRuntimeManifest } from "../create-runtime-manifest.mjs";

const repositoryRoot = path.resolve(import.meta.dirname, "../..");
const generator = path.join(repositoryRoot, "scripts/create-runtime-manifest.mjs");
const temporaryRoot = await mkdtemp(path.join(tmpdir(), "dsh-runtime-manifest-test-"));
const fixture = path.join(temporaryRoot, "runtime.zip");
await writeFile(fixture, "fixture", "utf8");

const validInput = {
  zipPath: fixture,
  dshVersion: "0.1.1-rc.2",
  nodeVersion: "24.15.0",
  minimumDesktopVersion: "0.1.0",
  coreCompatibility: "compatible",
  skinCompatibility: "unverified",
  artifactUrl:
    "https://github.com/ccx-dym/dsh-gui/releases/download/dsh-v0.1.1-rc.2-windows/dsh-runtime-0.1.1-rc.2-node-24.15.0-win-x64.zip",
  verifiedAt: "2026-08-22T00:00:00Z",
  compatibilitySummary: "Windows 10/11 x64 核心兼容验证通过；皮肤未验证时自动关闭。",
};

test("manifest 从真实 ZIP 计算 size 与 sha256", async () => {
  const manifest = await createRuntimeManifest(validInput);

  assert.deepEqual(manifest, {
    schema: 2,
    dsh_version: "0.1.1-rc.2",
    node_version: "24.15.0",
    minimum_desktop_version: "0.1.0",
    core_compatibility: "compatible",
    skin_compatibility: "unverified",
    platform: "windows",
    arch: "x86_64",
    artifact: {
      url: validInput.artifactUrl,
      size: 7,
      sha256: "f16d05ec6b29248d2c61adb1e9263f78e4f7bace1b955014a2d17872cfe4064d",
    },
    verified_at: "2026-08-22T00:00:00Z",
    compatibility_summary: "Windows 10/11 x64 核心兼容验证通过；皮肤未验证时自动关闭。",
  });
});

test("JSON Schema 接受 generator v2 输出与受支持的旧 v1", async () => {
  const schema = JSON.parse(
    await readFile(path.join(repositoryRoot, "runtime/manifest.schema.json"), "utf8"),
  );
  const ajv = new Ajv2020({ allErrors: true, strict: true });
  addFormats(ajv);
  const validate = ajv.compile(schema);

  assert.equal(validate(await createRuntimeManifest(validInput)), true, JSON.stringify(validate.errors));
  const legacy = JSON.parse(
    await readFile(
      path.join(repositoryRoot, "src-tauri/tests/fixtures/runtime-manifest/valid.json"),
      "utf8",
    ),
  );
  assert.equal(validate(legacy), true, JSON.stringify(validate.errors));
});

test("JSON Schema 拒绝 v2 缺少兼容字段或包含非法枚举", async () => {
  const schema = JSON.parse(
    await readFile(path.join(repositoryRoot, "runtime/manifest.schema.json"), "utf8"),
  );
  const ajv = new Ajv2020({ allErrors: true, strict: true });
  addFormats(ajv);
  const validate = ajv.compile(schema);
  const generated = await createRuntimeManifest(validInput);
  const { core_compatibility: _missing, ...withoutCore } = generated;

  assert.equal(validate(withoutCore), false);
  assert.equal(validate({ ...generated, skin_compatibility: "maybe" }), false);
});

test("manifest 只接受 exact semver", async () => {
  for (const [field, value] of [
    ["dshVersion", "latest"],
    ["dshVersion", "0.1"],
    ["dshVersion", "v0.1.1"],
    ["dshVersion", "0.1.01"],
    ["dshVersion", "0.1.1-01"],
    ["dshVersion", "0.1.1 || 1.0.0"],
    ["nodeVersion", "24.x"],
    ["minimumDesktopVersion", "^0.1.0"],
  ]) {
    await assert.rejects(
      createRuntimeManifest({ ...validInput, [field]: value }),
      new RegExp(field),
    );
  }
});

test("manifest 只接受明确的核心和皮肤兼容枚举", async () => {
  for (const [field, value] of [
    ["coreCompatibility", "maybe"],
    ["skinCompatibility", "compatible"],
  ]) {
    await assert.rejects(
      createRuntimeManifest({ ...validInput, [field]: value }),
      new RegExp(field),
    );
  }
});

test("manifest 拒绝非 HTTPS 或带凭据、查询和片段的制品 URL", async () => {
  for (const artifactUrl of [
    "http://downloads.example.com/runtime.zip",
    "https://user:pass@downloads.example.com/runtime.zip",
    "https://downloads.example.com/runtime.zip?token=secret",
    "https://downloads.example.com/runtime.zip#fragment",
  ]) {
    await assert.rejects(
      createRuntimeManifest({ ...validInput, artifactUrl }),
      /artifactUrl/,
    );
  }
});

test("manifest 只接受有效的秒精度 UTC verifiedAt", async () => {
  for (const verifiedAt of [
    "2026-08-22",
    "2026-08-22T08:00:00+08:00",
    "2026-08-22T00:00:00.000Z",
    "2026-02-30T00:00:00Z",
  ]) {
    await assert.rejects(
      createRuntimeManifest({ ...validInput, verifiedAt }),
      /verifiedAt/,
    );
  }
});

test("manifest 拒绝空白、控制字符或超过 512 字符的兼容摘要", async () => {
  for (const compatibilitySummary of ["   ", "可用\n未验证", "兼".repeat(513)]) {
    await assert.rejects(
      createRuntimeManifest({ ...validInput, compatibilitySummary }),
      /compatibilitySummary/,
    );
  }
});

test("manifest 拒绝孤立 Unicode 代理项但允许合法 emoji", async () => {
  for (const compatibilitySummary of ["损坏\uD800摘要", "损坏\uDC00摘要"]) {
    await assert.rejects(
      createRuntimeManifest({ ...validInput, compatibilitySummary }),
      /compatibilitySummary/,
    );
  }

  const manifest = await createRuntimeManifest({
    ...validInput,
    compatibilitySummary: "Windows x64 验证通过 🐋",
  });
  assert.equal(manifest.compatibility_summary, "Windows x64 验证通过 🐋");
});

test("CLI 以 canonical UTF-8 JSON 加单个换行写入新文件", async () => {
  const outputPath = path.join(temporaryRoot, "manifest.json");
  const result = spawnSync(
    process.execPath,
    [
      generator,
      "--zip",
      fixture,
      "--dsh-version",
      validInput.dshVersion,
      "--node-version",
      validInput.nodeVersion,
      "--minimum-desktop-version",
      validInput.minimumDesktopVersion,
      "--core-compatibility",
      validInput.coreCompatibility,
      "--skin-compatibility",
      validInput.skinCompatibility,
      "--artifact-url",
      validInput.artifactUrl,
      "--verified-at",
      validInput.verifiedAt,
      "--compatibility-summary",
      validInput.compatibilitySummary,
      "--output",
      outputPath,
    ],
    { cwd: repositoryRoot, encoding: "utf8" },
  );

  assert.equal(result.status, 0, `${result.stdout}${result.stderr}`);
  const bytes = await readFile(outputPath);
  const expected =
    '{"schema":2,"dsh_version":"0.1.1-rc.2","node_version":"24.15.0","minimum_desktop_version":"0.1.0","core_compatibility":"compatible","skin_compatibility":"unverified","platform":"windows","arch":"x86_64","artifact":{"url":"https://github.com/ccx-dym/dsh-gui/releases/download/dsh-v0.1.1-rc.2-windows/dsh-runtime-0.1.1-rc.2-node-24.15.0-win-x64.zip","size":7,"sha256":"f16d05ec6b29248d2c61adb1e9263f78e4f7bace1b955014a2d17872cfe4064d"},"verified_at":"2026-08-22T00:00:00Z","compatibility_summary":"Windows 10/11 x64 核心兼容验证通过；皮肤未验证时自动关闭。"}\n';
  assert.equal(bytes.toString("utf8"), expected);
  assert.equal(bytes.at(-1), 0x0a);
  assert.notEqual(bytes.at(-2), 0x0a);
});

test("CLI 拒绝覆盖既有 manifest 且原 bytes 完全不变", async () => {
  const outputPath = path.join(temporaryRoot, "immutable-manifest.json");
  const originalBytes = Buffer.from([0x64, 0x6f, 0x20, 0x6e, 0x6f, 0x74, 0x20, 0xff]);
  await writeFile(outputPath, originalBytes);

  const result = spawnSync(
    process.execPath,
    [
      generator,
      "--zip",
      fixture,
      "--dsh-version",
      validInput.dshVersion,
      "--node-version",
      validInput.nodeVersion,
      "--minimum-desktop-version",
      validInput.minimumDesktopVersion,
      "--core-compatibility",
      validInput.coreCompatibility,
      "--skin-compatibility",
      validInput.skinCompatibility,
      "--artifact-url",
      validInput.artifactUrl,
      "--verified-at",
      validInput.verifiedAt,
      "--compatibility-summary",
      validInput.compatibilitySummary,
      "--output",
      outputPath,
    ],
    { cwd: repositoryRoot, encoding: "utf8" },
  );

  assert.notEqual(result.status, 0);
  assert.deepEqual(await readFile(outputPath), originalBytes);
});
