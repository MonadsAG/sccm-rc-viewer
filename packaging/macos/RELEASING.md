# Releasing the macOS build

`.github/workflows/release-macos.yml` builds the universal `.dmg` when a tag
`v*` is pushed and attaches it to a **draft** GitHub release.

## Tags

| Tag            | Meaning                                                  | Release     |
|----------------|----------------------------------------------------------|-------------|
| `v0.9.13`      | the version in `crates/sccm-rc-viewer/Cargo.toml`         | normal      |
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

```sh
git checkout main && git pull
git tag -a v0.9.13-m1 -m "macOS build 0.9.13-m1"
git push origin v0.9.13-m1
```

1. Approve the `release` deployment in the run.
2. Download the `.dmg` from the draft release and open it on a Mac that has
   never seen the app. Signed builds must open without a Gatekeeper warning
   (`spctl -a -vv -t install SCCM-RC-*.dmg` says "Notarized Developer ID").
3. Edit the generated notes and publish the draft.

A dry run without a tag (Actions → Release (macOS) → Run workflow) builds
the `.dmg` as a workflow artifact and creates no release.
