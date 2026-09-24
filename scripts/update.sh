#!/usr/bin/env bash
# Update an installed p1 from the release channel: `install.sh --latest`, plus any
# further flags verbatim (e.g. `scripts/update.sh --prefix /opt/p1`).
set -euo pipefail
exec "$(dirname "$0")/install.sh" --latest "$@"
