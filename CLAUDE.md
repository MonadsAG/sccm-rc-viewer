# CLAUDE.md

Notes for Claude Code (and humans) working in this repository. Build setup
and checks: [CONTRIBUTING.md](CONTRIBUTING.md). macOS releases:
[packaging/macos/RELEASING.md](packaging/macos/RELEASING.md).

## Release notes are required in every pull request

Every PR description needs a `## Release notes` section (it is in
`.github/PULL_REQUEST_TEMPLATE.md`). The `pr-release-notes` check fails
without it.

- Write 1–5 bullets **for users of the app**, in English: what changes for
  them, not how the code changed.
- Write `None` when nothing changes for users (CI, refactoring, tests, docs
  for contributors).
- The text is not just for GitHub: the release workflow collects these
  sections into the draft release, and `publish-ghcr.yml` ships the published
  text to the viewer, which shows it in the update sheet. Plain Markdown
  only: bullets, an optional `###` heading, `**bold**`, links (shown as
  text).

Good:

```markdown
## Release notes

- The update sheet shows what's new in the offered version.
- Fixed the viewer hanging when a till drops the connection during logon.
```

Not useful: `- Refactor sheet.rs`, `- Fix #12`, `- Bump deps`, or the PR
title repeated. For such PRs write `None`.
