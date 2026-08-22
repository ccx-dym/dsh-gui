import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";

const reviewer = path.resolve(import.meta.dirname, "../review-runtime-lock.mjs");
const approvedPath = path.resolve(
  import.meta.dirname,
  "../../runtime/locks/dsh-0.1.1-rc.1/install-scripts.json",
);

test("审核脚本从 exact lock 输出完整的 install-script 候选清单", async () => {
  const result = spawnSync(process.execPath, [reviewer, "0.1.1-rc.1"], {
    cwd: path.resolve(import.meta.dirname, "../.."),
    encoding: "utf8",
  });

  assert.equal(result.status, 0, result.stderr);
  const actual = JSON.parse(result.stdout);
  const approved = JSON.parse(await readFile(approvedPath, "utf8"));
  assert.deepEqual(actual, approved);
});

test("审核脚本拒绝包含空标识符或非法连字符的 prerelease 版本", () => {
  for (const version of ["0.1.1-rc..2", "0.1.1-rc.", "0.1.1-rc-2"]) {
    const result = spawnSync(process.execPath, [reviewer, version], {
      cwd: path.resolve(import.meta.dirname, "../.."),
      encoding: "utf8",
    });

    assert.notEqual(result.status, 0, `${version} 不得被接受`);
    assert.match(result.stderr, /用法:/, `${version} 应输出明确用法`);
    assert.equal(result.stdout, "", `${version} 不得输出审核清单`);
  }
});
