import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";

const repositoryRoot = path.resolve(import.meta.dirname, "../..");

function parseScalar(source) {
  const value = source.trim();
  if (value === "") return {};
  if (value === "true") return true;
  if (value === "false") return false;
  if (value === "null") return null;
  if ((value.startsWith('"') && value.endsWith('"')) ||
      (value.startsWith("'") && value.endsWith("'"))) {
    return value.slice(1, -1);
  }
  return value;
}

function parseWorkflowYaml(source) {
  const lines = source.replaceAll("\r\n", "\n").split("\n");
  const indentation = (line) => line.match(/^ */)[0].length;
  const nextContentLine = (start) => {
    let index = start;
    while (index < lines.length && /^\s*(?:#.*)?$/.test(lines[index])) index += 1;
    return index;
  };
  const parsePair = (content, lineNumber) => {
    const separator = content.indexOf(":");
    if (separator < 1) throw new Error(`无效 YAML mapping: ${lineNumber}`);
    return [
      content.slice(0, separator).trim().replace(/^["']|["']$/g, ""),
      content.slice(separator + 1).trim(),
    ];
  };

  function parseBlock(start, expectedIndent) {
    let index = nextContentLine(start);
    const sequence = lines[index]?.slice(expectedIndent).startsWith("- ") ?? false;
    const value = sequence ? [] : {};
    while (index < lines.length) {
      index = nextContentLine(index);
      if (index >= lines.length || indentation(lines[index]) < expectedIndent) break;
      if (indentation(lines[index]) > expectedIndent) {
        throw new Error(`意外 YAML 缩进: ${index + 1}`);
      }
      const content = lines[index].slice(expectedIndent);
      if (sequence) {
        if (!content.startsWith("- ")) break;
        const item = {};
        const [key, scalar] = parsePair(content.slice(2), index + 1);
        item[key] = parseScalar(scalar);
        index += 1;
        const childStart = nextContentLine(index);
        if (childStart < lines.length && indentation(lines[childStart]) > expectedIndent) {
          const [remainder, nextIndex] = parseBlock(childStart, expectedIndent + 2);
          Object.assign(item, remainder);
          index = nextIndex;
        }
        value.push(item);
        continue;
      }

      const [key, scalar] = parsePair(content, index + 1);
      if (scalar === "|") {
        const block = [];
        index += 1;
        while (index < lines.length &&
               (!lines[index].trim() || indentation(lines[index]) > expectedIndent)) {
          block.push(lines[index].slice(Math.min(lines[index].length, expectedIndent + 2)));
          index += 1;
        }
        value[key] = `${block.join("\n").replace(/\n+$/, "")}\n`;
      } else if (scalar === "") {
        const childStart = nextContentLine(index + 1);
        if (childStart >= lines.length || indentation(lines[childStart]) <= expectedIndent) {
          value[key] = {};
          index += 1;
        } else {
          [value[key], index] = parseBlock(childStart, indentation(lines[childStart]));
        }
      } else {
        value[key] = parseScalar(scalar);
        index += 1;
      }
    }
    return [value, index];
  }

  return parseBlock(0, 0)[0];
}

async function loadWorkflow(relativePath) {
  const source = await readFile(path.join(repositoryRoot, relativePath), "utf8");
  return parseWorkflowYaml(source);
}

function stepsById(job) {
  assert.ok(Array.isArray(job.steps), "job 必须声明 steps 序列");
  return Object.fromEntries(job.steps.map((step) => [step.id, step]));
}

const APPROVED_ACTIONS = new Map([
  ["actions/checkout", "11bd71901bbe5b1630ceea73d27597364c9af683"],
  ["actions/upload-artifact", "ea165f8d65b6e75b540449e92b4886f43607fa02"],
  ["actions/download-artifact", "d3f86a106a0bac45b974a628896c90dbdf5c8093"],
]);

function assertPinnedAction(action) {
  const [name, revision] = action.split("@");
  assert.equal(
    revision,
    APPROVED_ACTIONS.get(name),
    `GitHub Action 必须匹配批准的官方 commit: ${action}`,
  );
}

test("runtime 发布仅在审批环境中获得 contents:write", async () => {
  const workflow = await loadWorkflow(".github/workflows/build-runtime.yml");
  assert.equal(workflow.permissions.contents, "read");
  assert.equal(workflow.jobs.publish.environment, "runtime-release");
  assert.equal(workflow.jobs.publish.permissions.contents, "write");
  assert.equal(workflow.jobs.build.permissions.contents, "read");
  assert.notEqual(workflow.jobs.build.environment, "runtime-release");
});

test("workflow 只接受 exact DSH 版本并固定 Windows 与 Node 构建输入", async () => {
  const workflow = await loadWorkflow(".github/workflows/build-runtime.yml");
  const input = workflow.on.workflow_dispatch.inputs.dsh_version;
  assert.equal(input.required, true);
  assert.equal(input.default, "0.1.1-rc.2");
  assert.equal(workflow.jobs.build["runs-on"], "windows-2025");
  assert.equal(workflow.jobs.build.env.NODE_VERSION, "24.15.0");
  assert.equal(
    workflow.jobs.build.env.NODE_SHA256,
    "cc5149eabd53779ce1e7bdc5401643622d0c7e6800ade18928a767e940bb0e62",
  );

  const steps = stepsById(workflow.jobs.build);
  assert.match(steps.validate.run, /exact semver/);
  assert.match(steps.validate.run, /git ls-files --error-unmatch/);
  assert.match(steps.node.run, /SHASUMS256\.txt/);
  assert.match(steps.node.run, /Get-FileHash/);
  assert.match(steps.node.run, /Node archive SHA-256/);
  assert.match(steps.node.run, /-cne \$env:NODE_SHA256/);
});

test("dispatch 只构建默认分支当前 SHA 且 publish 沿用同一 source_sha", async () => {
  const workflow = await loadWorkflow(".github/workflows/build-runtime.yml");
  const build = stepsById(workflow.jobs.build);
  const publish = stepsById(workflow.jobs.publish);

  assert.equal(build.checkout.with.ref, "${{ github.sha }}");
  assert.equal(build.validate.env.DISPATCH_REF, "${{ github.ref }}");
  assert.equal(
    build.validate.env.DEFAULT_BRANCH,
    "${{ github.event.repository.default_branch }}",
  );
  assert.match(build.validate.run, /refs\/heads\/\$env:DEFAULT_BRANCH/);
  assert.match(build.validate.run, /\$env:DISPATCH_REF -cne \$expectedRef/);
  assert.equal(workflow.jobs.build.outputs.source_sha, "${{ steps.validate.outputs.source_sha }}");
  assert.equal(publish.checkout.with.ref, "${{ needs.build.outputs.source_sha }}");
  assert.match(publish.release.run, /--target \$env:SOURCE_SHA/);
});

test("build 运行真实构建门禁并通过固定 SHA action 跨 job 传递候选物", async () => {
  const workflow = await loadWorkflow(".github/workflows/build-runtime.yml");
  const build = stepsById(workflow.jobs.build);
  const publish = stepsById(workflow.jobs.publish);

  const actionSteps = [...workflow.jobs.build.steps, ...workflow.jobs.publish.steps]
    .filter((step) => step.uses);
  assert.equal(actionSteps.length, 4);
  for (const step of actionSteps) {
    assertPinnedAction(step.uses);
  }
  assert.match(build.runtime.run, /scripts\/build-runtime\.ps1/);
  assert.match(build.runtime.run, /scripts\/smoke-runtime\.ps1/);
  assert.match(build.runtime.run, /scripts\/create-runtime-manifest\.mjs/);
  assert.match(build.runtime.run, /git show -s --format=%cI \$env:SOURCE_SHA/);
  assert.match(build.runtime.run, /DateTimeOffset\]::Parse/);
  assert.doesNotMatch(build.runtime.run, /UtcNow/);
  assert.match(build.runtime.run, /inventory\.json/);
  assert.match(build.runtime.run, /THIRD_PARTY_NOTICES\.json/);
  assert.equal(build.upload.with.name, "runtime-release-candidate");
  assert.equal(publish.download.with.name, "runtime-release-candidate");
  assert.equal(workflow.jobs.publish.needs, "build");
});

test("publish 不回显私钥、拒绝已有 tag 并用分支 PR 更新稳定通道", async () => {
  const workflow = await loadWorkflow(".github/workflows/build-runtime.yml");
  const publish = stepsById(workflow.jobs.publish);
  const allRuns = Object.values(publish)
    .map((step) => step.run ?? "")
    .join("\n");

  assert.equal(publish.sign.env.DSH_RUNTIME_SIGNING_KEY, "${{ secrets.DSH_RUNTIME_SIGNING_KEY }}");
  assert.equal(
    publish.sign.env.DSH_RUNTIME_SIGNING_KEY_PASSWORD,
    "${{ secrets.DSH_RUNTIME_SIGNING_KEY_PASSWORD }}",
  );
  assert.match(publish.sign.run, /scripts\/sign-runtime\.mjs/);
  assert.doesNotMatch(allRuns, /Write-(?:Host|Output).*DSH_RUNTIME_SIGNING_KEY/i);
  const stepIds = workflow.jobs.publish.steps.map((step) => step.id);
  assert.ok(stepIds.indexOf("channel") < stepIds.indexOf("release"));
  assert.match(publish.release.run, /git\/ref\/tags\/\$tag/);
  assert.match(publish.release.run, /Invoke-WebRequest -Uri \$tagUri/);
  assert.match(publish.release.run, /StatusCode .*NotFound/);
  assert.match(publish.release.run, /throw .*tag.*已存在/i);
  assert.match(publish.release.run, /gh release create/);
  assert.match(publish.release.run, /THIRD_PARTY_NOTICES\.json/);
  assert.match(publish.release.run, /manifest\.sig/);
  assert.doesNotMatch(publish.release.run, /runtime-release-candidate\/\*/);
  assert.match(publish.channel.run, /scripts\/publish-runtime-channel\.ps1/);
  assert.doesNotMatch(publish.channel.run, /git push\s+origin\s+(?:main|master)(?:\s|$)/i);

  const channelScript = await readFile(
    path.join(repositoryRoot, "scripts/publish-runtime-channel.ps1"),
    "utf8",
  );
  assert.match(channelScript, /--json number,baseRefName,headRefName/);
});

test("每个调用 native git、gh 或 node 的 PowerShell 步骤都失败关闭", async () => {
  const workflow = await loadWorkflow(".github/workflows/build-runtime.yml");
  for (const job of Object.values(workflow.jobs)) {
    for (const step of job.steps) {
      if (step.run && /(?:^|\s)(?:git|gh|node)(?:\s|$)/m.test(step.run)) {
        assert.match(
          step.run,
          /\$PSNativeCommandUseErrorActionPreference\s*=\s*\$true/,
          `${step.id} 必须让 native command 非零退出立即失败`,
        );
      }
    }
  }
});
