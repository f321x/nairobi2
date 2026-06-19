# Releasing nairobi2

CI ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) does three things:

| Trigger | Job(s) | Result |
| --- | --- | --- |
| Push to `master` | `test` | Compile + `cargo test --workspace` + clippy |
| Pull request | `test`, `debug-apk` | The above, plus an installable **debug** APK (uploaded as a build artifact), signed with an ephemeral debug key |
| Push a tag `v*` | `test`, `release-apk` | The above, plus a **signed release** APK per ABI attached to the tag's GitHub Release |

The debug APK on PRs uses Android's auto-generated debug keystore — nothing to
configure. The **release** APK is signed with a *persistent* key you control,
stored as GitHub Actions secrets.

## One-time setup: the release signing key

Android requires every update to an installed app to be signed with the **same**
key as the version it replaces. Generate this key once and keep it forever —
**back it up; if you lose the keystore or its password you can never ship an
update** that upgrades an existing install (users would have to uninstall first).

### 1. Generate the keystore

```sh
./build.sh keystore
```

This runs the helper inside the builder image (**no host JDK needed**; the
output is chowned back to you). It auto-generates a random 32-bit-strong
password and writes three files to the git-ignored `release-signing/` directory:

- `nairobi-release.jks` — the 4096-bit RSA keystore
- `nairobi-release.jks.base64` — the keystore, base64-encoded (for the secret)
- `secrets.env` — `RELEASE_KEYSTORE_PASSWORD`, `RELEASE_KEY_ALIAS`, `RELEASE_KEY_PASSWORD`

…and prints the exact secret values + `gh secret set` commands to run.

> If you have a JDK on your host you can also run `scripts/gen-release-keystore.sh`
> directly. Either way it refuses to overwrite an existing keystore.

### 2. Store the key in GitHub  ← **the part you must do**

Add **four repository secrets** under *Settings → Secrets and variables →
Actions → New repository secret*, or use the `gh secret set` commands the helper
printed (run from the repo root):

```sh
gh secret set RELEASE_KEYSTORE_BASE64   < release-signing/nairobi-release.jks.base64
gh secret set RELEASE_KEYSTORE_PASSWORD --body "$(grep '^RELEASE_KEYSTORE_PASSWORD=' release-signing/secrets.env | cut -d= -f2-)"
gh secret set RELEASE_KEY_ALIAS         --body "$(grep '^RELEASE_KEY_ALIAS=' release-signing/secrets.env | cut -d= -f2-)"
gh secret set RELEASE_KEY_PASSWORD      --body "$(grep '^RELEASE_KEY_PASSWORD=' release-signing/secrets.env | cut -d= -f2-)"
```

| Secret | Value |
| --- | --- |
| `RELEASE_KEYSTORE_BASE64` | base64 of `nairobi-release.jks` |
| `RELEASE_KEYSTORE_PASSWORD` | the keystore password |
| `RELEASE_KEY_ALIAS` | the key alias (`nairobi` by default) |
| `RELEASE_KEY_PASSWORD` | the key password |

CI decodes `RELEASE_KEYSTORE_BASE64` to a file on the runner, hands it
read-only to the builder container, and Gradle signs `assembleRelease` with it.
The secrets are masked in logs, and the **keystore is never committed**
(`release-signing/`, `*.jks`, `*.keystore`, `secrets.env` are git-ignored).

## Cutting a release

Versioning is derived from the tag, so just tag and push:

```sh
git tag v0.1.0
git push origin v0.1.0
```

- `versionName` = the tag without the leading `v` (e.g. `0.1.0`).
- `versionCode` = the workflow run number (monotonically increasing).

The `release-apk` job builds one signed APK per ABI
(`dist/nairobi-release-arm64-v8a.apk` for phones, `dist/nairobi-release-x86_64.apk`
for emulators), renames each to `nairobi-v0.1.0-<abi>.apk`, and attaches them to
the tag's GitHub Release (created with auto-generated notes). Re-running the job
re-uploads the assets.

## Building a signed release locally

Same builder image as CI:

```sh
NAIROBI_KEYSTORE=release-signing/nairobi-release.jks \
NAIROBI_KEYSTORE_PASSWORD="$(grep '^RELEASE_KEYSTORE_PASSWORD=' release-signing/secrets.env | cut -d= -f2-)" \
NAIROBI_KEY_ALIAS=nairobi \
NAIROBI_KEY_PASSWORD="$(grep '^RELEASE_KEY_PASSWORD=' release-signing/secrets.env | cut -d= -f2-)" \
./build.sh release            # -> dist/nairobi-release-<abi>.apk (one per ABI)
```

Without `NAIROBI_KEYSTORE`, `./build.sh release` refuses to run rather than emit
an unsigned APK that Android won't install.
