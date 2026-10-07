#!/bin/sh
set -eu

matrix_root="$HOME/.codex/packages/matrix"
matrix_binary="$matrix_root/current/bin/codex"
if [ ! -x "$matrix_binary" ]; then
    matrix_binary="$matrix_root/fallback/bin/codex"
fi

matrix_updater="$HOME/.codex/bin/update-matrix.py"
if [ -r "$matrix_updater" ] && command -v python3 >/dev/null 2>&1; then
    python3 "$matrix_updater" --fallback "$matrix_binary" || :
fi
if [ -x "$matrix_root/current/bin/codex" ]; then
    matrix_binary="$matrix_root/current/bin/codex"
fi
if [ ! -x "$matrix_binary" ]; then
    printf '%s\n' 'Matrix Codex 실행 파일을 찾을 수 없어.' >&2
    exit 1
fi

# Keep this process on one package even if another launch updates current.
matrix_binary_dir="$(CDPATH= cd -P -- "$(dirname -- "$matrix_binary")" && pwd)"
export CODEX_MATRIX_RAIN=1
exec "$matrix_binary_dir/codex" "$@"
