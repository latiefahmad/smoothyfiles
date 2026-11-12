# Releasing

Releases are tag-driven. The version number comes from the tag name, and each
release gets its changelog generated automatically.

## How to release

1. Make sure `main` is green on CI and in the state you want to ship.
2. Pick the next version:
   - `smoothyfiles-v1.0.1` — bug fixes
   - `smoothyfiles-v1.1.0` — new features, backwards compatible
   - `smoothyfiles-v2.0.0` — breaking changes
3. Tag and push:

   ```powershell
   git tag smoothyfiles-v1.0.1
   git push origin smoothyfiles-v1.0.1
   ```

4. The Release workflow then builds, packs (Setup.exe, portable zip, update
   packages), publishes a **new** GitHub release, and fills its body with the
   commits since the previous tag plus a compare link. Installed copies
   auto-update to it through Velopack.

## Rules

- **Never re-push a tag that has already shipped.** The version is stamped
  into the installer and the update feed; overwriting it breaks delta updates
  and confuses installed copies. Fix forward with the next version instead.
- Write commit messages as user-facing summaries (what changed and why) —
  they become the release changelog verbatim.
- Tag only from `main` after CI is green.
