# Release chain and Android completion gate

Every Haider release must include a signed release Android APK and its SHA-256 sidecar.
A desktop release, successful `release` workflow, successful `android-apk` workflow, or
uploaded Actions artifact alone does not establish release completion.

1. Run the required local platform checks and obtain separate Astra SHIP on the candidate.
2. Require `ci`, `xplat-check`, and `ship-gate` success on the exact candidate SHA. The
   pre-tag helpers are `scripts/release/require-evidence.sh` and `find-evidence.py`.
3. The release owner pushes the immutable version tag; never manually dispatch `release`.
4. Follow both tag-triggered workflows on the peeled tag SHA. `android-apk` checkpoints
   native outputs per ABI, then packages/tests/signs. Rerun failed jobs after eviction.
5. `release` requires the signed APK artifact and checksum before publishing. Immediately
   after upload, it queries the release's assets and fails if the exact APK/sidecar pair is
   absent, empty, or not uploaded.
6. Before installation/promotion or declaring complete, the orchestrator must independently
   run the same live post-publication gate and retain stdout, stderr and exit code:

   ```sh
   python3 scripts/release/require-android-asset.py v0.0.971 \
     57cda337d9c3490cf9852c34291836fb87a091b4 --repo Rizzist/haider-agent
   ```

   Substitute the new tag and full peeled SHA for future releases. The script verifies the
   remote tag commit and queries `gh release view --json tagName,isDraft,url,assets`.
   Required names are `haider-vVERSION-android.apk` and the same name plus `.sha256`.
   Any failure means **RELEASE INCOMPLETE**, with no promotion/completion claim. The gate
   is read-only and does not dispatch workflows, upload assets, or mutate tags.
7. Download the APK and sidecar, verify the checksum and APK `versionName`, and retain
   signing-scheme/static validation and build provenance. The asset-presence gate proves
   attachment; it does not substitute for APK/signing or device verification.

For an already published release missing Android assets, preserve the tag and all existing
assets. Prefer the signed Actions artifact from that tag; add only the missing APK/sidecar,
without `--clobber`, then repeat the live gate. A permitted local fallback must disclose
source tag/SHA, builder, date and hashes in release notes/evidence and use the existing key
outside all worktrees. Never inspect, print, copy, export, modify, or replace signing material.
