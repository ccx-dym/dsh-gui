import { readFile } from "node:fs/promises";
import path from "node:path";

const [version] = process.argv.slice(2);
if (!/^\d+\.\d+\.\d+(?:-[0-9A-Za-z]+(?:\.[0-9A-Za-z]+)*)?$/.test(version ?? "")) {
  throw new Error("用法: node scripts/review-runtime-lock.mjs <exact-dsh-version>");
}

// 路径只由已校验的精确版本组成，避免审核工具读取 runtime lock 目录之外的文件。
const lockPath = path.resolve(
  import.meta.dirname,
  `../runtime/locks/dsh-${version}/package-lock.json`,
);
const lock = JSON.parse(await readFile(lockPath, "utf8"));

// 只列出 npm 明确标记为会执行安装脚本的包；该输出必须经人工逐项审核后才可成为 allowlist。
const packages = Object.entries(lock.packages ?? {})
  .filter(([, metadata]) => metadata.hasInstallScript === true)
  .map(([packagePath, metadata]) => ({
    path: packagePath,
    name: metadata.name ?? packagePath.slice(packagePath.lastIndexOf("node_modules/") + 13),
    version: metadata.version,
    integrity: metadata.integrity,
  }))
  .sort((left, right) => left.path.localeCompare(right.path));

process.stdout.write(`${JSON.stringify({ schema: 1, packages }, null, 2)}\n`);
