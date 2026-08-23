import {
  createHash,
  createPublicKey,
  timingSafeEqual,
  verify,
} from "node:crypto";
import { readFile, stat } from "node:fs/promises";

const MAX_ARTIFACT_BYTES = 256 * 1024 * 1024;
const MAX_SIGNATURE_BYTES = 8192;
const MAX_PUBLIC_KEY_CHARS = 2048;
const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

function fail(code) {
  process.stderr.write(`${code}\n`);
  process.exit(1);
}

function decodeBase64(value, expectedBytes) {
  if (!/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(value)) {
    throw new Error("base64");
  }
  const decoded = Buffer.from(value, "base64");
  if (decoded.length !== expectedBytes || decoded.toString("base64") !== value) {
    throw new Error("base64");
  }
  return decoded;
}

function decodeEnvelope(value, maxDecodedBytes) {
  const encoded = value.trim();
  if (encoded.length === 0 || encoded.length > Math.ceil(maxDecodedBytes * 4 / 3) + 4) {
    throw new Error("envelope");
  }
  const decoded = Buffer.from(encoded, "base64");
  if (decoded.length === 0 || decoded.length > maxDecodedBytes || decoded.toString("base64") !== encoded) {
    throw new Error("envelope");
  }
  return decoded.toString("utf8");
}

function parsePublicKey(encodedKey) {
  const lines = decodeEnvelope(encodedKey, MAX_PUBLIC_KEY_CHARS)
    .trimEnd()
    .split(/\r?\n/);
  if (lines.length !== 2 || !lines[0].startsWith("untrusted comment: ")) {
    throw new Error("public_key");
  }
  const record = decodeBase64(lines[1], 42);
  const algorithm = record.subarray(0, 2).toString("ascii");
  if (algorithm !== "Ed" && algorithm !== "ED") {
    throw new Error("public_key");
  }
  return {
    keyId: record.subarray(2, 10),
    key: createPublicKey({
      key: Buffer.concat([ED25519_SPKI_PREFIX, record.subarray(10)]),
      format: "der",
      type: "spki",
    }),
  };
}

function parseSignature(encodedSignature) {
  const lines = decodeEnvelope(encodedSignature, MAX_SIGNATURE_BYTES)
    .trimEnd()
    .split(/\r?\n/);
  if (
    lines.length !== 4 ||
    !lines[0].startsWith("untrusted comment: ") ||
    !lines[2].startsWith("trusted comment: ")
  ) {
    throw new Error("signature");
  }
  const record = decodeBase64(lines[1], 74);
  if (record.subarray(0, 2).toString("ascii") !== "ED") {
    throw new Error("signature");
  }
  return {
    keyId: record.subarray(2, 10),
    primary: record.subarray(10),
    trustedComment: lines[2].slice("trusted comment: ".length),
    global: decodeBase64(lines[3], 64),
  };
}

const [artifactPath, signaturePath] = process.argv.slice(2);
const encodedPublicKey = process.env.TAURI_UPDATER_PUBLIC_KEY;
if (!artifactPath || !signaturePath || !encodedPublicKey) {
  fail("tauri_update_verifier_configuration_invalid");
}

try {
  const artifactStat = await stat(artifactPath);
  const signatureStat = await stat(signaturePath);
  if (
    !artifactStat.isFile() ||
    artifactStat.size === 0 ||
    artifactStat.size > MAX_ARTIFACT_BYTES ||
    !signatureStat.isFile() ||
    signatureStat.size === 0 ||
    signatureStat.size > MAX_SIGNATURE_BYTES ||
    encodedPublicKey.length > MAX_PUBLIC_KEY_CHARS
  ) {
    fail("tauri_update_artifact_bounds_invalid");
  }

  const [artifact, encodedSignature] = await Promise.all([
    readFile(artifactPath),
    readFile(signaturePath, "utf8"),
  ]);
  const publicKey = parsePublicKey(encodedPublicKey);
  const signature = parseSignature(encodedSignature);
  if (!timingSafeEqual(publicKey.keyId, signature.keyId)) {
    fail("tauri_update_signature_invalid");
  }

  // Tauri 2 使用 Minisign 预哈希格式：主签名覆盖 BLAKE2b-512 摘要，
  // 全局签名再绑定主签名和 trusted comment，避免元数据被替换。
  const digest = createHash("blake2b512").update(artifact).digest();
  const primaryValid = verify(null, digest, publicKey.key, signature.primary);
  const globalMessage = Buffer.concat([
    signature.primary,
    Buffer.from(signature.trustedComment, "utf8"),
  ]);
  const globalValid = verify(null, globalMessage, publicKey.key, signature.global);
  if (!primaryValid || !globalValid) {
    fail("tauri_update_signature_invalid");
  }
  process.stdout.write("tauri-update-signature: OK\n");
} catch {
  fail("tauri_update_signature_invalid");
}
