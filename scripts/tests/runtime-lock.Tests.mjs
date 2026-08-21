import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";

const lockPath = path.resolve(
  import.meta.dirname,
  "../../runtime/locks/dsh-0.1.1-rc.1/package-lock.json",
);
const lock = JSON.parse(await readFile(lockPath, "utf8"));
const packages = lock.packages;

assert.equal(lock.lockfileVersion, 3);
assert.equal(packages[""].dependencies["@deepseek-ai/dsh"], "0.1.1-rc.1");
assert.equal(packages["node_modules/@deepseek-ai/dsh"].version, "0.1.1-rc.1");

const installedNames = new Set(
  Object.entries(packages)
    .filter(([packagePath]) => packagePath !== "")
    .map(([packagePath, metadata]) => metadata.name ?? packagePath.slice(packagePath.lastIndexOf("node_modules/") + 13)),
);
const missing = [];
for (const [packagePath, metadata] of Object.entries(packages)) {
  for (const dependency of Object.keys(metadata.dependencies ?? {})) {
    if (!installedNames.has(dependency)) {
      missing.push(`${packagePath || "<root>"} -> ${dependency}`);
    }
  }
}
assert.deepEqual(missing, [], `lock 缺少 required dependency 条目:\n${missing.join("\n")}`);

console.log(`runtime lock closure passed: ${Object.keys(packages).length} package entries`);
