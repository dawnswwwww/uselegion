# Release signing key

Legion signs every release manifest with an **Ed25519** keypair so the CLI
(`legion gateway install`) can verify it is downloading a genuine
`legion-gateway` build. This document describes how the key is managed,
rotated, and verified.

## Where the keys live

| Key | Location | Access |
|---|---|---|
| Public key | `crates/legion-protocol/src/manifest.rs` (`STABLE_RELEASE_PUBLIC_KEY`) | Public, in the repo |
| Private key (seed, 32 bytes / 64 hex chars) | GitHub Actions secret `LEGION_RELEASE_SIGNING_KEY` | Repo admins only |

The public key is compiled into every `legion` binary. The matching private
key never leaves GitHub Actions — it is referenced only by the
`manifest-sign` job in `.github/workflows/release.yml`.

## Verifying a release (end users)

`legion gateway install` verifies automatically. To inspect manually:

```bash
# Fetch the manifest and its signature.
curl -O https://raw.githubusercontent.com/dawnswwwww/uselegion/releases/stable/manifest.json
curl -O https://raw.githubusercontent.com/dawnswwwww/uselegion/releases/stable/manifest.json.sig

# Verify against the public key baked into the source.
# (The exact bytes are in crates/legion-protocol/src/manifest.rs.)
```

A mismatch means the manifest was not signed by the Legion release key — do
not install the gateway from it.

## Rotating the key

1. Generate a new Ed25519 keypair locally:
   ```bash
   head -c 32 /dev/urandom | xxd -p -c 64   # → new seed (save privately)
   ```
   Derive the matching public key with `ed25519-dalek` (see
   `scripts/` for a derive helper) or any Ed25519 tool.
2. Add the new private seed as the `LEGION_RELEASE_SIGNING_KEY` secret
   (Settings → Secrets and variables → Actions).
3. Replace `STABLE_RELEASE_PUBLIC_KEY` in
   `crates/legion-protocol/src/manifest.rs` with the new public key bytes.
4. Update the signature tests — they sign with an **ephemeral** test keypair
   injected via `test_manager_with_key`, so they do **not** depend on the
   production key and need no change for a rotation.
5. Tag the next release. The first release after rotation ships the new
   public key; older CLIs will reject the new manifest (expected — users must
   upgrade their CLI).

## Key compromise response

If `LEGION_RELEASE_SIGNING_KEY` is leaked:

1. Rotate the key (above) **immediately** — the new public key ships with the
   next CLI release.
2. Force-push the `releases/stable` branch with a manifest signed by the new
   key, and delete/replace any tampered GitHub Release assets.
3. Document the incident here with dates.

## Why Ed25519

Ed25519 gives short signatures (64 bytes), fast verification, and is already
a dependency of the CLI (`ed25519-dalek`). It matches the design of other
signed-release systems (e.g. Sigstore, TUF) without introducing a new trust
root.
