# Releasing nairobi2

A signed release APK is built automatically when you push a `v*` tag — the
`release-apk` job in [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)
builds it in the container and attaches it to a GitHub Release. Debug APKs are
built on every pull request (ephemeral debug signing, nothing to configure).

## One-time: create the signing key

The signing key must persist across releases — Android only installs an update
if it's signed with the **same** key as the installed build. Generate it once
with the builder image (no host JDK needed):

```sh
NAIROBI_KEYSTORE_PASSWORD='<store-pass>' \
NAIROBI_KEY_ALIAS='nairobi' \
NAIROBI_KEY_PASSWORD='<key-pass>' \
./build.sh keystore        # -> release-signing/nairobi-release.jks
```

**Keep `release-signing/nairobi-release.jks` and both passwords safe and
private.** Losing them means you can never ship an update to installed users.
The `release-signing/` directory is git-ignored.

## One-time: configure GitHub secrets

In the repo: *Settings → Secrets and variables → Actions → New repository secret*.

| Secret | Value |
|---|---|
| `RELEASE_KEYSTORE_BASE64` | `base64 -w0 release-signing/nairobi-release.jks` |
| `RELEASE_KEYSTORE_PASSWORD` | the keystore password |
| `RELEASE_KEY_ALIAS` | the key alias (e.g. `nairobi`) |
| `RELEASE_KEY_PASSWORD` | the key password |

```sh
base64 -w0 release-signing/nairobi-release.jks   # paste into RELEASE_KEYSTORE_BASE64
```

## Cut a release

```sh
git tag v0.1.0
git push origin v0.1.0
```

CI then: runs the host test + clippy gate, builds the **signed release APK**
(`versionName` from the tag, `versionCode` from the run number), and publishes
it to a GitHub Release named `nairobi v0.1.0` with auto-generated notes, with
`nairobi-v0.1.0.apk` attached.

## Building a signed release locally

```sh
NAIROBI_KEYSTORE=release-signing/nairobi-release.jks \
NAIROBI_KEYSTORE_PASSWORD='<store-pass>' \
NAIROBI_KEY_ALIAS='nairobi' \
NAIROBI_KEY_PASSWORD='<key-pass>' \
./build.sh release         # -> dist/nairobi-release.apk
```

A release build with no keystore fails loudly rather than producing an
unsigned APK that Android refuses to install.
