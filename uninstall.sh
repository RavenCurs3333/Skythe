#!/usr/bin/env bash
# ============================================================================
# Skythe Uninstaller — thorough and robust
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

# ── Derive version for display ─────────────────────────────────────────────
APP_VERSION=""
if [ -f player-ui/Cargo.toml ]; then
    APP_VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' player-ui/Cargo.toml | head -1)"
elif [ -f VERSION ]; then
    APP_VERSION="$(cat VERSION | tr -d '[:space:]')"
elif [ -f "$(dirname "$0")/VERSION" ]; then
    APP_VERSION="$(cat "$(dirname "$0")/VERSION" | tr -d '[:space:]')"
fi
APP_VERSION="${APP_VERSION:-unknown}"

# ── Target directory prefixes ──────────────────────────────────────────────
PREFIX="${PREFIX:-/usr/local}"
DESTDIR="${DESTDIR:-}"
BIN_DIR="${DESTDIR}${PREFIX}/bin"
ICON_DIR="${DESTDIR}${PREFIX}/share/icons/hicolor/scalable/apps"
APP_DIR="${DESTDIR}${PREFIX}/share/applications"
DATA_DIR="${DESTDIR}${PREFIX}/share/${APP_NAME}"

# ── All known installation locations (system-wide + user-local) ────────────
ALL_BIN_DIRS=(
    "${BIN_DIR}"
    "/usr/local/bin"
    "/usr/bin"
    "${HOME}/.local/bin"
    "${HOME}/bin"
)

ALL_APP_DIRS=(
    "${APP_DIR}"
    "/usr/local/share/applications"
    "/usr/share/applications"
    "${HOME}/.local/share/applications"
)

ALL_ICON_DIRS=(
    "${ICON_DIR}"
    "/usr/local/share/icons/hicolor/scalable/apps"
    "/usr/share/icons/hicolor/scalable/apps"
    "${HOME}/.local/share/icons/hicolor/scalable/apps"
)

ALL_DATA_DIRS=(
    "${DATA_DIR}"
    "/usr/local/share/${APP_NAME}"
    "/usr/share/${APP_NAME}"
    "${HOME}/.local/share/${APP_NAME}"
    "${HOME}/.config/${APP_NAME}"
    "${HOME}/.cache/${APP_NAME}"
)

# ── Auto-elevate if system files need removing ─────────────────────────────
NEED_SUDO=false
if [ "$(id -u)" -ne 0 ]; then
    # Check if any system-level binary exists owned by root
    for dir in /usr/local/bin /usr/bin; do
        [ -f "${dir}/${APP_NAME}" ] && NEED_SUDO=true
    done
    [ -d "/usr/local/share/applications" ] && NEED_SUDO=true
    [ -d "/usr/local/share/icons/hicolor" ] && NEED_SUDO=true
fi

if [ "$NEED_SUDO" = true ]; then
    SELF="$(realpath "$0" 2>/dev/null || readlink -f "$0" 2>/dev/null || echo "$0")"
    PROJECT_DIR="$(dirname "$SELF")"
    echo "System-level files detected. Launching with elevated privileges..."
    echo ""
    if command -v pkexec &>/dev/null; then
        exec pkexec bash -c "cd '$PROJECT_DIR' && exec bash '$SELF' $*"
    elif command -v sudo &>/dev/null; then
        exec sudo bash -c "cd '$PROJECT_DIR' && exec bash '$SELF' $*"
    else
        echo "Error: Cannot elevate. Please run: sudo bash $SELF"
        exit 1
    fi
fi

echo ""
echo "╔══════════════════════════════════════════════════╗"
echo "║   Uninstalling ${APP_DISPLAY} v${APP_VERSION}           ║"
echo "╚══════════════════════════════════════════════════╝"
echo ""

# ============================================================================
# 1. Remove binaries
# ============================================================================
echo "── Removing binary files ──"
for dir in "${ALL_BIN_DIRS[@]}"; do
    f="${dir}/${APP_NAME}"
    if [ -f "$f" ] || [ -L "$f" ]; then
        rm -f "$f" 2>/dev/null && echo "  Removed: $f" || true
    fi
done

# ============================================================================
# 2. Remove desktop files (ONLY skythe.desktop, never remove the directory)
# ============================================================================
echo "── Removing desktop entries ──"
for dir in "${ALL_APP_DIRS[@]}"; do
    f="${dir}/${APP_NAME}.desktop"
    if [ -f "$f" ] || [ -L "$f" ]; then
        rm -f "$f" 2>/dev/null && echo "  Removed: $f" || true
    fi
done

# ============================================================================
# 3. Remove icons (ONLY skythe.svg, never remove the directory)
# ============================================================================
echo "── Removing icons ──"
for dir in "${ALL_ICON_DIRS[@]}"; do
    f="${dir}/${APP_NAME}.svg"
    if [ -f "$f" ] || [ -L "$f" ]; then
        rm -f "$f" 2>/dev/null && echo "  Removed: $f" || true
    fi
done

# ============================================================================
# 4. Remove Skythe-specific data / config / cache directories ONLY
# ============================================================================
echo "── Removing Skythe-specific data/config/cache directories ──"
# Only remove directories that are exclusively ours (prefixed with app name)
# NEVER remove ~/.local/bin, ~/.local/share/applications, or any shared dirs
for dir in \
    "${DATA_DIR}" \
    "${HOME}/.local/share/${APP_NAME}" \
    "${HOME}/.config/${APP_NAME}" \
    "${HOME}/.cache/${APP_NAME}"; \
do
    if [ -d "$dir" ]; then
        rm -rf "$dir" 2>/dev/null && echo "  Removed dir: $dir" || true
    fi
done

# Clean other users' home dirs (ONLY skythe-specific subdirectories)
for user_home in /home/*; do
    [ -d "$user_home" ] || continue
    [ "$user_home" = "$HOME" ] && continue
    for sub in .local/share/skythe .config/skythe .cache/skythe; do
        f="${user_home}/${sub}"
        [ -d "$f" ] || continue
        rm -rf "$f" 2>/dev/null && echo "  Removed: $f" || true
    done
done

# ============================================================================
# 6. Remove Skythe from MIME defaults so it's no longer the default audio player
# ============================================================================
echo "── Removing Skythe from MIME default associations ──"

# Audio MIME types that Skythe may have claimed as default
AUDIO_MIME_TYPES=(
    audio/flac audio/mpeg audio/ogg audio/wav audio/x-wav
    audio/aac audio/mp4 audio/x-m4a audio/x-flac audio/x-ape
    audio/x-wavpack
)

# All mimeapps.list locations to check (user-local first, then system-wide)
MIME_LIST_CANDIDATES=(
    "${HOME}/.config/mimeapps.list"
    "${HOME}/.local/share/applications/mimeapps.list"
    "/usr/local/share/applications/mimeapps.list"
    "/usr/share/applications/mimeapps.list"
)

removed_mime=false
for mime_file in "${MIME_LIST_CANDIDATES[@]}"; do
    if [ ! -f "$mime_file" ]; then
        continue
    fi
    # Check if skythe.desktop appears anywhere in the file
    if grep -q "${APP_NAME}.desktop" "$mime_file" 2>/dev/null; then
        echo "  Cleaning MIME entries in: $mime_file"
        # Remove all lines referencing skythe.desktop
        if [ "$(id -u)" -eq 0 ]; then
            sed -i "/${APP_NAME}.desktop/d" "$mime_file"
        else
            # When not root, use sudo for system files, or direct edit for user files
            if [[ "$mime_file" == "$HOME"* ]]; then
                sed -i "/${APP_NAME}.desktop/d" "$mime_file"
            else
                sudo sed -i "/${APP_NAME}.desktop/d" "$mime_file" 2>/dev/null || true
            fi
        fi
        removed_mime=true
    fi
done

# Also explicitly unset defaults for each audio MIME type in user-local files
for mime_file in "${MIME_LIST_CANDIDATES[@]}"; do
    if [ ! -f "$mime_file" ]; then
        continue
    fi
    for mime_type in "${AUDIO_MIME_TYPES[@]}"; do
        # Check if this MIME type has skythe.desktop as default in [Default Applications]
        if grep -q "^${mime_type}=${APP_NAME}.desktop" "$mime_file" 2>/dev/null; then
            if [ "$(id -u)" -eq 0 ]; then
                sed -i "s|^${mime_type}=${APP_NAME}.desktop||" "$mime_file"
            elif [[ "$mime_file" == "$HOME"* ]]; then
                sed -i "s|^${mime_type}=${APP_NAME}.desktop||" "$mime_file"
            else
                sudo sed -i "s|^${mime_type}=${APP_NAME}.desktop||" "$mime_file" 2>/dev/null || true
            fi
            echo "  Removed default for ${mime_type} from $mime_file"
            removed_mime=true
        fi
    done
done

if [ "$removed_mime" = true ]; then
    # Rebuild the MIME database
    update-desktop-database ~/.local/share/applications 2>/dev/null || true
    # Trigger the desktop environment to refresh file associations
    # Restart nautilus/xdg-settings to pick up the changes
    if command -v xdg-settings &>/dev/null; then
        xdg-settings set default-web-browser 2>/dev/null || true
    fi
fi

# ============================================================================
# 7. Update desktop & icon caches
# ============================================================================
echo ""
echo "── Updating desktop & icon caches ──"

# System desktop databases
for dir in "${APP_DIR}" "/usr/local/share/applications" "/usr/share/applications" "${HOME}/.local/share/applications"; do
    if [ -d "$dir" ]; then
        update-desktop-database "$dir" 2>/dev/null || true
    fi
done

# Icon caches
for icon_base in "/usr/local/share/icons/hicolor" "/usr/share/icons/hicolor" "${HOME}/.local/share/icons/hicolor"; do
    if [ -d "$icon_base" ]; then
        gtk-update-icon-cache -f -t "$icon_base" 2>/dev/null || true
    fi
done

# ============================================================================
# 8. Update font cache if fonts were installed
# ============================================================================
if command -v fc-cache &>/dev/null; then
    echo "  Updating font cache..."
    fc-cache -f 2>/dev/null || true
fi

echo ""
echo "✔ ${APP_DISPLAY} v${APP_VERSION} has been completely uninstalled."
echo ""
echo "Note: Default audio player settings may require you to open a"
echo "file manager, right-click an audio file → Properties → Open With"
echo "to select a different default player."
echo ""
