import assert from "node:assert/strict";
import {
  createHash,
  generateKeyPairSync,
  sign,
} from "node:crypto";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";

const root = await mkdtemp(path.join(tmpdir(), "dsh-tauri-signature-test-"));
const artifactPath = path.join(root, "desktop.exe");
const signaturePath = `${artifactPath}.sig`;
const artifact = Buffer.from("bounded desktop updater fixture\n", "utf8");
const keyId = Buffer.from("0123456789abcdef", "hex");
const { privateKey, publicKey } = generateKeyPairSync("ed25519");
const rawPublicKey = publicKey.export({ type: "spki", format: "der" }).subarray(-32);
const publicKeyLine = Buffer.concat([
  Buffer.from("ED", "ascii"),
  keyId,
  rawPublicKey,
]).toString("base64");
const publicKeyText = `untrusted comment: test public key\n${publicKeyLine}\n`;
const encodedPublicKey = Buffer.from(publicKeyText, "utf8").toString("base64");
const digest = createHash("blake2b512").update(artifact).digest();
const primarySignature = sign(null, digest, privateKey);
const trustedComment = "timestamp:1\tfile:desktop.exe\tprehashed";
const signatureRecord = Buffer.concat([
  Buffer.from("ED", "ascii"),
  keyId,
  primarySignature,
]);
const globalSignature = sign(
  null,
  Buffer.concat([primarySignature, Buffer.from(trustedComment, "utf8")]),
  privateKey,
);
const signatureText = [
  "untrusted comment: test signature",
  signatureRecord.toString("base64"),
  `trusted comment: ${trustedComment}`,
  globalSignature.toString("base64"),
  "",
].join("\n");

await writeFile(artifactPath, artifact);
await writeFile(signaturePath, Buffer.from(signatureText, "utf8").toString("base64"));

function verify(extraEnv = {}) {
  return spawnSync(
    process.execPath,
    ["scripts/verify-tauri-update.mjs", artifactPath, signaturePath],
    {
      cwd: process.cwd(),
      encoding: "utf8",
      env: {
        ...process.env,
        TAURI_UPDATER_PUBLIC_KEY: encodedPublicKey,
        ...extraEnv,
      },
    },
  );
}

const valid = verify();
assert.equal(valid.status, 0, `${valid.stdout}${valid.stderr}`);
assert.match(valid.stdout, /^tauri-update-signature: OK\s*$/);
assert.doesNotMatch(`${valid.stdout}${valid.stderr}`, /test public key|RW/);

await writeFile(artifactPath, Buffer.concat([await readFile(artifactPath), Buffer.from("tamper")]));
const tampered = verify();
assert.notEqual(tampered.status, 0);
assert.match(tampered.stderr, /tauri_update_signature_invalid/);
assert.doesNotMatch(`${tampered.stdout}${tampered.stderr}`, /test public key|RW/);

console.log("verify-tauri-update contract tests passed");
