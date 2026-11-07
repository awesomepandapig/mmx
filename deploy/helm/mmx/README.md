# MMX Helm chart

Deploys the matching cluster, REST gateway, market data, settlement outbox, trade recorder,
WebSocket server, operator console, NATS, Valkey, and QuestDB. Marketing UI is optional.

Use the [installation guide](https://docs.mmx.exchange/installation/helm) and review
[production operations](https://docs.mmx.exchange/guides/production) before exposing services.
The gateway currently trusts your game server's account header; restrict network access to it.

## Install a published release

```sh
helm upgrade --install mmx oci://ghcr.io/awesomepandapig/mmx/charts/mmx \
  --version 0.1.0-rc.2 --namespace mmx --create-namespace \
  --wait --timeout 10m
helm test mmx --namespace mmx --logs
```

The current release candidate is `0.1.0-rc.2`. Add `--values production-values.yaml` for your own overrides.
Choose a version that exists in [GitHub releases](https://github.com/awesomepandapig/mmx/releases).
The version in source is not evidence that a release has been published. Keep credentials in
Kubernetes Secrets rather than values files. `values-local.yaml` permits development placement;
`values-performance.yaml` requires nodes prepared for dedicated CPUs.

## Artifact Hub

Artifact Hub indexes the published OCI chart directly. Register a **Helm charts** repository
named `mmx`, display name `MMX`, with this URL:

```text
oci://ghcr.io/awesomepandapig/mmx/charts/mmx
```

Registration is a one-time operation in the Artifact Hub control panel; subsequent published
versions are indexed automatically. Use an Artifact Hub API key ID and secret to automate
registration. Store them locally rather than committing them. Listing does not require moving
the chart to another registry or using `helm repo add`.

## Validate and package source

Run from the repository root:

```sh
deploy/scripts/validate.sh
helm lint deploy/helm/mmx
helm template mmx deploy/helm/mmx --namespace mmx > /tmp/mmx.yaml
helm package deploy/helm/mmx --destination /tmp
```

## Publish

The existing `.github/workflows/release.yml` builds native images for both architectures,
locks image digests, stamps the chart version and application version, validates and packages
the chart, builds the CLI, then pushes the chart to GHCR and attaches it to a GitHub release.
Run that workflow against the reviewed source revision with a new, unused SemVer:

```sh
gh workflow run release.yml --ref main -f version=0.1.0-rc.2
```

For the first release, push the reviewed revision as `release/mmx-0.1.0-rc.1`; this triggers the workflow
even before it is present on the default branch. Once merged, workflow dispatch can use `main`. The workflow needs Actions enabled
and its `GITHUB_TOKEN` needs `contents: write` and `packages: write` in the publish job. New GHCR
packages default to private; set all eight core image packages and the chart package to public in
GitHub package settings (profile **Packages → package → Package settings → Change visibility → Public**).
The package names are `mmx/node`, `mmx/matching-engine`, `mmx/api`, `mmx/mmx`, `mmx/outbox`,
`mmx/websocket`, `mmx/market-data`, `mmx/trade-reporter`, and `mmx/charts/mmx`.
The publish job verifies anonymous manifest access and pulls/renders the chart
without registry credentials before publishing the staged draft GitHub release. The draft and its
assets remain available to maintainers while package visibility is being configured. If it fails on visibility, change the
package settings and rerun that job; do not rebuild the successful image jobs. The pipeline currently publishes checksums, not cryptographic
release signatures.

The destination is `oci://ghcr.io/awesomepandapig/mmx/charts/mmx`; Helm appends the chart name when
pushing to `oci://ghcr.io/awesomepandapig/mmx/charts`. This is an OCI package, so consumers use its
OCI URL directly rather than `helm repo add`. See [Helm OCI registries](https://helm.sh/docs/topics/registries/).

### First workflow publication

GitHub restricts release creation and publication when the target commit changes
`.github/workflows/` relative to the default branch. The job's `GITHUB_TOKEN` cannot receive the
required workflow-write permission. Merge matching workflow changes into the default branch before
normal workflow dispatch, or bootstrap the first release using a maintainer's authenticated GitHub CLI
with the OAuth `workflow` scope. Reuse the successful run's artifacts and verify their checksums and
anonymous registry access; rebuilding them does not fix an authorization error. See
[GitHub release API permissions](https://docs.github.com/en/rest/releases/releases#create-a-release).

`release.yml` owns versioned image tags and chart publication. The general `deploy.yml` workflow
validates pull requests and publishes commit-addressed images on `main`; it must not retag release
images from an unversioned rebuild.
