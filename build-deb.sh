#!/usr/bin/env bash
# 把 miniam 打成 .deb（二进制 + README）。产物放仓库外，发布时上传 Release。
set -euo pipefail
APP=miniam
VERSION=${1:-0.1.0}
ARCH=$(dpkg --print-architecture 2>/dev/null || echo amd64)
HERE=$(cd "$(dirname "$0")" && pwd)
BIN=$HERE/target/release/$APP
OUT=${MINIAM_DEB_OUT:-/home/u/${APP}_${VERSION}_${ARCH}.deb}

[ -x "$BIN" ] || (cd "$HERE" && cargo build --release)

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/usr/bin" "$STAGE/usr/share/doc/$APP" "$STAGE/DEBIAN"
install -m755 "$BIN" "$STAGE/usr/bin/$APP"
install -m644 "$HERE/README.md" "$STAGE/usr/share/doc/$APP/README.md"
cat > "$STAGE/DEBIAN/control" <<CTRL
Package: $APP
Version: $VERSION
Section: sound
Priority: optional
Architecture: $ARCH
Depends: pulseaudio-utils
Maintainer: Wian1977
Description: Apple Music in the terminal
 True-color cover art, track title and a live spectrum in one terminal window.
 Plays through a hidden Firefox/Chrome with Widevine.
CTRL
dpkg-deb --build "$STAGE" "$OUT" >/dev/null
echo "$OUT"
