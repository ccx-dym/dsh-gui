import assert from "node:assert/strict";
import test from "node:test";

import {
  createCandidateIssue,
  discoverCandidate,
  fetchOfficialRegistry,
  readOpenCandidateVersions,
  readPublishedVersions,
} from "../scan-dsh-upstream.mjs";

const fixtureIntegrity =
  "sha512-UP1UIh6q3Gme/yXRn/QL2P8IsVlv8Shpg22TRJIZPsCRWLm4CBiA1MUvXmJAfsOEETBMLAl+xWPtFw6ICsN3wg==";
const fixtureTarball =
  "https://registry.npmjs.org/@deepseek-ai/dsh/-/dsh-0.1.1-rc.2.tgz";

function registryFixture(overrides = {}) {
  return {
    "dist-tags": { latest: "0.1.1-rc.2" },
    versions: {
      "0.1.1-rc.2": {
        version: "0.1.1-rc.2",
        dist: {
          integrity: fixtureIntegrity,
          tarball: fixtureTarball,
        },
      },
    },
    ...overrides,
  };
}

test("发现未锁定且未建 issue 的 exact latest", () => {
  const result = discoverCandidate({
    registry: registryFixture(),
    knownVersions: new Set(["0.1.1-rc.1"]),
    openCandidateVersions: new Set(),
  });

  assert.deepEqual(result, {
    status: "candidate",
    version: "0.1.1-rc.2",
    integrity: fixtureIntegrity,
    tarball: fixtureTarball,
  });
});

test("已锁定版本不报告，已有 exact marker 仍交给写 job 收敛", () => {
  assert.deepEqual(discoverCandidate({
    registry: registryFixture(),
    knownVersions: new Set(["0.1.1-rc.2"]),
    openCandidateVersions: new Set(),
  }), { status: "none" });

  assert.equal(discoverCandidate({
    registry: registryFixture(),
    knownVersions: new Set(),
    openCandidateVersions: new Set(["0.1.1-rc.2"]),
  }).status, "candidate");
});

test("latest 必须按 SemVer 优先级严格高于所有已知版本", () => {
  const cases = [
    { latest: "1.0.0-rc.10", known: ["1.0.0-rc.2"], status: "candidate" },
    { latest: "1.0.0-rc-2", known: ["1.0.0-rc-1"], status: "candidate" },
    { latest: "1.0.0", known: ["1.0.0-rc.99"], status: "candidate" },
    { latest: "1.0.0-rc.2", known: ["1.0.0-rc.10"], status: "none" },
    { latest: "1.0.0+new", known: ["1.0.0+old"], status: "none" },
    { latest: "1.0.0", known: [], open: ["1.1.0-rc.1"], status: "none" },
  ];

  for (const { latest, known, open = [], status } of cases) {
    const tarball = `https://registry.npmjs.org/@deepseek-ai/dsh/-/dsh-${latest}.tgz`;
    const registry = {
      "dist-tags": { latest },
      versions: {
        [latest]: { version: latest, dist: { integrity: fixtureIntegrity, tarball } },
      },
    };
    assert.equal(discoverCandidate({
      registry,
      knownVersions: new Set(known),
      openCandidateVersions: new Set(open),
    }).status, status);
  }
});

test("严格 SemVer 拒绝 core 和数字 prerelease 的 leading zero", () => {
  for (const version of ["01.0.0", "1.00.0", "1.0.01", "1.0.0-01", "1.0.0-rc..1"] ) {
    const registry = registryFixture({
      "dist-tags": { latest: version },
      versions: {
        [version]: {
          version,
          dist: { integrity: fixtureIntegrity, tarball: fixtureTarball },
        },
      },
    });
    assert.throws(
      () => discoverCandidate({
        registry,
        knownVersions: new Set(),
        openCandidateVersions: new Set(),
      }),
      { message: "invalid_registry_metadata" },
    );
  }
});

test("拒绝无法绑定 exact record 的 registry metadata", () => {
  for (const registry of [
    registryFixture({ "dist-tags": { latest: "latest" } }),
    registryFixture({ versions: {} }),
    registryFixture({
      versions: {
        "0.1.1-rc.2": {
          version: "0.1.1-rc.1",
          dist: { integrity: fixtureIntegrity, tarball: fixtureTarball },
        },
      },
    }),
  ]) {
    assert.throws(
      () => discoverCandidate({
        registry,
        knownVersions: new Set(),
        openCandidateVersions: new Set(),
      }),
      { message: "invalid_registry_metadata" },
    );
  }
});

test("拒绝无效 integrity 和非官方 HTTPS tarball", () => {
  const shortIntegrity = `sha512-${Buffer.alloc(63).toString("base64")}`;
  const longIntegrity = `sha512-${Buffer.alloc(65).toString("base64")}`;
  const invalidCases = [
    {
      dist: { integrity: "sha256-not-approved", tarball: fixtureTarball },
      error: "invalid_registry_integrity",
    },
    {
      dist: { integrity: shortIntegrity, tarball: fixtureTarball },
      error: "invalid_registry_integrity",
    },
    {
      dist: { integrity: longIntegrity, tarball: fixtureTarball },
      error: "invalid_registry_integrity",
    },
    {
      dist: { integrity: fixtureIntegrity, tarball: "http://registry.npmjs.org/dsh.tgz" },
      error: "invalid_registry_tarball",
    },
    {
      dist: { integrity: fixtureIntegrity, tarball: "https://example.com/dsh.tgz" },
      error: "invalid_registry_tarball",
    },
    {
      dist: { integrity: fixtureIntegrity, tarball: "https://token@registry.npmjs.org/@deepseek-ai/dsh/-/dsh-0.1.1-rc.2.tgz" },
      error: "invalid_registry_tarball",
    },
    {
      dist: { integrity: fixtureIntegrity, tarball: "https://registry.npmjs.org:444/@deepseek-ai/dsh/-/dsh-0.1.1-rc.2.tgz" },
      error: "invalid_registry_tarball",
    },
    {
      dist: { integrity: fixtureIntegrity, tarball: `${fixtureTarball}?mirror=1` },
      error: "invalid_registry_tarball",
    },
    {
      dist: { integrity: fixtureIntegrity, tarball: "https://registry.npmjs.org/other/-/other-0.1.1-rc.2.tgz" },
      error: "invalid_registry_tarball",
    },
  ];

  for (const { dist, error } of invalidCases) {
    const registry = registryFixture({
      versions: { "0.1.1-rc.2": { version: "0.1.1-rc.2", dist } },
    });
    assert.throws(
      () => discoverCandidate({
        registry,
        knownVersions: new Set(),
        openCandidateVersions: new Set(),
      }),
      { message: error },
    );
  }
});

test("在读取正文前拒绝超过 2 MiB 的 Content-Length", async () => {
  let bodyRead = false;
  const fetchImpl = async (url, options) => {
    assert.equal(
      url,
      "https://registry.npmjs.org/%40deepseek-ai%2Fdsh",
    );
    assert.ok(options.signal instanceof AbortSignal);
    return {
      ok: true,
      status: 200,
      headers: new Headers({ "content-length": "2097153" }),
      body: {
        getReader() {
          bodyRead = true;
          throw new Error("body_must_not_be_read");
        },
      },
    };
  };

  await assert.rejects(
    fetchOfficialRegistry({ fetchImpl }),
    { message: "registry_response_too_large" },
  );
  assert.equal(bodyRead, false);
});

test("无 Content-Length 的正文仍执行 2 MiB 流式限长", async () => {
  const oversized = new Uint8Array(2_097_153);
  const response = new Response(oversized, { status: 200 });

  await assert.rejects(
    fetchOfficialRegistry({ fetchImpl: async () => response }),
    { message: "registry_response_too_large" },
  );
});

test("15 秒超时边界返回稳定错误码且不泄露外部错误正文", async () => {
  const fetchImpl = async (_url, { signal }) => new Promise((_, reject) => {
    signal.addEventListener("abort", () => reject(signal.reason), { once: true });
  });

  await assert.rejects(
    fetchOfficialRegistry({ fetchImpl, timeoutMs: 5 }),
    { message: "registry_timeout" },
  );
});

test("429 按有上限的 Retry-After 重试且不输出响应正文", async () => {
  const responses = [
    new Response("secret upstream body", {
      status: 429,
      headers: { "retry-after": "999" },
    }),
    new Response(JSON.stringify(registryFixture()), {
      status: 200,
      headers: { "content-type": "application/json" },
    }),
  ];
  const delays = [];

  const registry = await fetchOfficialRegistry({
    fetchImpl: async () => responses.shift(),
    maxAttempts: 2,
    sleepImpl: async (delay) => delays.push(delay),
  });

  assert.equal(registry["dist-tags"].latest, "0.1.1-rc.2");
  assert.deepEqual(delays, [5_000]);
});

test("响应头完成后正文流超时仍返回 registry_timeout", async () => {
  let attempts = 0;
  const fetchImpl = async (_url, { signal }) => ({
    ok: true,
    status: 200,
    headers: new Headers(),
    body: {
      getReader() {
        attempts += 1;
        return {
          read() {
            return new Promise((_, reject) => {
              signal.addEventListener("abort", () => reject(signal.reason), {
                once: true,
              });
            });
          },
        };
      },
    },
  });

  await assert.rejects(
    fetchOfficialRegistry({
      fetchImpl,
      timeoutMs: 5,
      maxAttempts: 2,
      sleepImpl: async () => {},
    }),
    { message: "registry_timeout" },
  );
  assert.equal(attempts, 2);
});

test("开放 issue 只提取结构化 exact marker", () => {
  const issues = JSON.stringify([[
      {
        number: 7,
        state: "open",
        title: "DSH runtime candidate: 0.1.1-rc.2",
        body: "untrusted text\n<!-- dsh-runtime-candidate:0.1.1-rc.2 -->",
        user: { login: "github-actions[bot]" },
        labels: [{ name: "dsh-runtime-candidate" }],
      },
      {
        number: 8,
        state: "open",
        title: "DSH runtime candidate: 0.1.2",
        body: "<!-- dsh-runtime-candidate:0.1.2 -->",
        user: { login: "attacker" },
        labels: [{ name: "dsh-runtime-candidate" }],
      },
      {
        number: 9,
        state: "open",
        title: "DSH runtime candidate: 0.1.3",
        body: "<!-- dsh-runtime-candidate:0.1.3 -->",
        user: { login: "github-actions[bot]" },
        labels: [],
      },
      {
        number: 10,
        state: "open",
        title: "DSH runtime candidate: 0.1.4",
        body: "<!-- dsh-runtime-candidate:0.1.4 -->",
        user: { login: "github-actions[bot]" },
        labels: [{ name: "dsh-runtime-candidate" }],
        pull_request: { url: "https://api.github.com/pulls/10" },
      },
      {
        number: 11,
        state: "open",
        title: "DSH runtime candidate: 0.2.0",
        body: "<!-- dsh-runtime-candidate:0.2.0 -->\n<!-- dsh-runtime-candidate:9.9.9 -->",
        user: { login: "github-actions[bot]" },
        labels: [{ name: "dsh-runtime-candidate" }],
      },
      { body: "<!-- dsh-runtime-candidate:../../escape -->" },
      { body: null },
    ]]);

  assert.deepEqual(
    readOpenCandidateVersions(issues),
    new Set(["0.1.1-rc.2"]),
  );
});

test("Release 和 tag 共同成为严格已发布版本基线", () => {
  const releases = JSON.stringify([[
    { tag_name: "dsh-v0.1.1-rc.2-windows" },
    { tag_name: "unrelated-v9.9.9" },
  ]]);
  const tags = JSON.stringify([[
    { name: "dsh-v0.1.2-windows" },
    { name: "dsh-v01.2.0-windows" },
  ]]);

  assert.deepEqual(
    readPublishedVersions(releases, tags),
    new Set(["0.1.1-rc.2", "0.1.2"]),
  );
  const published = readPublishedVersions(releases, tags);
  assert.equal(discoverCandidate({
    registry: registryFixture(),
    knownVersions: published,
    openCandidateVersions: new Set(),
  }).status, "none");
});

test("创建后若出现可信重复，只关闭本次创建的较新 issue", async () => {
  const calls = [];
  const trustedIssue = (number) => ({
    number,
    state: "open",
    title: "DSH runtime candidate: 0.1.1-rc.2",
    body: "<!-- dsh-runtime-candidate:0.1.1-rc.2 -->",
    user: { login: "github-actions[bot]" },
    labels: [{ name: "dsh-runtime-candidate" }],
  });
  const responses = [
    new Response("untrusted rate-limit body", {
      status: 429,
      headers: { "retry-after": "999" },
    }),
    new Response(JSON.stringify([]), { status: 200 }),
    new Response(JSON.stringify({ name: "dsh-runtime-candidate" }), { status: 200 }),
    new Response(JSON.stringify(trustedIssue(42)), { status: 201 }),
    new Response(JSON.stringify([trustedIssue(41), trustedIssue(42)]), { status: 200 }),
    new Response(JSON.stringify({ ...trustedIssue(42), state: "closed" }), { status: 200 }),
  ];
  const delays = [];
  const fetchImpl = async (url, options) => {
    calls.push({ url, method: options.method, body: options.body });
    return responses.shift();
  };

  const result = await createCandidateIssue({
    repository: "ccx-dym/dsh-gui",
    candidate: {
      status: "candidate",
      version: "0.1.1-rc.2",
      integrity: fixtureIntegrity,
      tarball: fixtureTarball,
    },
    token: "test-token",
    fetchImpl,
    sleepImpl: async (delay) => delays.push(delay),
  });

  assert.deepEqual(result, {
    status: "closed_duplicate",
    issueNumber: 42,
    closedIssueNumbers: [42],
  });
  assert.deepEqual(delays, [5_000]);
  const close = calls.at(-1);
  assert.equal(close.method, "PATCH");
  assert.match(close.url, /\/issues\/42$/);
  assert.deepEqual(JSON.parse(close.body), {
    state: "closed",
    state_reason: "not_planned",
  });
  const issueQuery = new URL(calls[1].url);
  assert.equal(issueQuery.pathname, "/repos/ccx-dym/dsh-gui/issues");
  assert.equal(issueQuery.searchParams.get("state"), "open");
  assert.equal(issueQuery.searchParams.get("labels"), "dsh-runtime-candidate");
  assert.equal(issueQuery.searchParams.get("per_page"), "100");
  assert.equal(issueQuery.searchParams.get("page"), "1");
});

test("已有可信重复候选保留最小 number 且不关闭不可信 issue", async () => {
  const trusted = (number) => ({
    number,
    state: "open",
    title: "DSH runtime candidate: 0.1.1-rc.2",
    body: "<!-- dsh-runtime-candidate:0.1.1-rc.2 -->",
    user: { login: "github-actions[bot]" },
    labels: [{ name: "dsh-runtime-candidate" }],
  });
  const untrusted = {
    ...trusted(6),
    title: "Human-owned tracking issue",
  };
  const ambiguous = {
    ...trusted(7),
    body: "<!-- dsh-runtime-candidate:0.1.1-rc.2 -->\n<!-- dsh-runtime-candidate:9.9.9 -->",
  };
  const calls = [];
  const responses = [
    new Response(JSON.stringify([trusted(8), untrusted, ambiguous, trusted(4)]), { status: 200 }),
    new Response(JSON.stringify({ ...trusted(8), state: "closed" }), { status: 200 }),
  ];

  const result = await createCandidateIssue({
    repository: "ccx-dym/dsh-gui",
    candidate: {
      status: "candidate",
      version: "0.1.1-rc.2",
      integrity: fixtureIntegrity,
      tarball: fixtureTarball,
    },
    token: "test-token",
    fetchImpl: async (url, options) => {
      calls.push({ url, method: options.method, body: options.body });
      return responses.shift();
    },
    sleepImpl: async () => {},
  });

  assert.deepEqual(result, {
    status: "duplicate",
    issueNumber: 4,
    closedIssueNumbers: [8],
  });
  assert.deepEqual(
    calls.filter((call) => call.method === "PATCH")
      .map((call) => Number(new URL(call.url).pathname.split("/").at(-1))),
    [8],
  );
});

test("仓库名拒绝点段、路径归一化和非规范 owner", async () => {
  for (const repository of [
    "./repo",
    "owner/..",
    "owner/.",
    "-owner/repo",
    "owner-/repo",
    "owner/repo/extra",
    "owner\\repo",
  ]) {
    await assert.rejects(createCandidateIssue({
      repository,
      candidate: {
        status: "candidate",
        version: "0.1.1-rc.2",
        integrity: fixtureIntegrity,
        tarball: fixtureTarball,
      },
      token: "test-token",
      fetchImpl: async () => {
        throw new Error("network_must_not_run");
      },
      sleepImpl: async () => {},
    }), { message: "invalid_repository" });
  }
});
