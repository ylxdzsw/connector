# Native releases

The native build is independent of the Cloudflare Worker. This repository
contains no gateway service. Use the checked-in lockfile and already-installed
toolchains; these scripts do not install anything or deploy services.

```sh
./scripts/build-release.sh
```

This builds `target/x86_64-unknown-linux-musl/release/connector-client` and
checks it has no ELF interpreter or dynamic dependencies. An installed
alternative musl target may be selected with `CONNECTOR_CLIENT_TARGET`.
The x86-64 packaging recipe below requires the x86-64 artifact.

Build Windows separately on a native Windows runner with PowerShell 7:

```powershell
cargo build --release --locked --target x86_64-pc-windows-msvc --bin connector-client
```

The CI Windows job runs all-target checks, Clippy and tests, including a real
native WebSocket/MCP fixture exercising PowerShell, literal stdin, patching,
notes and output bounds. It then builds the release and checks a real public
WSS connection using a random, unregistered credential: successful certificate
validation followed by HTTP 401 is expected. No live controller/client token
is stored in CI.

After pushing the intended commit, wait for its CI run to succeed and download
the Windows artifact into a new directory:

```sh
gh run list --repo ylxdzsw/connector --commit COMMIT_SHA
gh run watch RUN_ID --repo ylxdzsw/connector --exit-status
gh run download RUN_ID --repo ylxdzsw/connector \
  --name connector-client-windows-x86_64 --dir target/ci/RUN_ID
```

The artifact contains `connector-client.exe` and `provenance.json`. Verify the
run's commit and conclusion, then verify the provenance's commit, MSVC target,
and SHA-256 against the downloaded executable before packaging it. Build Linux
locally from the same clean commit. No script attempts a Windows cross-build.

## Stage a release

Supply existing, tested, nonempty native artifacts. Windows is optional: omit
`--windows` for a Linux-only release rather than publishing an incompatible old
executable. Missing platforms remain unavailable at the download routes.
The package script
copies bytes and computes SHA-256; it does not prove their architecture,
platform, static linkage, or provenance. Verify those before packaging.

```sh
python3 scripts/package-release.py 2026.11.04 \
  --linux target/x86_64-unknown-linux-musl/release/connector-client \
  --windows path/to/connector-client.exe \
  --output target/packages/2026.11.04
```

Versions are 1–64 ASCII letters, digits, dots, underscores, or hyphens,
starting with a letter or digit; path separators and traversal components
are not allowed. Use a new version for changed artifacts. The staging
directory must not already exist (even for the same version). No file is
overwritten. A failed/interrupted package may leave partial staging files;
inspect them and use a new staging directory rather than reusing them.

Output:

```text
target/packages/2026.11.04/
  releases/2026.11.04/
    connector-client-linux-x86_64
    connector-client-windows-x86_64.exe
    manifest.json
  stable.json
```

Both JSON files have the same schema; checksums are lowercase hexadecimal
SHA-256 of the copied binary bytes:

```json
{
  "version": "2026.11.04",
  "artifacts": {
    "linux-x86_64": {
      "key": "releases/2026.11.04/connector-client-linux-x86_64",
      "sha256": "<64 hex characters>"
    },
    "windows-x86_64": {
      "key": "releases/2026.11.04/connector-client-windows-x86_64.exe",
      "sha256": "<64 hex characters>"
    }
  }
}
```

`stable.json` is written last, after the supplied binaries and `manifest.json` are
complete. Local packaging publishes nothing remotely. Version directories
are immutable by release policy and the script's refusal to reuse staging
paths, not by filesystem immutable flags. The mutable remote pointer is
`stable.json`; never mutate a published `releases/<version>/` object.

## R2 upload (operator action, not part of packaging)

Use the new Worker project's installed Wrangler and configured R2 bucket.
Confirm the account/bucket, inspect existing remote keys, and ensure the
version is unused before uploading. Stop on any existing immutable key;
Wrangler `put` can overwrite, so it is not itself an immutability guard.
Inspect the current `stable.json` and retain it for rollback before replacing
that pointer. Authenticate through the platform tooling, not a native-client
credential environment variable.

Example commands, run only after that review, from the Worker project. This
example includes Windows; omit its filename from the upload loop for a Linux-only
release (as in the deployed `2026.11.04-1` release):

```sh
stage=/absolute/path/to/target/packages/2026.11.04
version=2026.11.04
bucket=YOUR_RELEASE_BUCKET
for file in connector-client-linux-x86_64 connector-client-windows-x86_64.exe manifest.json; do
  npx --no-install wrangler r2 object put "$bucket/releases/$version/$file" \
    --remote --file "$stage/releases/$version/$file" \
    --cache-control 'public, max-age=31536000, immutable' || exit 1
done
```

Download/read back the uploaded artifacts and verify their SHA-256 against
the staged manifest. Only after every immutable object is verified, publish
the stable pointer **last**:

```sh
npx --no-install wrangler r2 object put "$bucket/stable.json" \
  --remote --file "$stage/stable.json" --content-type application/json \
  --cache-control 'no-cache'
```

Read back `stable.json` and verify the public download routes. A failed
artifact upload must never advance stable. Rollback republishes a previously
verified version's manifest as `stable.json`, not modified binaries.

Worker/bootstrap integration must resolve the platform through this manifest,
fetch the exact immutable key, and check SHA-256 before executing. Checksums
protect byte integrity relative to the trusted manifest; they are not a
signature or protection against a compromised publisher. Serve the stable
pointer with revalidation/no-cache and immutable versioned binaries with long
cache lifetimes. Bootstrap scripts pass the named link with `--url` and the
separate opaque credential with `--token`. Do not log or normalize the key.
The named-URL release must be cut over together with the matching bootstraps.
