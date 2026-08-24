#!/usr/bin/env bash
set -euo pipefail

requested_version="${AGENTIC_API_RELEASE_VERSION:-}"
if [[ "$requested_version" != "0.4.0" ]]; then
  echo "release-python.yml is a 0.4.0 build-only workflow; other versions are rejected" >&2
  exit 1
fi
