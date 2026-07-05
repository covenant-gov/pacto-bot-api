# Changelog

All notable changes to the `pacto-bot-sdk` Python package will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.1] - 2026-07-04

### Fixed

- `HttpTransport` now sends `X-Pacto-Handler-Id` for `handler.response` frames, matching the mutating methods (`agent.send_dm`, `agent.set_profile`, `agent.error`). This fixes a daemon-side correlation failure when a Dockerized bot replies over HTTP: without the header the daemon could not map the reply to the live handler registration and rejected it with "handler not registered".
- Updated the `HttpTransport` docstring to document that `handler.response` requires the handler ID header.

### Added

- Regression test asserting that `handler.response` frames include `X-Pacto-Handler-Id` when a `handler_id` is set.
