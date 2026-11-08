#!/bin/bash
# Build the e1000e driver variants and the diagnostics tool:
#
#   tools/build-driver.sh            both drivers and e1000ediag
#   tools/build-driver.sh release    driver/e1000e.device only
#   tools/build-driver.sh diag       driver/e1000e_diag.device only
#
# e1000e.device       the release driver: no private commands.
# e1000e_diag.device  the diag build: the diagnostics report, tuning and
#                     test commands (for e1000ediag), transmit batching,
#                     jumbo frames from ENV:e1000e_diag/MTU, and the extras
#                     log in RAM:e1000e_diag.log.
#
# Run on Linux (WSL works); see tools/build-package.sh.
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
WHAT="${1:-all}"

build_release() {
    echo "== e1000e.device (release)"
    MAKE_ARGS= bash "$REPO/tools/build-package.sh" driver
}
build_diag() {
    echo "== e1000e_diag.device (diag build)"
    MAKE_ARGS=DIAG=1 bash "$REPO/tools/build-package.sh" driver --features diag
}
build_tool() {
    echo "== e1000e_diag (the settings and diagnostics tool)"
    MAKE_ARGS= bash "$REPO/tools/build-package.sh" diag
}

case "$WHAT" in
    release) build_release ;;
    diag) build_diag ;;
    all) build_release; build_diag; build_tool ;;
    *) echo "usage: tools/build-driver.sh [all|release|diag]"; exit 2 ;;
esac
ls -l "$REPO"/driver/*.device "$REPO"/diag/e1000e_diag 2>/dev/null || true
