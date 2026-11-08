# Release publishing

Push the reviewed source to `release/mmx-<version>` (for example `release/mmx-0.1.0`) to publish that version. The release workflow also supports manual dispatch once it is present on the default branch. The workflow uses repository `GITHUB_TOKEN` permissions to publish packages and create the GitHub release. It refuses an existing GitHub release version. No workstation build or push is required.

The workflow builds the eight core images natively on Linux amd64 and arm64, merges their manifests, and records immutable manifest digests. It resolves NATS, Valkey, QuestDB and the Helm smoke-test image to digests too. The packaged chart contains these pins in its defaults; the release Compose file and the Compose file embedded in each CLI contain the same pins.

The published OCI chart is `oci://ghcr.io/<lowercase-owner>/mmx/charts/mmx`, selected with `--version <version>`. GHCR packages must be publicly readable for anonymous installations; verify package visibility and organization package policies before announcing a release. The workflow adds source/revision/version image labels to associate packages with the repository.

GitHub release assets include:

- `mmx-<version>-linux-amd64.tar.gz`, `mmx-<version>-linux-arm64.tar.gz`, and `mmx-<version>-darwin-arm64.tar.gz`.
- `mmx-<version>.tgz`, the pinned Helm chart.
- `image-digests.json` and `image-digests.tsv`, generated from published manifests rather than checked-in placeholder hashes.
- `compose.yaml`, `quickstart.yaml`, `install.sh`, and `SHA256SUMS`.

Linux CLI binaries use musl for a static executable. macOS binaries target Apple silicon. Each archive contains only `mmx`; the installer verifies its SHA256 checksum before installing into `~/.local/bin`, without sudo. Download and inspect `install.sh`, then run `sh install.sh <version>`. Set `MMX_REPOSITORY=owner/repository` when installing from a renamed repository or fork. Checksums establish download integrity against the selected GitHub release; they are not an independent publisher signature.

`prepare.mjs` stages a copied chart and pinned Compose file from a complete tab-separated digest inventory. It rejects absent or malformed digests. `version-cli.mjs` updates the staged CLI package and lockfile version before the release build. Neither script publishes anything.
