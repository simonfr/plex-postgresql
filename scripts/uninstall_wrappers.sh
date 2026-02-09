#!/bin/bash
# Uninstall Plex wrapper scripts - restore original binaries

set -e

PLEX_APP="/Applications/Plex Media Server.app/Contents/MacOS"

echo "=== Plex PostgreSQL Wrapper Uninstaller ==="
echo ""

# Check if Plex is running
if pgrep -f "Plex Media Server" >/dev/null 2>&1; then
    echo "WARNING: Plex is running. Stop it first:"
    echo "  pkill -f 'Plex Media Server'"
    exit 1
fi

# Restore Server
if [[ -f "$PLEX_APP/Plex Media Server.original" ]]; then
    echo "Restoring Plex Media Server..."
    rm -f "$PLEX_APP/Plex Media Server"
    mv "$PLEX_APP/Plex Media Server.original" "$PLEX_APP/Plex Media Server"
    echo "  Done"
else
    echo "Plex Media Server.original not found - nothing to restore"
fi

# Restore Scanner
if [[ -f "$PLEX_APP/Plex Media Scanner.original" ]]; then
    echo "Restoring Plex Media Scanner..."
    rm -f "$PLEX_APP/Plex Media Scanner"
    mv "$PLEX_APP/Plex Media Scanner.original" "$PLEX_APP/Plex Media Scanner"
    echo "  Done"
else
    if otool -L "$PLEX_APP/Plex Media Scanner" 2>/dev/null | grep -q "db_interpose_pg.dylib"; then
        echo "WARNING: Scanner is patched but Plex Media Scanner.original is missing."
        echo "         Reinstall Plex to fully restore Scanner binary."
    else
        echo "Plex Media Scanner.original not found - nothing to restore"
    fi
fi

# Remove shim dylib
if [[ -f "$PLEX_APP/db_interpose_pg.dylib" ]]; then
    echo "Removing shim dylib..."
    rm -f "$PLEX_APP/db_interpose_pg.dylib"
    echo "  Done"
fi

echo ""
echo "=== Uninstall complete ==="
echo "Plex will now use SQLite (original behavior)."
