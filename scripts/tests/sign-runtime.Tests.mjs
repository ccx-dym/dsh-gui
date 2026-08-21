import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { generateKeyPairSync } from "node:crypto";
import { spawnSync } from "node:child_process";

const repositoryRoot = path.resolve(import.meta.dirname, "../..");
const signer = path.join(repositoryRoot, "scripts/sign-runtime.mjs");
const temporaryRoot = await mkdtemp(path.join(tmpdir(), "dsh-runtime-sign-test-"));
const manifestPath = path.join(temporaryRoot, "manifest.json");
const signaturePath = path.join(temporaryRoot, "manifest.sig");
await writeFile(manifestPath, '{"schema":1}\n', "utf8");

function runSigner(environment = {}, args = [manifestPath, signaturePath]) {
  return spawnSync(process.execPath, [signer, ...args], {
    cwd: repositoryRoot,
    env: { ...process.env, ...environment },
    encoding: "utf8",
  });
}

const missingKey = runSigner({ DSH_RUNTIME_SIGNING_KEY_FILE: "" });
assert.notEqual(missingKey.status, 0);
assert.match(`${missingKey.stdout}${missingKey.stderr}`, /DSH_RUNTIME_SIGNING_KEY_FILE/);

const rsa = generateKeyPairSync("rsa", { modulusLength: 2048 });
const rsaPath = path.join(temporaryRoot, "do-not-print-rsa-private.pem");
await writeFile(rsaPath, rsa.privateKey.export({ type: "pkcs8", format: "pem" }));
const wrongType = runSigner({ DSH_RUNTIME_SIGNING_KEY_FILE: rsaPath });
assert.notEqual(wrongType.status, 0);
assert.match(`${wrongType.stdout}${wrongType.stderr}`, /Ed25519/);
assert.doesNotMatch(`${wrongType.stdout}${wrongType.stderr}`, /do-not-print-rsa-private|BEGIN PRIVATE KEY/);

const ed25519 = generateKeyPairSync("ed25519");
const keyPath = path.join(temporaryRoot, "do-not-print-test-private.pem");
await writeFile(keyPath, ed25519.privateKey.export({ type: "pkcs8", format: "pem" }));
const signed = runSigner({ DSH_RUNTIME_SIGNING_KEY_FILE: keyPath });
assert.equal(signed.status, 0, `${signed.stdout}${signed.stderr}`);
assert.doesNotMatch(`${signed.stdout}${signed.stderr}`, /do-not-print-test-private|BEGIN PRIVATE KEY/);
assert.match(await readFile(signaturePath, "utf8"), /^[0-9a-f]{128}$/);

console.log("sign-runtime contract tests passed");
