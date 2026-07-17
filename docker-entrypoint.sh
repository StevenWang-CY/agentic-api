#!/bin/sh
set -eu

# Keep runtime-created SQLite files writable when OpenShift rotates the
# arbitrary UID while retaining the image's root-group permission model.
umask 0002

database_url=${DATABASE_URL:-sqlite://./agentic_api.db}
case "$database_url" in
    sqlite://\?mode=memory* | sqlite::memory:* | sqlite://*mode=ro*)
        ;;
    sqlite://*)
        # `?` and `#` are URI query/fragment delimiters, so they are not part
        # of the filesystem path extracted here. Keep the parent directory
        # group-writable as a fallback for percent-encoded path characters.
        database_path=${database_url#sqlite://}
        database_path=${database_path%%\?*}
        database_path=${database_path%%\#*}
        if [ -n "$database_path" ] && [ ! -e "$database_path" ]; then
            : >"$database_path"
            chmod g+rw "$database_path"
        fi
        database_directory=$(dirname -- "$database_path")
        chmod g+rwx "$database_directory" 2>/dev/null || true
        ;;
esac

exec agentic-server "$@"
