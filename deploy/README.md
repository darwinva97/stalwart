# Stalwart — Free Edition (AGPL) deploy

This directory ships the **free edition**: every formerly-enterprise capability
(multi-tenancy, white-label branding, advanced telemetry incl. live tracing/metrics,
delivery history, and horizontal scalability via read-replicas) reimplemented as
free AGPL code and compiled **without the `enterprise` feature**. No license key.

## How it works

1. **CI/CD** (`.github/workflows/build-free-edition.yml`) compiles the binary and
   publishes a tarball to a GitHub Release:
   - Push to the `free-edition` branch (or run the workflow manually) → updates the
     rolling **`free-latest`** pre-release.
   - Push a `vX.Y.Z` tag → publishes a versioned release.
   - Artifact: `stalwart-x86_64-unknown-linux-gnu.tar.gz` (+ `.sha256`).

2. **Install on a VPS** — one command, as root:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/darwinva97/stalwart/free-edition/deploy/install-free-edition.sh | sh
   ```

   It downloads the CI artifact, installs `/usr/local/bin/stalwart`, creates the
   `stalwart` service account and data dirs, writes a systemd unit, and starts it.
   With no config yet it boots in **bootstrap mode on port 8080** — finish setup at
   `http://<server-ip>:8080/admin` using the bootstrap admin written to
   `/etc/stalwart/stalwart.env`.

### Overrides

```sh
REPO=darwinva97/stalwart RELEASE_TAG=v1.0.0 sh install-free-edition.sh
```

## Features built into the artifact

Default: `sqlite postgres mysql rocks s3 redis` (change via the workflow's
`features` input). The `enterprise` feature is never enabled.

## Local testing (Docker/Podman)

For a local run against Postgres (incl. read-replicas), point `--config` at a
`DataStore` JSON, e.g.:

```json
{ "@type": "PostgreSql", "host": "127.0.0.1", "port": 5433,
  "database": "stalwart", "authUsername": "stalwart",
  "authSecret": { "@type": "Value", "secret": "stalwart" },
  "useTls": false, "allowInvalidCerts": true,
  "readReplicas": { "0": { "host": "127.0.0.1", "port": 5433,
     "database": "stalwart", "authUsername": "stalwart",
     "authSecret": { "@type": "Value", "secret": "stalwart" } } } }
```

```sh
STALWART_HOSTNAME=localhost STALWART_RECOVERY_ADMIN=admin:pass \
  stalwart --config ./config.json
```
