# Releasing

A release is a signed tag on `main`. The [release workflow](../.github/workflows/release.yml)
then builds, after approval, for every supported platform:

- a static binary per platform (`tolk-<version>-linux-<arch>`) with
  `SHA256SUMS` and the binaries' signed provenance
  (`tolk-<version>.intoto.jsonl`, for offline verification), attached to the
  GitHub release;
- one multi-platform image, `ghcr.io/rbogie/tolk-jmap:<version>` and, for a
  stable release, `:<major>.<minor>` and from 1.0 on `:<major>`, which then
  follow every non-breaking update. Before 1.0 a minor version may break
  (Semantic Versioning), so there is no `:0`. There is no `latest` tag;
- signed build provenance for the binaries and the image, and an SBOM and
  BuildKit provenance inside the image.

Every pull request that changes something for users adds a line to the
`## Unreleased` section of `CHANGELOG.md`, so a release only renames that
section; its text becomes the release notes.

Each platform is compiled once: the binary in the image is the release
download. Version tags follow [Semantic Versioning](https://semver.org);
`v1.2.0-rc.1` makes a pre-release (image tag `1.2.0-rc.1` only).

## Steps

1. In a pull request: set `version` in `Cargo.toml`, and rename the
   `## Unreleased` section of `CHANGELOG.md` to the version, e.g.
   `## 0.2.0 (2026-12-01)` (the release date is required), with a new empty
   `## Unreleased` above it. Merge it.
2. Tag the merge commit on `main` (signed, see `tag.gpgsign`) and push the
   tag:

   ```sh
   git switch main && git pull
   git tag -s v0.1.0 -m "Tolk 0.1.0"
   git push origin v0.1.0
   ```

3. The workflow checks that the tag matches `Cargo.toml` and that the
   changelog entry is dated, then waits: approve it under Actions, in the
   run's `release` environment.
4. After it finishes, check the release page and verify one download:

   ```sh
   gh attestation verify tolk-0.1.0-linux-amd64 --repo RBogie/tolk-jmap
   gh attestation verify oci://ghcr.io/rbogie/tolk-jmap:0.1.0 --repo RBogie/tolk-jmap
   ```

A failed run can be re-run from Actions. A published version is never
replaced: tags are immutable (tag rulesets) and so are releases; fix forward
with a new patch version.

## Updating the build tools

Dependabot updates the Rust toolchain, the base image, Cargo dependencies
and the workflow actions. The Zig and cargo-zigbuild versions in the
`Dockerfile` are updated by hand: change version and both checksums
together (from the projects' release pages).
