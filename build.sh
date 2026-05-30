#!/usr/bin/env bash
# ============================================================================
# Skythe Build + Package — default build workflow
# ============================================================================
# Builds the release binary and packages it into ~/Music/Skythe-v{VERSION}/
# Usage:  ./build.sh [--install]
#         --install    also run the installer after packaging
# ============================================================================
set -euo pipefail

APP_NAME="skythe"
BINARY_NAME="player-ui"
FEATURES="gui_egui"

# Colours (if supported)
if [ -t 1 ] && command -v tput >/dev/null 2>&1; then
    GREEN=$(tput setaf 2)
    CYAN=$(tput setaf 6)
    BOLD=$(tput bold)
    RESET=$(tput sgr0)
else
    GREEN=""; CYAN=""; BOLD=""; RESET=""
fi

echo ""
echo "${BOLD}${CYAN}══════════════════════════════════════════${RESET}"
echo "${BOLD}${CYAN}   Skythe Build & Package                 ${RESET}"
echo "${BOLD}${CYAN}══════════════════════════════════════════${RESET}"
echo ""

# ── 1. Extract version from Cargo.toml ─────────────────────────────────────
if [ ! -f player-ui/Cargo.toml ]; then
    echo "Error: player-ui/Cargo.toml not found. Run from the project root."
    exit 1
fi
APP_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' player-ui/Cargo.toml | head -1)"
if [ -z "$APP_VERSION" ]; then
    echo "Error: could not extract version from player-ui/Cargo.toml"
    exit 1
fi
echo "  ${BOLD}Version:${RESET} ${GREEN}${APP_VERSION}${RESET}"

# ── 2. Build release ────────────────────────────────────────────────────────
echo "  ${BOLD}Building:${RESET} cargo build --release -p ${BINARY_NAME} --features ${FEATURES}"
echo ""
cargo build --release -p "${BINARY_NAME}" --features "${FEATURES}"
echo ""

# ── 3. Verify binary exists ────────────────────────────────────────────────
BINARY="target/release/${BINARY_NAME}"
if [ ! -f "$BINARY" ]; then
    echo "Error: build succeeded but binary not found at ${BINARY}"
    exit 1
fi

BINARY_SIZE=$(stat -c%s "$BINARY" 2>/dev/null || stat -f%z "$BINARY" 2>/dev/null || echo "?")
BINARY_SIZE_HUMAN=$(numfmt --to=iec "$BINARY_SIZE" 2>/dev/null || echo "${BINARY_SIZE}B")
echo "  ${BOLD}Binary:${RESET} ${BINARY} (${GREEN}${BINARY_SIZE_HUMAN}${RESET})"

# ── 4. Package into Music folder ────────────────────────────────────────────
PACKAGE_DIR="${HOME}/Music/${APP_NAME}-v${APP_VERSION}"
mkdir -p "$PACKAGE_DIR"

cp "$BINARY" "${PACKAGE_DIR}/${APP_NAME}"
cp install.sh "$PACKAGE_DIR/"
cp uninstall.sh "$PACKAGE_DIR/"
echo "$APP_VERSION" > "${PACKAGE_DIR}/VERSION"
chmod +x "${PACKAGE_DIR}/${APP_NAME}" "${PACKAGE_DIR}/install.sh" "${PACKAGE_DIR}/uninstall.sh"

echo "  ${BOLD}Package:${RESET} ${PACKAGE_DIR}/"
echo "           ├── ${APP_NAME}        (binary)"
echo "           ├── install.sh"
echo "           ├── uninstall.sh"
echo "           └── VERSION"
echo ""

# ── 5. Optionally install ──────────────────────────────────────────────────
if [ "${1:-}" = "--install" ]; then
    echo "  ${BOLD}Installing...${RESET}"
    cd "$PACKAGE_DIR" && ./install.sh
    cd "$OLDPWD"
else
    echo "  ${BOLD}Tip:${RESET} To install, run:"
    echo "    cd ${PACKAGE_DIR} && sudo ./install.sh"
    echo "  Or re-run with:  ./build.sh --install"
fi

echo ""
echo "${BOLD}${GREEN}✔ Build complete${RESET}"
echo ""