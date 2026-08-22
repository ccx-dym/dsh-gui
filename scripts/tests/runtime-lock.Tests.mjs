import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";

for (const version of ["0.1.1-rc.1", "0.1.1-rc.2"]) {
  const lockPath = path.resolve(
    import.meta.dirname,
    `../../runtime/locks/dsh-${version}/package-lock.json`,
  );
  const lock = JSON.parse(await readFile(lockPath, "utf8"));
  const packages = lock.packages;
  const allowlistPath = path.join(path.dirname(lockPath), "install-scripts.json");
  const allowlist = JSON.parse(await readFile(allowlistPath, "utf8"));

  assert.equal(lock.lockfileVersion, 3);
  assert.equal(packages[""].dependencies["@deepseek-ai/dsh"], version);
  assert.equal(packages["node_modules/@deepseek-ai/dsh"].version, version);

  const installedNames = new Set(
    Object.entries(packages)
      .filter(([packagePath]) => packagePath !== "")
      .map(([packagePath, metadata]) => metadata.name ?? packagePath.slice(packagePath.lastIndexOf("node_modules/") + 13)),
  );
  assert.ok(
    installedNames.has("@deepseek-ai/cordis-plugin-group"),
    `官方 DSH ${version} CLI import closure 必须包含 dsh-app-boot 的 cordis-plugin-group peer`,
  );
  const missing = [];
  for (const [packagePath, metadata] of Object.entries(packages)) {
    for (const dependency of Object.keys(metadata.dependencies ?? {})) {
      if (!installedNames.has(dependency)) {
        missing.push(`${packagePath || "<root>"} -> ${dependency}`);
      }
    }
  }
  assert.deepEqual(missing, [], `DSH ${version} lock 缺少 required dependency 条目:\n${missing.join("\n")}`);

  const missingRequiredPeers = [];
  for (const [packagePath, metadata] of Object.entries(packages)) {
    for (const peer of Object.keys(metadata.peerDependencies ?? {})) {
      if (metadata.peerDependenciesMeta?.[peer]?.optional === true) continue;
      if (!installedNames.has(peer)) {
        missingRequiredPeers.push(`${packagePath} -> peer ${peer}`);
      }
    }
  }
  assert.deepEqual(
    missingRequiredPeers,
    [],
    `DSH ${version} lock 缺少 non-optional peer runtime closure:\n${missingRequiredPeers.join("\n")}`,
  );

  const actualInstallScripts = Object.entries(packages)
    .filter(([, metadata]) => metadata.hasInstallScript === true)
    .map(([packagePath, metadata]) => ({
      path: packagePath,
      name: metadata.name ?? packagePath.slice(packagePath.lastIndexOf("node_modules/") + 13),
      version: metadata.version,
      integrity: metadata.integrity,
    }))
    .sort((left, right) => left.path.localeCompare(right.path));
  const approvedInstallScripts = [...allowlist.packages]
    .sort((left, right) => left.path.localeCompare(right.path));
  assert.equal(allowlist.schema, 1);
  assert.equal(approvedInstallScripts.length, 5);
  assert.deepEqual(approvedInstallScripts, actualInstallScripts);

  console.log(`DSH ${version} runtime lock closure passed: ${Object.keys(packages).length} package entries, ${actualInstallScripts.length} install scripts`);
}
