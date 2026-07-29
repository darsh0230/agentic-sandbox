# Releasing

Tagged releases publish the runtime image to GitHub Container Registry (GHCR)
and create GitHub release notes. Docker Hub is not used, so no Docker Hub login
or secret is required.

## Before tagging

1. Update the version in `Cargo.toml` and refresh `Cargo.lock` if the release
   number changed.
2. Move relevant entries from `[Unreleased]` in `CHANGELOG.md` into a dated
   version section.
3. Confirm the intended commit is on the default branch and CI is green.
4. Run the local checks from [Contributing](../CONTRIBUTING.md).

## Publish

Create and push an annotated semantic-version tag:

```bash
git tag -a v0.1.0 -m "Agentic Sandbox v0.1.0"
git push origin v0.1.0
```

The `Publish container image` workflow then:

1. builds the `linux/amd64` image;
2. publishes version, minor, commit-SHA, and eligible `latest` tags to GHCR;
3. attaches BuildKit provenance and an SBOM; and
4. creates a GitHub release with generated notes if one does not already exist.

Use the actual version being released rather than copying `v0.1.0` for later
releases.

## First-package visibility

GHCR packages can initially be private. After the first successful publish,
open the `agentic-sandbox` package on the `darsh0230` GitHub account, choose
**Package settings**, and change visibility to **Public** if the image is intended
for anonymous quick-start pulls. Confirm the package is linked to this
repository.

Making a GHCR package public cannot be undone through the normal visibility
controls, so verify the image and metadata before changing this setting.

## Verify

```bash
docker pull ghcr.io/darsh0230/agentic-sandbox:v0.1.0
AGENTIC_SANDBOX_IMAGE=ghcr.io/darsh0230/agentic-sandbox:v0.1.0 \
  docker compose --profile user up agentic-sandbox-user
```

In another terminal:

```bash
curl \
  -H 'Authorization: Bearer change-me-dev-token' \
  http://127.0.0.1:8080/api/health
```

After verification, stop the profile:

```bash
docker compose --profile user down
```

