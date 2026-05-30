#!/usr/bin/env bash
# ============================================================================
# Skythe Installer — smart, robust, and thorough
# ============================================================================
set -euo pipefail

# ── Ensure we are running inside a visible terminal ─────────────────────────
# If stdout is not a terminal, re-exec inside konsole/gnome-terminal/xterm.
if [ ! -t 1 ]; then
    # Build a properly quoted command string for re-execution
    SELF="$(realpath "$0" 2>/dev/null || readlink -f "$0" 2>/dev/null || echo "$0")"
    CMD="$SELF"
    for arg in "$@"; do
        # Quote arguments that contain spaces or quotes
        printf -v qarg '%q' "$arg"
        CMD="$CMD $qarg"
    done
    # Detect which terminal emulators are available
    if command -v konsole &>/dev/null; then
        exec konsole --hold -e "$SELF" "$@"
    elif command -v gnome-terminal &>/dev/null; then
        exec gnome-terminal -- bash -c "$CMD; echo; echo \"Press Enter to close...\"; read"
    elif command -v xterm &>/dev/null; then
        exec xterm -hold -e "$SELF" "$@"
    elif command -v mate-terminal &>/dev/null; then
        exec mate-terminal -- bash -c "$CMD; echo; echo \"Press Enter to close...\"; read"
    elif command -v lxterminal &>/dev/null; then
        exec lxterminal -e "$SELF" "$@"
    elif command -v urxvt &>/dev/null; then
        exec urxvt -hold -e "$SELF" "$@"
    elif command -v terminator &>/dev/null; then
        exec terminator -e "$SELF" "$@"
    elif command -v xfce4-terminal &>/dev/null; then
        exec xfce4-terminal --hold -e "$SELF" "$@"
    elif command -v alacritty &>/dev/null; then
        exec alacritty -e "$SELF" "$@"
    elif command -v kitty &>/dev/null; then
        exec kitty sh -c "$CMD; echo; echo \"Press Enter to close...\"; read"
    else
        # Fallback: just run as-is (no terminal available)
        :
    fi
fi

# ── Config ──────────────────────────────────────────────────────────────────
APP_NAME="skythe"
APP_DISPLAY="Skythe"
BINARY_NAME="player-ui"
BINARY_PATH="target/release/${BINARY_NAME}"

# ── Derive version (Cargo.toml in project root, or VERSION file in package) ─
APP_VERSION=""
if [ -f player-ui/Cargo.toml ]; then
    APP_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' player-ui/Cargo.toml | head -1)"
elif [ -f VERSION ]; then
    APP_VERSION="$(cat VERSION | tr -d '[:space:]')"
elif [ -f "$(dirname "$0")/VERSION" ]; then
    APP_VERSION="$(cat "$(dirname "$0")/VERSION" | tr -d '[:space:]')"
fi
if [ -z "$APP_VERSION" ]; then
    echo "Error: could not determine version. Run from project root or package directory."
    exit 1
fi

# ── Directories ─────────────────────────────────────────────────────────────
PREFIX="${PREFIX:-/usr/local}"
DESTDIR="${DESTDIR:-}"
BIN_DIR="${DESTDIR}${PREFIX}/bin"
DATA_DIR="${DESTDIR}${PREFIX}/share/${APP_NAME}"
ICON_DIR="${DESTDIR}${PREFIX}/share/icons/hicolor/scalable/apps"
APP_DIR="${DESTDIR}${PREFIX}/share/applications"

# ── Determine the original user's home directory ──────────────────────────
# When running under sudo/pkexec, $HOME points to /root. We need the
# actual user's home for cleaning up ~/.local/… paths.
if [ -n "${SUDO_USER:-}" ]; then
    ORIGINAL_USER_HOME="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
elif [ -n "${PKEXEC_UID:-}" ]; then
    ORIGINAL_USER_HOME="$(getent passwd "$PKEXEC_UID" | cut -d: -f6)"
else
    ORIGINAL_USER_HOME="${HOME}"
fi

# User-local paths (use the original user's home, not root's)
USER_LOCAL_BIN="${ORIGINAL_USER_HOME}/.local/bin"
USER_LOCAL_APP_DIR="${ORIGINAL_USER_HOME}/.local/share/applications"
USER_LOCAL_ICON_DIR="${ORIGINAL_USER_HOME}/.local/share/icons/hicolor/scalable/apps"
USER_LOCAL_DATA_DIR="${ORIGINAL_USER_HOME}/.local/share/${APP_NAME}"

# ── Pre-flight checks ──────────────────────────────────────────────────────
# When run from the package directory, the binary is alongside install.sh
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [ -f "$BINARY_PATH" ]; then
    : # Found at project-root target path
elif [ -x "${SCRIPT_DIR}/${APP_NAME}" ]; then
    BINARY_PATH="${SCRIPT_DIR}/${APP_NAME}"
elif [ -x "${SCRIPT_DIR}/${BINARY_NAME}" ]; then
    BINARY_PATH="${SCRIPT_DIR}/${BINARY_NAME}"
else
    echo "Error: binary not found."
    echo "Run 'cargo build --release -p ${BINARY_NAME} --features gui_egui' from the project root first."
    exit 1
fi

# ── Privilege escalation ────────────────────────────────────────────────────
# If the target bin dir is not user-writable and we're not root,
# re-exec with sudo or pkexec so the user is prompted for their password.
NEED_ELEVATION=false
check_dir_writeable() {
    local d="$1"
    # If DESTDIR is set, the prefix may be anywhere; skip elevation check.
    [ -n "${DESTDIR:-}" ] && return 1
    # If the directory exists, test it; otherwise test the first existing parent.
    local testdir="$d"
    while [ ! -d "$testdir" ] && [ "$testdir" != "/" ]; do
        testdir="$(dirname "$testdir")"
    done
    [ -w "$testdir" ] && return 1 || return 0
}

if [ "$(id -u)" -ne 0 ]; then
    check_dir_writeable "$BIN_DIR" && NEED_ELEVATION=true
fi

if [ "$NEED_ELEVATION" = true ]; then
    SELF="$(realpath "$0" 2>/dev/null || readlink -f "$0" 2>/dev/null || echo "$0")"
    # pkexec/sudo may change the working directory, so cd back to project root
    PROJECT_DIR="$(dirname "$SELF")"
    # Build safe argument quoting for re-exec
    ARGS=""
    for arg in "$@"; do
        printf -v qarg '%q' "$arg"
        ARGS="$ARGS $qarg"
    done
    echo "This installer needs elevated privileges to write to ${PREFIX}."
    echo ""
    # Try pkexec first (polkit GUI), then sudo.
    if command -v pkexec &>/dev/null; then
        echo "Launching pkexec…"
        exec pkexec bash -c "cd '$PROJECT_DIR' && exec bash '$SELF'$ARGS"
    elif command -v sudo &>/dev/null; then
        echo "Launching sudo (you may be prompted for your password)…"
        exec sudo bash -c "cd '$PROJECT_DIR' && exec bash '$SELF'$ARGS"
    else
        echo "Error: cannot escalate privileges (neither sudo nor pkexec found)."
        echo "Please run this installer with: sudo bash $SELF"
        exit 1
    fi
    # Note: exec replaces the current process; we never reach here.
fi

echo ""
echo "╔══════════════════════════════════════════════════╗"
echo "║     Skythe v${APP_VERSION} Installer                 ║"
echo "╚══════════════════════════════════════════════════╝"
echo ""

# ============================================================================
# 1. DEEP CLEAN — remove ALL previous traces from EVERY known location
# ============================================================================
echo "── Cleaning up previous Skythe installations ──"

# Helper: try to remove a file, ignore if gone
clean_file() {
    local f="$1"
    if [ -f "$f" ] || [ -L "$f" ]; then
        rm -f "$f" 2>/dev/null && echo "  Removed: $f" || true
    fi
}

# Known binary locations
for dir in \
    "${BIN_DIR}" \
    "/usr/local/bin" \
    "/usr/bin" \
    "${USER_LOCAL_BIN}" \
    "${ORIGINAL_USER_HOME}/bin"; \
do
    clean_file "${dir}/${APP_NAME}"
done

# Known desktop-file locations
for dir in \
    "${APP_DIR}" \
    "/usr/local/share/applications" \
    "/usr/share/applications" \
    "${USER_LOCAL_APP_DIR}"; \
do
    clean_file "${dir}/${APP_NAME}.desktop"
done

# Known icon locations
for dir in \
    "${ICON_DIR}" \
    "/usr/local/share/icons/hicolor/scalable/apps" \
    "/usr/share/icons/hicolor/scalable/apps" \
    "${USER_LOCAL_ICON_DIR}"; \
do
    clean_file "${dir}/${APP_NAME}.svg"
done

# Data directories (may hold config/cache from previous runs)
for dir in \
    "${DATA_DIR}" \
    "/usr/local/share/${APP_NAME}" \
    "/usr/share/${APP_NAME}" \
    "${USER_LOCAL_DATA_DIR}" \
    "${ORIGINAL_USER_HOME}/.config/${APP_NAME}" \
    "${ORIGINAL_USER_HOME}/.cache/${APP_NAME}"; \
do
    if [ -d "$dir" ]; then
        rm -rf "$dir" 2>/dev/null && echo "  Removed dir: $dir" || true
    fi
done

echo ""

# ============================================================================
# 2. INSTALL binary
# ============================================================================
echo "── Installing files ──"

install -Dm755 "$BINARY_PATH" "${BIN_DIR}/${APP_NAME}"
echo "  Binary: ${BIN_DIR}/${APP_NAME}"

# ============================================================================
# 3. SVG icon
# ============================================================================
ICON_TMP=$(mktemp /tmp/${APP_NAME}-icon-XXXXXX.svg)
trap 'rm -f "$ICON_TMP"' EXIT

cat > "$ICON_TMP" << 'SVGEOF'
<?xml version="1.0" encoding="UTF-8"?>
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128" width="128" height="128">
  <rect width="128" height="128" rx="24" fill="#1f1f2b"/>
  <circle cx="64" cy="64" r="32" fill="none" stroke="#4fede1" stroke-width="5"/>
  <circle cx="64" cy="64" r="19" fill="none" stroke="rgba(255,255,255,0.25)" stroke-width="3"/>
  <polygon points="54,50 54,78 78,64" fill="#4fede1"/>
</svg>
SVGEOF

install -Dm644 "$ICON_TMP" "${ICON_DIR}/${APP_NAME}.svg"
echo "  Icon:   ${ICON_DIR}/${APP_NAME}.svg"

# ============================================================================
# 4. Desktop file
# ============================================================================
DESKTOP_FILE="${APP_DIR}/${APP_NAME}.desktop"
mkdir -p "$APP_DIR"

cat > "$DESKTOP_FILE" << DESKTOPEOF
[Desktop Entry]
Version=1.0
Type=Application
Name=${APP_DISPLAY}
GenericName=Music Player
Comment=Listen to your local music collection
Exec=${BIN_DIR}/${APP_NAME} %F
Icon=${APP_NAME}
Terminal=false
Categories=Audio;AudioVideo;Player;Music;
Keywords=music;audio;player;flac;mp3;
MimeType=audio/flac;audio/mpeg;audio/ogg;audio/wav;audio/x-wav;audio/aac;audio/mp4;audio/x-m4a;audio/x-flac;audio/x-ape;audio/x-wavpack;
StartupNotify=true
StartupWMClass=Skythe
Actions=PlayPause;Next;Previous;

[Desktop Action PlayPause]
Name=Play / Pause
Exec=${BIN_DIR}/${APP_NAME} --play-pause

[Desktop Action Next]
Name=Next Track
Exec=${BIN_DIR}/${APP_NAME} --next

[Desktop Action Previous]
Name=Previous Track
Exec=${BIN_DIR}/${APP_NAME} --previous
DESKTOPEOF

chmod 644 "$DESKTOP_FILE"
echo "  Desktop: ${DESKTOP_FILE}"

# ============================================================================
# 5. Update databases
# ============================================================================
echo ""
echo "── Updating desktop & icon caches ──"

update-desktop-database "${APP_DIR}" 2>/dev/null || true
gtk-update-icon-cache -f -t "${ICON_DIR%/*/*}" 2>/dev/null || true  # parent of "apps"

# Also update user-local caches if they exist
if [ -d "${USER_LOCAL_APP_DIR}" ]; then
    update-desktop-database "${USER_LOCAL_APP_DIR}" 2>/dev/null || true
fi
if [ -d "${USER_LOCAL_ICON_DIR%/*/*}" ]; then
    gtk-update-icon-cache -f -t "${USER_LOCAL_ICON_DIR%/*/*}" 2>/dev/null || true
fi

# ============================================================================
# 6. Set Skythe as the default audio player
# ============================================================================
echo ""
echo "── Setting default audio MIME associations ──"

AUDIO_MIME_TYPES=(
    audio/flac
    audio/mpeg
    audio/ogg
    audio/wav
    audio/x-wav
    audio/aac
    audio/mp4
    audio/x-m4a
    audio/x-flac
    audio/x-ape
    audio/x-wavpack
)

for mime in "${AUDIO_MIME_TYPES[@]}"; do
    xdg-mime default "${APP_NAME}.desktop" "$mime" 2>/dev/null || true
    echo "  Default for ${mime} → ${APP_NAME}.desktop"
done

# ============================================================================
# 7. Summary
# ============================================================================
echo ""
echo "✔ Skythe v${APP_VERSION} installed successfully!"
echo ""
echo "   Binary:  ${BIN_DIR}/${APP_NAME}"
echo "   Icon:    ${ICON_DIR}/${APP_NAME}.svg"
echo "   Desktop: ${DESKTOP_FILE}"
echo ""
echo "   Skythe has been set as the default audio player."
echo "   It should appear in your start menu under Multimedia."
echo ""
echo "To uninstall: ./uninstall.sh"
echo ""
