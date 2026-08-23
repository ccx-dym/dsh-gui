import { createPrivateKey, sign } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import process from "node:process";

function fail(message) {
  console.error(message);
  process.exitCode = 1;
}

const [manifestPath, signaturePath] = process.argv.slice(2);
const keyPath = process.env.DSH_RUNTIME_SIGNING_KEY_FILE;
const keyPassword = process.env.DSH_RUNTIME_SIGNING_KEY_PASSWORD;

if (!manifestPath || !signaturePath) {
  fail("用法: node scripts/sign-runtime.mjs <manifest.json> <manifest.sig>");
} else if (!keyPath) {
  fail("缺少 DSH_RUNTIME_SIGNING_KEY_FILE");
} else {
  try {
    // 原始 bytes 直接进入 Ed25519；不得解析或重排 JSON，以保持 Rust 验证边界一致。
    const [manifestBytes, privatePem] = await Promise.all([
      readFile(manifestPath),
      readFile(keyPath),
    ]);
    const privateKey = createPrivateKey(keyPassword
      ? { key: privatePem, passphrase: keyPassword }
      : privatePem);
    if (privateKey.asymmetricKeyType !== "ed25519") {
      throw new Error("签名密钥必须是 Ed25519 私钥");
    }
    const signature = sign(null, manifestBytes, privateKey).toString("hex");
    await writeFile(signaturePath, signature, { encoding: "ascii", flag: "wx" });
    console.log("runtime manifest signature written");
  } catch (error) {
    // 故意不打印异常对象：文件系统异常可能包含私钥路径。
    fail(error instanceof Error && error.message.includes("Ed25519")
      ? error.message
      : "runtime manifest 签名失败");
  }
}
