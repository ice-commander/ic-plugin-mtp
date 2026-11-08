#!/bin/sh
set -e
cd "$(dirname "$0")"

case "$(uname -s)" in
    Darwin) EXT=dylib; TARGET="$HOME/Library/Application Support/ice-commander/plugins" ;;
    MINGW*|MSYS*|CYGWIN*) EXT=dll; TARGET="$APPDATA/ice-commander/plugins" ;;
    *) EXT=so; TARGET="${XDG_DATA_HOME:-$HOME/.local/share}/ice-commander/plugins" ;;
esac

if [ -n "$IC_PLUGIN_DIR" ]; then
    TARGET="$IC_PLUGIN_DIR"
fi

if ! ls "bin/"*."$EXT" >/dev/null 2>&1; then
    echo "nothing in bin/ — run ./build.sh first" >&2
    exit 1
fi

mkdir -p "$TARGET"

# Sweep only what bin/.deployed lists: several plugin repositories share the target folder.
if [ -f bin/.deployed ]; then
    while IFS= read -r stale; do
        [ -n "$stale" ] || continue
        [ -f "bin/$stale" ] && continue
        if [ -f "$TARGET/$stale" ]; then
            rm -f "$TARGET/$stale"
            printf '%s\n' "  removed $stale, no longer built here"
        fi
    done < bin/.deployed
fi

: > bin/.deployed
for library in bin/*."$EXT"; do
    name="$(basename "$library")"
    cp "$library" "$TARGET/"
    printf '%s\n' "$name" >> bin/.deployed
    printf '%s\n' "  $name -> $TARGET"
done
echo "done; switch them on in Settings -> Plugins, then restart"
