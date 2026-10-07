#!/bin/sh
set -eu

if [ "$#" -gt 1 ]; then
    printf '%s\n' '사용법: sh install.command [설치 경로]' >&2
    exit 1
fi
if [ "$(uname -s):$(uname -m)" != 'Darwin:arm64' ]; then
    printf '%s\n' 'macOS Apple Silicon 맥에서 실행해.' >&2
    exit 1
fi

package_dir="$(CDPATH= cd -P -- "$(dirname -- "$0")" && pwd)"
version="$("$package_dir/bin/codex" --version)"
case "$version" in
    'codex-cli '*) version="${version#codex-cli }" ;;
    *) printf '%s\n' 'Codex 버전 확인 실패.' >&2; exit 1 ;;
esac
case "$version" in
    ''|*[!0-9.]*) printf '%s\n' '올바른 버전이 아니야.' >&2; exit 1 ;;
esac
package_name="codex-matrix-$version-aarch64-apple-darwin"
if [ "$(basename -- "$package_dir")" != "$package_name" ]; then
    printf '%s\n' '원래 패키지 폴더에서 실행해.' >&2
    exit 1
fi
codesign --verify --strict "$package_dir/bin/codex"

install_prefix="${1:-$HOME/.codex}"
mkdir -p "$install_prefix/bin" "$install_prefix/packages/portable"
install_prefix="$(CDPATH= cd -P -- "$install_prefix" && pwd)"
command_path="$install_prefix/bin/codex-matrix-$version"
if [ -e "$command_path" ] || [ -L "$command_path" ]; then
    printf '이미 설치돼 있어: %s\n' "$command_path" >&2
    exit 1
fi

# Publish the command only after the complete package has been copied and checked.
install_dir="$(mktemp -d "$install_prefix/packages/portable/$package_name.XXXXXXXX")"
trap 'rm -rf -- "$install_dir"' EXIT
trap 'exit 1' HUP INT TERM
cp -R "$package_dir/." "$install_dir/"
codesign --verify --strict "$install_dir/bin/codex"
if [ "$("$install_dir/bin/codex-matrix" --version)" != "codex-cli $version" ]; then
    printf '%s\n' '설치한 Codex 버전 확인 실패.' >&2
    exit 1
fi
ln -s "$install_dir/bin/codex-matrix" "$command_path"
trap - EXIT HUP INT TERM
printf '설치 완료. 실행: "%s"\n' "$command_path"
