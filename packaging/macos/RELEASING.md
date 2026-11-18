# Releasing the macOS build

`.github/workflows/release-macos.yml` builds the universal `.dmg` for a tag
`v*` and attaches it to a **draft** GitHub release. The usual way to get
such a tag is to raise the version and merge that to `main`; see below.

## Tags

| Tag            | Meaning                                                  | Release     |
|----------------|----------------------------------------------------------|-------------|
| `v0.9.13`      | the version in `Cargo.toml` (`[workspace.package]`)      | normal      |
| `v0.9.13-m1`   | a rebuild of that version (fork or downstream), 1, 2, …  | pre-release |

The tag must start with the crate version, otherwise the run fails. The
version shown in Finder stays `0.9.13`. The GitHub run number goes into
`CFBundleVersion`, so every build has a higher build number. The suffix is
part of the `.dmg` name: `SCCM-RC-0.9.13-m1-macos-universal.dmg`.

## One-time setup in the repository

1. Settings → Actions → General: allow Actions (off by default on forks).
2. Settings → Environments → New environment `release`:
   - *Required reviewers*: the maintainers who may release.
   - *Deployment branches and tags*: selected tags only, rule `v*`.
3. Signing, once there is an Apple Developer ID: enter the secrets listed at
   the top of the workflow **in that environment**. Without them the `.dmg`
   is only ad-hoc signed and Gatekeeper blocks it after download.
4. Optional: repository variable `MACOS_BUNDLE_ID` for a distribution under
   its own bundle identifier.

## Making a release

### New version: raise it and merge

1. In a pull request, raise `version` under `[workspace.package]` in
   `Cargo.toml` (e.g. `0.9.13` → `0.9.14`) and run `cargo check` so that
   `Cargo.lock` follows.
2. Merge it to `main`. When CI is green for the merge commit,
   `.github/workflows/tag-on-version-bump.yml` tags it `v0.9.14` and starts
   Release (macOS) for that tag.

Merges that leave the version alone release nothing. If the tag exists
already (for instance an upstream tag that came with a sync), nothing is
tagged; use a rebuild tag instead.

### Rebuild of the same version: tag by hand

```sh
git checkout main && git pull
git tag -a v0.9.13-m2 -m "macOS build 0.9.13-m2"
git push origin v0.9.13-m2
```

To rebuild an existing, still unpublished tag (for instance after the
signing secrets were added), start Actions → Release (macOS) → Run workflow
and choose the tag instead of a branch; the new `.dmg` replaces the one in
its draft release.

### Then, in both cases

1. Approve the `release` deployment in the run.
2. Download the `.dmg` from the draft release and open it on a Mac that has
   never seen the app. Signed builds must open without a Gatekeeper warning
   (`spctl -a -vv -t install SCCM-RC-*.dmg` says "Notarized Developer ID").
3. Edit the generated notes and publish the draft.

A dry run on a branch (Actions → Release (macOS) → Run workflow, branch
`main`) builds the `.dmg` as a workflow artifact and creates no release. macOS runner
minutes count ten times, so build and check the `.dmg` locally first
(`packaging/macos/bundle.sh --universal --dmg`) and use the workflow only for
the actual release.

## Signing credentials

The Developer ID Application certificate has an expiry date (see Apple
Developer → Certificates). Before it runs out, create a new one and replace
`MACOS_CERT_P12_BASE64`, `MACOS_CERT_PASSWORD` and, if the name changed,
`MACOS_SIGN_IDENTITY`. Notarized `.dmg` files released earlier keep opening
after the certificate expires. The App Store Connect API key for
notarization does not expire; revoking it in App Store Connect stops
notarization until a new key is entered.
