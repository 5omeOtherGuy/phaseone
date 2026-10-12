//! The subscription connectors' own checks (ADR-0200). The suites live in `tests/`: they read
//! the connector route files by path from `connectors/subscriptions/routes` and compose them
//! through the host with p1-host's test harness. This library is empty; a package needs one
//! target besides its tests.
