<!-- Thanks for contributing to sccm-rc! -->

## Summary

<!-- What does this PR change, and why? -->

## Release notes

<!-- Required (checked in CI). 1-5 bullets for users of the app, in English:
     what changes for them. They are shown in the app's update sheet.
     Write "None" for changes users don't see (CI, refactoring, tests).
     See CLAUDE.md. -->

## Related issues

<!-- e.g. Closes #123 -->

## Checklist

- [ ] `cargo build --workspace` succeeds (gnu toolchain via `env.ps1`)
- [ ] `cargo test --workspace` passes
- [ ] `cargo fmt --all -- --check` is clean
- [ ] `cargo clippy --workspace --all-targets` introduces no new warnings
- [ ] No secrets / real hostnames / IPs / captures added
- [ ] Docs updated if behavior or protocol changed

## Testing

<!-- How was this verified? If tested against a live target, note that (without
     including any real hostnames, IPs, or session content). -->
