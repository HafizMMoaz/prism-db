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
