#!/usr/bin/env bash
# Builds a .deb, and an AppImage when appimagetool is reachable. Linux half of M8.
#
# The Depends line is DERIVED from the binary with dpkg-shlibdeps, not hand-written. A
# hand-written list is a guess that rots silently: it stays plausible while the real
# dependency set moves under it, and the failure lands on a user's machine as a missing
# .so at startup rather than here. If dpkg-shlibdeps is unavailable the script refuses
# rather than shipping a package whose Depends line is fiction.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/release/medatat-ui"
VERSION="$(grep -m1 '^version' "$ROOT/crates/medatat-ui/Cargo.toml" | cut -d'"' -f2)"
DEB_ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
STAGE="$ROOT/dist/deb"
DEB="$ROOT/dist/medatat_${VERSION}_${DEB_ARCH}.deb"

[ -x "$BIN" ] || { echo "error: $BIN not built. Run: cargo build --release -p medatat-ui" >&2; exit 1; }
command -v dpkg-deb >/dev/null || { echo "error: dpkg-deb not found (apt-get install dpkg-dev)" >&2; exit 1; }
command -v dpkg-shlibdeps >/dev/null || {
  echo "error: dpkg-shlibdeps not found (apt-get install dpkg-dev)." >&2
  echo "  Refusing to emit a hand-written Depends line -- see the note at the top." >&2
  exit 1; }

rm -rf "$STAGE"
mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/share/applications" "$STAGE/usr/share/doc/medatat"
install -m 0755 "$BIN" "$STAGE/usr/bin/medatat"

# No Icon= key: this project ships no icon asset, and naming one that does not exist
# leaves a broken image in every menu. Add the key together with the file, not before.
cat > "$STAGE/usr/share/applications/medatat.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=medatat
Comment=Medical data abstraction
Exec=/usr/bin/medatat
Terminal=false
Categories=Office;
DESKTOP

cat > "$STAGE/usr/share/doc/medatat/copyright" <<'COPYRIGHT'
Upstream-Name: medatat
Files: *
License: proprietary
COPYRIGHT

# dpkg-shlibdeps reads the ELF and resolves each SONAME to the package providing it. It
# insists on running from a tree containing debian/control, so give it a throwaway one.
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
mkdir -p "$TMP/debian"
printf 'Source: medatat\n\nPackage: medatat\nArchitecture: any\n' > "$TMP/debian/control"
( cd "$TMP" && dpkg-shlibdeps -O --ignore-missing-info "$STAGE/usr/bin/medatat" ) \
  > "$TMP/deps" 2>"$TMP/warn" || {
    echo "error: dpkg-shlibdeps failed; refusing to ship a guessed Depends line" >&2
    sed 's/^/  /' "$TMP/warn" >&2
    exit 1; }
DEPENDS="$(sed -n 's/^shlibs:Depends=//p' "$TMP/deps")"
[ -s "$TMP/warn" ] && { echo "dpkg-shlibdeps warnings:"; sed 's/^/  /' "$TMP/warn"; }

{
  echo "Package: medatat"
  echo "Version: ${VERSION}"
  echo "Section: science"
  echo "Priority: optional"
  echo "Architecture: ${DEB_ARCH}"
  [ -n "$DEPENDS" ] && echo "Depends: ${DEPENDS}"
  echo "Maintainer: medatat <doug.lance@gmail.com>"
  echo "Description: Medical data abstraction tool"
  echo " Data-driven forms with configurable fields, backed by a local encrypted"
  echo " store that syncs in the background. The UI never awaits the network."
} > "$STAGE/DEBIAN/control"

dpkg-deb --build --root-owner-group "$STAGE" "$DEB" >/dev/null
echo "built $DEB ($(du -h "$DEB" | cut -f1))"
echo "depends: ${DEPENDS:-<none resolved>}"

# --root-owner-group is load-bearing: without it every file is owned by the building
# user's uid, and dpkg installs them that way.
dpkg-deb --info "$DEB" >/dev/null || { echo "error: $DEB is not a readable package" >&2; exit 1; }

if command -v lintian >/dev/null; then
  lintian --fail-on error "$DEB" || { echo "error: lintian found errors in $DEB" >&2; exit 1; }
  echo "lintian: no errors"
else
  echo "lintian: not installed, package not linted"
fi

# ---------------------------------------------------------------------- AppImage
# Optional: appimagetool is a download rather than a package. The .deb is the supported
# artifact; the AppImage is a convenience for non-Debian distributions.
APPIMAGETOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
if [ -z "$APPIMAGETOOL" ]; then
  echo "appimage: skipped (appimagetool not found; set APPIMAGETOOL=/path to build one)"
  exit 0
fi

APPDIR="$ROOT/dist/medatat.AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin"
install -m 0755 "$BIN" "$APPDIR/usr/bin/medatat"
cp "$STAGE/usr/share/applications/medatat.desktop" "$APPDIR/medatat.desktop"
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/medatat" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

OUT="$ROOT/dist/medatat-${VERSION}-$(uname -m).AppImage"
# appimagetool wants a FUSE mount unless told otherwise; CI runners rarely have one.
if APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" "$APPDIR" "$OUT" >/dev/null 2>&1; then
  echo "built $OUT ($(du -h "$OUT" | cut -f1))"
else
  echo "appimage: appimagetool failed (the .deb is unaffected)" >&2
fi
