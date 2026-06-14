# Cutting a release

PrismDB's binaries are built and published by [dist](https://opensource.axo.dev/cargo-dist/)
(formerly cargo-dist). Pushing a version tag runs
[`.github/workflows/release.yml`](../../.github/workflows/release.yml), which
builds the `prismdb` package for every target, generates the installers, and
publishes a GitHub Release. The release configuration lives in
[`dist-workspace.toml`](../../dist-workspace.toml).

## What a release produces

For each tag `vX.Y.Z`, the GitHub Release gets:

- archives (`.tar.xz` / `.zip`) per target, each containing `prismd`,
  `prism-shell`, `prism-fsck`, `prism-dump`;
- a shell installer (`prismdb-installer.sh`) and a PowerShell installer
  (`prismdb-installer.ps1`);
- a Windows `.msi`;
- a Homebrew formula pushed to the `HafizMMoaz/homebrew-prism` tap;
- SHA-256 checksums and a `dist-manifest.json`.

Targets: Linux `x86_64`/`aarch64`, macOS `x86_64`/`aarch64`, Windows `x86_64`.

A separate workflow,
[`linux-packages.yml`](../../.github/workflows/linux-packages.yml), also builds
the native **`.deb`** (cargo-deb) and **`.rpm`** (cargo-generate-rpm) — each
registering `prismd` as a systemd service — and attaches them to the same
release. It runs on `release: published`, and can be dispatched manually
(`ref` = code to build, `release_tag` = release to upload to) to (re)build for an
existing tag. The metadata lives in `crates/prism-cli/Cargo.toml`; the service
unit/config and Debian maintainer scripts are in [`deploy/`](../../deploy).

## One-time setup

1. **Homebrew tap repo.** Create an empty public repo `HafizMMoaz/homebrew-prism`.
   dist pushes the formula there on each release; users then
   `brew install HafizMMoaz/prism/prismdb`.
2. **Tap push token.** The default `GITHUB_TOKEN` cannot push to another repo, so
   add a repo secret `HOMEBREW_TAP_TOKEN` — a fine-grained PAT with `contents:write`
   on `homebrew-prism`. (If you skip the tap, drop `publish-jobs`/`tap` from
   `dist-workspace.toml` and re-run `dist generate`.)

## Releasing

1. Bump the workspace version in the root `Cargo.toml` (`[workspace.package] version`).
2. Commit, and tag:

   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```

3. Watch the **Release** workflow. When it finishes, the install commands in the
   README resolve against the new release.

Use a pre-release tag (e.g. `v0.1.0-rc.1`) to validate the pipeline without a
"latest" release; dist marks `-rc`/`-alpha`/`-beta` tags as pre-releases.

## Publishing the Node SDK (npm)

The TypeScript client in [`sdks/node`](../../sdks/node) (`@prismdb/client`) is
published to npm by [`.github/workflows/npm-publish.yml`](../../.github/workflows/npm-publish.yml).
It publishes the version in `sdks/node/package.json`, so **bump that version
alongside the Cargo version when cutting a release**. It runs when a GitHub
Release is published, and can be dispatched manually (Actions → **Publish Node
SDK** → Run workflow → ref `main`) — useful for the first publish, or when a
release tag predates a packaging fix. The job builds, tests, and runs `npm
publish --provenance --access public`.

One-time setup:

1. **npm org / scope.** `@prismdb/client` is a scoped package; create the
   `prismdb` organization on npmjs.com (free for public packages). To publish
   unscoped instead, rename `name` in `sdks/node/package.json` (e.g.
   `prismdb-client`) and drop `publishConfig.access`.
2. **`NPM_TOKEN` secret.** On npmjs.com create an **automation** access token with
   publish rights to the `@prismdb` scope, and add it as a repo secret
   `NPM_TOKEN` on `HafizMMoaz/prism-db`.

To publish by hand instead: `cd sdks/node && npm publish --access public` (after
`npm login`).

## Package repository (apt / yum)

[`pages-repo.yml`](../../.github/workflows/pages-repo.yml) builds GPG-signed APT
and YUM repositories from **every** released `.deb`/`.rpm` and publishes them to
GitHub Pages (`https://hafizmmoaz.github.io/prism-db/`), so users can
`apt install prismdb` / `dnf install prismdb`. It runs on `release: published`
(after the packages are attached) and is dispatchable. A `verify-apt` job installs
from the freshly published repo to confirm the signature and service.

One-time setup (already done): GitHub Pages source set to **GitHub Actions**
(`gh api -X POST repos/<owner>/<repo>/pages -f build_type=workflow`); a dedicated
GPG signing key with its private half in the `GPG_PRIVATE_KEY` secret and its
public half committed at `deploy/prismdb-archive-keyring.asc` (served as the repo
key). To rotate the key, regenerate it, replace the secret and that file.

## Changing the build

Edit `dist-workspace.toml` (installers, targets, tap, …) and **regenerate** CI —
never hand-edit `release.yml`:

```sh
dist init      # interactive; or edit dist-workspace.toml directly
dist generate  # rewrites .github/workflows/release.yml and the MSI definitions
dist plan      # preview the artifacts a release would produce
```

dist is a standalone tool (install it from <https://opensource.axo.dev/cargo-dist/>);
it does not need to compile against this project's pinned toolchain.
