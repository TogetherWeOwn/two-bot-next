# Changelog

## Unreleased

### Added

- Port website-contract counter, rank and scheduled-events domain logic and transactional storage, with legacy-shaped read views. Runtime job wiring remains deferred.

### Fixed

- Preserve isolated bot schemas when applying the website contract, without rebinding the public read views.
- Refuse non-test targets before resetting the website-contract acceptance database.
