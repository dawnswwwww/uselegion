#!/usr/bin/env node
// Sign the Legion release manifest with the stable-channel Ed25519 key.
//
// Usage:
//   node sign-manifest.mjs <manifest.json> <output.sig>
//
// The signing key is read from the LEGION_RELEASE_SIGNING_KEY environment
// variable (a 64-character hex string — the 32-byte Ed25519 seed). The
// companion public key is compiled into the CLI as
// `STABLE_RELEASE_PUBLIC_KEY` (crates/legion-protocol/src/manifest.rs).
//
// No npm dependencies: uses Node's built-in `crypto` module (KeyObject API).

import { createPrivateKey, sign, createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";

const PKCS8_ED25519_PREFIX = Buffer.from([
  0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70,
  0x04, 0x22, 0x04, 0x20,
]);

async function main() {
  const [, , manifestPath, sigPath] = process.argv;

  if (!manifestPath || !sigPath) {
    console.error("usage: sign-manifest.mjs <manifest.json> <output.sig>");
    process.exit(2);
  }

  const seedHex = process.env.LEGION_RELEASE_SIGNING_KEY;
  if (!seedHex) {
    console.error(
      "LEGION_RELEASE_SIGNING_KEY env var is required (64-char hex seed)",
    );
    process.exit(2);
  }
  if (seedHex.length !== 64 || !/^[0-9a-fA-F]{64}$/.test(seedHex)) {
    console.error(
      `LEGION_RELEASE_SIGNING_KEY must be 32 bytes (64 hex chars), got ${seedHex.length} chars`,
    );
    process.exit(2);
  }

  // Wrap the 32-byte Ed25519 seed in the PKCS8 ASN.1 envelope so Node's
  // crypto module can import it as a private KeyObject. This is the standard
  // Ed25519 PKCS8 prefix followed by the raw seed.
  const pkcs8 = Buffer.concat([PKCS8_ED25519_PREFIX, Buffer.from(seedHex, "hex")]);
  const privateKey = createPrivateKey({
    key: pkcs8,
    format: "der",
    type: "pkcs8",
  });

  const manifestBytes = await readFile(manifestPath);

  // Ed25519 signs the raw message bytes directly (no prehash). Node's
  // crypto.sign() with an explicit algorithm is the correct API for
  // Ed25519 (createSign does not accept a null algorithm here).
  const sig = sign(null, manifestBytes, privateKey);

  await writeFile(sigPath, sig);

  const sha256 = createHash("sha256").update(manifestBytes).digest("hex");
  console.error(`manifest sha256: ${sha256}`);
  console.error(`signature (${sig.length} bytes): ${sig.toString("hex")}`);
  console.error(`wrote signature to ${sigPath}`);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
