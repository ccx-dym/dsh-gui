import { readdir } from "node:fs/promises";
import path from "node:path";
import { pathToFileURL } from "node:url";

const REGISTRY_URL =
  "https://registry.npmjs.org/%40deepseek-ai%2Fdsh";
const MAX_REGISTRY_BYTES = 2 * 1024 * 1024;
const REGISTRY_TIMEOUT_MS = 15_000;
const MAX_RETRY_DELAY_MS = 5_000;
const DEFAULT_MAX_ATTEMPTS = 3;
const MAX_GITHUB_PAGES = 20;
const CANDIDATE_LABEL = "dsh-runtime-candidate";
const TRUSTED_BOT_LOGINS = new Set(["github-actions[bot]"]);
const OPEN_CANDIDATE_ISSUES_ENDPOINT =
  `/issues?state=open&labels=${encodeURIComponent(CANDIDATE_LABEL)}`;
const SEMVER_SOURCE =
  "(?:0|[1-9]\\d*)\\.(?:0|[1-9]\\d*)\\.(?:0|[1-9]\\d*)" +
  "(?:-(?:0|[1-9]\\d*|\\d*[A-Za-z-][0-9A-Za-z-]*)" +
  "(?:\\.(?:0|[1-9]\\d*|\\d*[A-Za-z-][0-9A-Za-z-]*))*)?" +
  "(?:\\+[0-9A-Za-z-]+(?:\\.[0-9A-Za-z-]+)*)?";
const EXACT_SEMVER = new RegExp(`^${SEMVER_SOURCE}$`);
const CANDIDATE_MARKER = new RegExp(
  `<!--\\s*dsh-runtime-candidate:(${SEMVER_SOURCE})\\s*-->`,
  "g",
);
const RELEASE_TAG = new RegExp(`^dsh-v(${SEMVER_SOURCE})-windows$`);

function parseSemVer(version) {
  if (typeof version !== "string" || !EXACT_SEMVER.test(version)) return null;
  const withoutBuild = version.split("+", 1)[0];
  const prereleaseStart = withoutBuild.indexOf("-");
  const core = prereleaseStart === -1
    ? withoutBuild
    : withoutBuild.slice(0, prereleaseStart);
  const prerelease = prereleaseStart === -1
    ? undefined
    : withoutBuild.slice(prereleaseStart + 1);
  const [major, minor, patch] = core.split(".").map((part) => BigInt(part));
  return {
    major,
    minor,
    patch,
    prerelease: prerelease?.split(".") ?? null,
  };
}

function compareSemVer(leftVersion, rightVersion) {
  const left = parseSemVer(leftVersion);
  const right = parseSemVer(rightVersion);
  if (!left || !right) throw new Error("invalid_known_version");
  for (const key of ["major", "minor", "patch"]) {
    if (left[key] < right[key]) return -1;
    if (left[key] > right[key]) return 1;
  }
  if (left.prerelease === null && right.prerelease === null) return 0;
  if (left.prerelease === null) return 1;
  if (right.prerelease === null) return -1;
  const length = Math.max(left.prerelease.length, right.prerelease.length);
  for (let index = 0; index < length; index += 1) {
    const leftPart = left.prerelease[index];
    const rightPart = right.prerelease[index];
    if (leftPart === undefined) return -1;
    if (rightPart === undefined) return 1;
    if (leftPart === rightPart) continue;
    const leftNumeric = /^\d+$/.test(leftPart);
    const rightNumeric = /^\d+$/.test(rightPart);
    if (leftNumeric && rightNumeric) {
      return BigInt(leftPart) < BigInt(rightPart) ? -1 : 1;
    }
    if (leftNumeric !== rightNumeric) return leftNumeric ? -1 : 1;
    return leftPart < rightPart ? -1 : 1;
  }
  return 0;
}

function isCanonicalSha512(integrity) {
  if (typeof integrity !== "string" || !integrity.startsWith("sha512-")) return false;
  const encoded = integrity.slice("sha512-".length);
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(encoded)) return false;
  const digest = Buffer.from(encoded, "base64");
  return digest.byteLength === 64 && digest.toString("base64") === encoded;
}

function validatedTarball(version, source) {
  let tarball;
  try {
    tarball = new URL(source);
  } catch {
    throw new Error("invalid_registry_tarball");
  }
  const expectedPath = `/@deepseek-ai/dsh/-/dsh-${version}.tgz`;
  if (tarball.protocol !== "https:" ||
      tarball.hostname !== "registry.npmjs.org" ||
      tarball.port !== "" ||
      tarball.username !== "" ||
      tarball.password !== "" ||
      tarball.search !== "" ||
      tarball.hash !== "" ||
      tarball.pathname !== expectedPath) {
    throw new Error("invalid_registry_tarball");
  }
  return tarball.href;
}

/**
 * 从已验证的 registry 文档中选择一个尚未处理的 exact 版本。
 *
 * @param {{registry: object, knownVersions: Set<string>, openCandidateVersions: Set<string>}} input 扫描输入。
 * @returns {{status: "none"} | {status: "candidate", version: string, integrity: string, tarball: string}} 候选结果。
 * @throws {Error} registry 元数据、integrity 或下载地址不可信时抛出稳定错误码。
 */
export function discoverCandidate({
  registry,
  knownVersions,
  openCandidateVersions,
}) {
  const version = registry?.["dist-tags"]?.latest;
  const exact = registry?.versions?.[version];
  if (!parseSemVer(version) || exact?.version !== version) {
    throw new Error("invalid_registry_metadata");
  }
  if (!isCanonicalSha512(exact.dist?.integrity)) {
    throw new Error("invalid_registry_integrity");
  }
  const tarball = validatedTarball(version, exact.dist?.tarball);

  const known = [...knownVersions];
  const open = [...openCandidateVersions];
  if ([...known, ...open].some((seen) => !parseSemVer(seen))) {
    throw new Error("invalid_known_version");
  }
  // 已发布/已锁定版本是安装事实，必须严格低于 latest；同版本开放 Issue
  // 仍交给写 job 做唯一性收敛，但更高版本开放 Issue 会阻止 registry 回退。
  if (known.some((seen) => compareSemVer(version, seen) <= 0) ||
      open.some((seen) => compareSemVer(version, seen) < 0)) {
    return { status: "none" };
  }
  return {
    status: "candidate",
    version,
    integrity: exact.dist.integrity,
    tarball,
  };
}

async function readResponseJson(response, signal, errorPrefix) {
  const contentLength = response.headers.get("content-length");
  if (contentLength !== null) {
    if (!/^\d+$/.test(contentLength)) {
      throw new Error(`invalid_${errorPrefix}_response`);
    }
    if (Number(contentLength) > MAX_REGISTRY_BYTES) {
      throw new Error(`${errorPrefix}_response_too_large`);
    }
  }
  if (!response.body || typeof response.body.getReader !== "function") {
    throw new Error(`invalid_${errorPrefix}_response`);
  }

  const reader = response.body.getReader();
  const decoder = new TextDecoder("utf-8", { fatal: true });
  let totalBytes = 0;
  let body = "";
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      totalBytes += value.byteLength;
      if (totalBytes > MAX_REGISTRY_BYTES) {
        await reader.cancel().catch(() => undefined);
        throw new Error(`${errorPrefix}_response_too_large`);
      }
      body += decoder.decode(value, { stream: true });
    }
    body += decoder.decode();
  } catch (error) {
    if (error instanceof Error &&
        error.message === `${errorPrefix}_response_too_large`) {
      throw error;
    }
    if (signal.aborted) throw new Error(`${errorPrefix}_timeout`);
    throw new Error(`invalid_${errorPrefix}_response`);
  }

  try {
    return JSON.parse(body);
  } catch {
    throw new Error(`invalid_${errorPrefix}_json`);
  }
}

function retryDelay(response, attempt) {
  const retryAfter = response?.headers.get("retry-after");
  if (retryAfter !== null && /^\d+$/.test(retryAfter)) {
    return Math.min(Number(retryAfter) * 1_000, MAX_RETRY_DELAY_MS);
  }
  return Math.min(250 * (2 ** attempt), MAX_RETRY_DELAY_MS);
}

async function requestJson({
  url,
  errorPrefix,
  acceptedStatuses,
  method = "GET",
  headers = {},
  body,
  fetchImpl,
  timeoutMs,
  maxAttempts,
  sleepImpl,
}) {
  const retryableStatuses = new Set([429, 500, 502, 503, 504]);
  let lastError = `${errorPrefix}_request_failed`;
  for (let attempt = 0; attempt < maxAttempts; attempt += 1) {
    const signal = AbortSignal.timeout(timeoutMs);
    let response;
    try {
      response = await fetchImpl(url, {
        method,
        headers,
        body,
        redirect: "error",
        signal,
      });
    } catch {
      lastError = signal.aborted
        ? `${errorPrefix}_timeout`
        : `${errorPrefix}_request_failed`;
      if (attempt + 1 < maxAttempts) {
        await sleepImpl(retryDelay(null, attempt));
        continue;
      }
      throw new Error(lastError);
    }

    if (acceptedStatuses.has(response.status)) {
      try {
        const json = response.status === 204
          ? null
          : await readResponseJson(response, signal, errorPrefix);
        return { status: response.status, json };
      } catch (error) {
        if (error instanceof Error &&
            error.message === `${errorPrefix}_timeout` &&
            attempt + 1 < maxAttempts) {
          await sleepImpl(retryDelay(null, attempt));
          continue;
        }
        throw error;
      }
    }
    if (retryableStatuses.has(response.status) && attempt + 1 < maxAttempts) {
      await response.body?.cancel().catch(() => undefined);
      await sleepImpl(retryDelay(response, attempt));
      continue;
    }
    await response.body?.cancel().catch(() => undefined);
    throw new Error(`${errorPrefix}_http_error`);
  }
  throw new Error(lastError);
}

/**
 * 读取官方 npm registry 文档，并实施超时、有限重试与 2 MiB 上限。
 *
 * @param {{fetchImpl?: typeof fetch, timeoutMs?: number, maxAttempts?: number, sleepImpl?: (delay: number) => Promise<void>}} [options] 可替换的外部网络边界。
 * @returns {Promise<object>} 已解析的 registry JSON。
 * @throws {Error} 网络、超时、状态码、大小或 JSON 不合法时抛出稳定错误码。
 */
export async function fetchOfficialRegistry({
  fetchImpl = globalThis.fetch,
  timeoutMs = REGISTRY_TIMEOUT_MS,
  maxAttempts = DEFAULT_MAX_ATTEMPTS,
  sleepImpl = (delay) => new Promise((resolve) => setTimeout(resolve, delay)),
} = {}) {
  const response = await requestJson({
    url: REGISTRY_URL,
    errorPrefix: "registry",
    acceptedStatuses: new Set([200]),
    headers: { accept: "application/json" },
    fetchImpl,
    timeoutMs,
    maxAttempts,
    sleepImpl,
  });
  if (response.json === null || typeof response.json !== "object" ||
      Array.isArray(response.json)) {
    throw new Error("invalid_registry_json");
  }
  return response.json;
}

/**
 * 从 GitHub issues API 的 JSON 中提取结构化候选 marker。
 *
 * @param {string} source GitHub API JSON；允许 `gh api --slurp` 的分页数组。
 * @returns {Set<string>} 已有开放候选的 exact 版本集合。
 * @throws {Error} JSON 不是受支持的 issue 数组时抛出稳定错误码。
 */
export function readOpenCandidateVersions(source) {
  let pages;
  try {
    pages = JSON.parse(source);
  } catch {
    throw new Error("invalid_issues_json");
  }
  if (!Array.isArray(pages)) throw new Error("invalid_issues_json");

  const issues = pages.flatMap((page) => {
    if (!Array.isArray(page)) throw new Error("invalid_issues_json");
    return page;
  });
  const versions = new Set();
  for (const issue of issues) {
    const version = matchCandidateVersion(issue?.body);
    const trusted = issue?.state === "open" &&
      !Object.hasOwn(issue, "pull_request") &&
      TRUSTED_BOT_LOGINS.has(issue?.user?.login) &&
      version !== null &&
      issue?.title === `DSH runtime candidate: ${version}` &&
      Array.isArray(issue?.labels) &&
      issue.labels.some((label) => label?.name === CANDIDATE_LABEL);
    if (trusted) versions.add(version);
  }
  return versions;
}

function matchCandidateVersion(body) {
  if (typeof body !== "string") return null;
  const matches = [...body.matchAll(new RegExp(CANDIDATE_MARKER.source, "g"))];
  return matches.length === 1 ? matches[0][1] : null;
}

function parsePages(source, errorCode) {
  let pages;
  try {
    pages = JSON.parse(source);
  } catch {
    throw new Error(errorCode);
  }
  if (!Array.isArray(pages) || pages.some((page) => !Array.isArray(page))) {
    throw new Error(errorCode);
  }
  return pages.flat();
}

/**
 * 从 GitHub Releases 与 tags 响应中提取已经发布的 Windows runtime 版本。
 *
 * @param {string} releasesSource Releases 分页 JSON。
 * @param {string} tagsSource tags 分页 JSON。
 * @returns {Set<string>} 严格 SemVer 的已发布版本。
 * @throws {Error} GitHub JSON 结构不合法时抛出固定错误码。
 */
export function readPublishedVersions(releasesSource, tagsSource) {
  const versions = new Set();
  for (const release of parsePages(releasesSource, "invalid_releases_json")) {
    const match = typeof release?.tag_name === "string"
      ? RELEASE_TAG.exec(release.tag_name)
      : null;
    if (match) versions.add(match[1]);
  }
  for (const tag of parsePages(tagsSource, "invalid_tags_json")) {
    const match = typeof tag?.name === "string" ? RELEASE_TAG.exec(tag.name) : null;
    if (match) versions.add(match[1]);
  }
  return versions;
}

function validateRepository(repository) {
  if (typeof repository !== "string") throw new Error("invalid_repository");
  const segments = repository.split("/");
  if (segments.length !== 2) throw new Error("invalid_repository");
  const [owner, name] = segments;
  const ownerValid = /^[A-Za-z0-9](?:[A-Za-z0-9-]{0,37}[A-Za-z0-9])?$/.test(owner);
  const nameValid = /^[A-Za-z0-9_.-]{1,100}$/.test(name) &&
    name !== "." && name !== "..";
  if (!ownerValid || !nameValid) {
    throw new Error("invalid_repository");
  }
  return `${owner}/${name}`;
}

/**
 * 从 CLI 环境中读取唯一批准的 GitHub token 变量。
 *
 * @param {Record<string, string | undefined>} environment 进程环境映射。
 * @returns {string} 非空 `GITHUB_TOKEN`。
 * @throws {Error} token 缺失时抛出固定错误码。
 */
export function githubTokenFromEnvironment(environment) {
  const token = environment?.GITHUB_TOKEN;
  if (typeof token !== "string" || token.length === 0) {
    throw new Error("github_token_missing");
  }
  return token;
}

async function githubRequest({
  repository,
  endpoint,
  token,
  method = "GET",
  body,
  acceptedStatuses = new Set([200]),
  fetchImpl,
  timeoutMs,
  maxAttempts,
  sleepImpl,
}) {
  validateRepository(repository);
  if (typeof token !== "string" || token.length === 0) {
    throw new Error("github_token_missing");
  }
  const url = new URL(`https://api.github.com/repos/${repository}${endpoint}`);
  if (url.hostname !== "api.github.com" || url.protocol !== "https:") {
    throw new Error("invalid_github_url");
  }
  return requestJson({
    url: url.href,
    errorPrefix: "github",
    acceptedStatuses,
    method,
    headers: {
      accept: "application/vnd.github+json",
      authorization: `Bearer ${token}`,
      "content-type": "application/json",
      "user-agent": "dsh-desktop-upstream-scanner",
      "x-github-api-version": "2022-11-28",
    },
    body: body === undefined ? undefined : JSON.stringify(body),
    fetchImpl,
    timeoutMs,
    maxAttempts,
    sleepImpl,
  });
}

async function fetchGitHubPages(options, endpoint) {
  const pages = [];
  for (let page = 1; page <= MAX_GITHUB_PAGES; page += 1) {
    const separator = endpoint.includes("?") ? "&" : "?";
    const response = await githubRequest({
      ...options,
      endpoint: `${endpoint}${separator}per_page=100&page=${page}`,
    });
    if (!Array.isArray(response.json)) throw new Error("invalid_github_json");
    pages.push(response.json);
    if (response.json.length < 100) return pages;
  }
  throw new Error("github_page_limit_exceeded");
}

function trustedIssuesForVersion(pages, version) {
  const marker = `<!-- dsh-runtime-candidate:${version} -->`;
  const title = `DSH runtime candidate: ${version}`;
  return pages.flat()
    .filter((issue) => issue?.state === "open" &&
      Number.isSafeInteger(issue?.number) && issue.number > 0 &&
      !Object.hasOwn(issue, "pull_request") &&
      TRUSTED_BOT_LOGINS.has(issue?.user?.login) &&
      Array.isArray(issue?.labels) &&
      issue.labels.some((label) => label?.name === CANDIDATE_LABEL) &&
      issue?.title === title &&
      matchCandidateVersion(issue?.body) === version && issue.body.includes(marker))
    .sort((left, right) => left.number - right.number);
}

async function convergeTrustedIssues(options, issues) {
  if (issues.length === 0) return { issueNumber: undefined, closedIssueNumbers: [] };
  const [kept, ...duplicates] = issues;
  const closedIssueNumbers = [];
  for (const duplicate of duplicates) {
    await githubRequest({
      ...options,
      endpoint: `/issues/${duplicate.number}`,
      method: "PATCH",
      body: { state: "closed", state_reason: "not_planned" },
      acceptedStatuses: new Set([200]),
    });
    closedIssueNumbers.push(duplicate.number);
  }
  return { issueNumber: kept.number, closedIssueNumbers };
}

function candidateIssueBody(candidate) {
  return `<!-- dsh-runtime-candidate:${candidate.version} -->

## Official package

- Version: \`${candidate.version}\`
- Integrity: \`${candidate.integrity}\`
- Tarball: ${candidate.tarball}
- [Upstream GitHub compare](https://github.com/deepseek-ai/deepseek-harness/compare/v${candidate.version}...main)

## Core compatibility gates

- [ ] Exact package/version, npm integrity and dependency closure
- [ ] Node engine and Windows x64 native dependencies
- [ ] CLI help/version smoke
- [ ] Loopback web startup with no browser opening
- [ ] stdout readiness and HTTP root readiness
- [ ] Temporary DSH_HOME config, session and workspace load
- [ ] Process-tree stop and failure cleanup
- [ ] Isolated data-copy verification without real user-data writes
`;
}

/**
 * 创建候选 issue，并在创建后关闭本次产生的较新可信重复项。
 *
 * @param {{repository: string, candidate: object, token: string, fetchImpl?: typeof fetch, timeoutMs?: number, maxAttempts?: number, sleepImpl?: (delay: number) => Promise<void>}} input 创建参数。
 * @returns {Promise<{status: "duplicate" | "created" | "closed_duplicate", issueNumber?: number}>} 收敛结果。
 * @throws {Error} 候选、GitHub 响应或收敛状态不合法时抛出固定错误码。
 */
export async function createCandidateIssue({
  repository,
  candidate,
  token,
  fetchImpl = globalThis.fetch,
  timeoutMs = REGISTRY_TIMEOUT_MS,
  maxAttempts = DEFAULT_MAX_ATTEMPTS,
  sleepImpl = (delay) => new Promise((resolve) => setTimeout(resolve, delay)),
}) {
  if (candidate?.status !== "candidate" || !parseSemVer(candidate.version)) {
    throw new Error("invalid_candidate");
  }
  if (!isCanonicalSha512(candidate.integrity)) {
    throw new Error("invalid_candidate");
  }
  validatedTarball(candidate.version, candidate.tarball);
  const options = {
    repository,
    token,
    fetchImpl,
    timeoutMs,
    maxAttempts,
    sleepImpl,
  };

  const before = await fetchGitHubPages(options, OPEN_CANDIDATE_ISSUES_ENDPOINT);
  const existing = trustedIssuesForVersion(before, candidate.version);
  if (existing.length > 0) {
    return {
      status: "duplicate",
      ...await convergeTrustedIssues(options, existing),
    };
  }

  const label = await githubRequest({
    ...options,
    endpoint: `/labels/${encodeURIComponent(CANDIDATE_LABEL)}`,
    acceptedStatuses: new Set([200, 404]),
  });
  if (label.status === 404) {
    await githubRequest({
      ...options,
      endpoint: "/labels",
      method: "POST",
      body: {
        name: CANDIDATE_LABEL,
        color: "1d76db",
        description: "Automated official DSH runtime compatibility candidate",
      },
      acceptedStatuses: new Set([201]),
    });
  }

  const created = await githubRequest({
    ...options,
    endpoint: "/issues",
    method: "POST",
    body: {
      title: `DSH runtime candidate: ${candidate.version}`,
      body: candidateIssueBody(candidate),
      labels: [CANDIDATE_LABEL],
    },
    acceptedStatuses: new Set([201]),
  });
  const issueNumber = created.json?.number;
  if (!Number.isSafeInteger(issueNumber) || issueNumber < 1 ||
      !TRUSTED_BOT_LOGINS.has(created.json?.user?.login)) {
    throw new Error("invalid_created_issue");
  }

  const after = await fetchGitHubPages(options, OPEN_CANDIDATE_ISSUES_ENDPOINT);
  const duplicates = trustedIssuesForVersion(after, candidate.version);
  if (!duplicates.some((issue) => issue.number === issueNumber)) {
    throw new Error("github_consistency_failed");
  }
  const convergence = await convergeTrustedIssues(options, duplicates);
  if (convergence.issueNumber !== issueNumber) {
    return {
      status: "closed_duplicate",
      issueNumber,
      closedIssueNumbers: convergence.closedIssueNumbers,
    };
  }
  return {
    status: "created",
    issueNumber,
    closedIssueNumbers: convergence.closedIssueNumbers,
  };
}

async function readKnownVersions(repositoryRoot) {
  const lockRoot = path.join(repositoryRoot, "runtime", "locks");
  const entries = await readdir(lockRoot, { withFileTypes: true });
  return new Set(entries
    .filter((entry) => entry.isDirectory() && entry.name.startsWith("dsh-"))
    .map((entry) => entry.name.slice(4))
    .filter((version) => parseSemVer(version) !== null));
}

function parseArguments(argv) {
  const [command, ...rest] = argv;
  if (!new Set(["scan", "create"]).has(command) || rest.length % 2 !== 0) {
    throw new Error("invalid_arguments");
  }
  const values = {};
  for (let index = 0; index < rest.length; index += 2) {
    const option = rest[index];
    const value = rest[index + 1];
    if (!option.startsWith("--") || !value || Object.hasOwn(values, option)) {
      throw new Error("invalid_arguments");
    }
    values[option] = value;
  }
  const allowed = command === "scan"
    ? new Set(["--repository"])
    : new Set(["--repository", "--version", "--integrity", "--tarball"]);
  if (Object.keys(values).some((option) => !allowed.has(option)) ||
      [...allowed].some((option) => !Object.hasOwn(values, option))) {
    throw new Error("invalid_arguments");
  }
  return { command, values };
}

async function runCli() {
  const { command, values } = parseArguments(process.argv.slice(2));
  const repositoryRoot = path.resolve(import.meta.dirname, "..");
  const token = githubTokenFromEnvironment(process.env);
  const repository = values["--repository"];
  let result;
  if (command === "scan") {
    const github = { repository, token, fetchImpl: globalThis.fetch,
      timeoutMs: REGISTRY_TIMEOUT_MS, maxAttempts: DEFAULT_MAX_ATTEMPTS,
      sleepImpl: (delay) => new Promise((resolve) => setTimeout(resolve, delay)) };
    const [registry, knownVersions, issues, releases, tags] = await Promise.all([
      fetchOfficialRegistry(),
      readKnownVersions(repositoryRoot),
      fetchGitHubPages(github, "/issues?state=open"),
      fetchGitHubPages(github, "/releases"),
      fetchGitHubPages(github, "/tags"),
    ]);
    for (const version of readPublishedVersions(
      JSON.stringify(releases),
      JSON.stringify(tags),
    )) {
      knownVersions.add(version);
    }
    result = discoverCandidate({
      registry,
      knownVersions,
      openCandidateVersions: readOpenCandidateVersions(JSON.stringify(issues)),
    });
  } else {
    result = await createCandidateIssue({
      repository,
      token,
      candidate: {
        status: "candidate",
        version: values["--version"],
        integrity: values["--integrity"],
        tarball: values["--tarball"],
      },
    });
  }
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

function publicErrorCode(error) {
  const code = error instanceof Error ? error.message : "scanner_failed";
  return /^[a-z][a-z0-9_]+$/.test(code) ? code : "scanner_failed";
}

if (process.argv[1] && pathToFileURL(path.resolve(process.argv[1])).href === import.meta.url) {
  runCli().catch((error) => {
    process.stderr.write(`${publicErrorCode(error)}\n`);
    process.exitCode = 1;
  });
}
