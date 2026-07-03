# Docker support files

This directory holds `relay-config.toml`, the configuration file mounted by the
`nostr-relay` service in the generated `docker-compose.yml`.

The relay and bunker Docker images are maintained in the `pacto-dev-env`
repository and published to GitHub Container Registry:

- `ghcr.io/covenant-gov/pacto-dev-env/nostr-relay:main`
- `ghcr.io/covenant-gov/pacto-dev-env/nip46-bunker:main`

The generated compose file pulls these images automatically, so you no longer
need to copy Dockerfiles or build them locally. If the packages are private,
ask a repository admin to make them public in GitHub Container Registry
settings.
