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

# Icon= names a file that is installed below. The two must land together: an Icon key
# pointing at nothing shows a broken image in every menu, and no Icon key at all makes
# appimagetool refuse outright ("Icon entry not found in desktop file").
ICON="$ROOT/packaging/medatat.png"
[ -f "$ICON" ] || { echo "error: $ICON missing. Run: python3 packaging/make-icon.py" >&2; exit 1; }
mkdir -p "$STAGE/usr/share/icons/hicolor/256x256/apps"
install -m 0644 "$ICON" "$STAGE/usr/share/icons/hicolor/256x256/apps/medatat.png"

cat > "$STAGE/usr/share/applications/medatat.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=medatat
Comment=Medical data abstraction
Exec=/usr/bin/medatat
Icon=medatat
Terminal=false
Categories=Office;
DESKTOP

# Machine-readable DEP-5. lintian rejects a copyright file with no actual copyright
# notice in it, which the first version of this was.
cat > "$STAGE/usr/share/doc/medatat/copyright" <<'COPYRIGHT'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: medatat

Files: *
Copyright: 2026 Doug Lance <doug.lance@gmail.com>
License: proprietary
 All rights reserved. This software is not distributed under a free-software
 licence; no permission to copy, modify, or redistribute is granted here.
COPYRIGHT

# A native package must ship a changelog, and lintian treats its absence as an error
# rather than a nicety: it is how anyone installing the package finds out what changed.
# The date comes from the last commit so a rebuild of the same tree is reproducible,
# rather than from `date` which would make every build differ.
CHANGELOG_DATE="$(git -C "$ROOT" log -1 --format=%aD 2>/dev/null || echo 'Mon, 18 Aug 2026 00:00:00 +0000')"
cat > "$STAGE/usr/share/doc/medatat/changelog" <<CHANGELOG
medatat (${VERSION}) unstable; urgency=medium

  * Initial release.

 -- medatat <doug.lance@gmail.com>  ${CHANGELOG_DATE}
CHANGELOG
# -n omits the timestamp from the gzip header, for the same reproducibility reason.
gzip -9n "$STAGE/usr/share/doc/medatat/changelog"

# A binary in /usr/bin with no man page is a lintian warning and, more to the point, a
# real gap: `man medatat` is where someone looks first.
mkdir -p "$STAGE/usr/share/man/man1"
cat > "$STAGE/usr/share/man/man1/medatat.1" <<MAN
.TH MEDATAT 1 "2026-08-18" "medatat ${VERSION}" "User Commands"
.SH NAME
medatat \- medical data abstraction tool
.SH SYNOPSIS
.B medatat
.SH DESCRIPTION
.B medatat
is a data-entry application for abstracting medical records into
data-driven forms whose fields and layout are configured through the
interface rather than in code.
.PP
Edits are written to an encrypted local store first and synchronised in
the background, so the interface never waits for the network and never
shows a loading spinner. Work continues unchanged while offline; queued
edits are sent when connectivity returns.
.SH FILES
.TP
.I \$XDG_DATA_HOME/medatat/
Local store and its key file, falling back to
.I ~/.local/share/medatat/
when
.B XDG_DATA_HOME
is unset. Contains unsynced edits: do not delete it to troubleshoot, as
queued work has not reached the server and is not recoverable from it.
.SH EXIT STATUS
.TP
.B 0
The application exited normally.
.SH SEE ALSO
Project documentation in
.IR docs/ .
MAN
gzip -9n "$STAGE/usr/share/man/man1/medatat.1"

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
# appimagetool looks for the icon beside the desktop file, and uses .DirIcon as the
# thumbnail the file manager shows. Both are the same image.
cp "$ICON" "$APPDIR/medatat.png"
cp "$ICON" "$APPDIR/.DirIcon"
mkdir -p "$APPDIR/usr/share/icons/hicolor/256x256/apps"
install -m 0644 "$ICON" "$APPDIR/usr/share/icons/hicolor/256x256/apps/medatat.png"
cat > "$APPDIR/AppRun" <<'APPRUN'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/medatat" "$@"
APPRUN
chmod +x "$APPDIR/AppRun"

OUT="$ROOT/dist/medatat-${VERSION}-$(uname -m).AppImage"
# appimagetool wants a FUSE mount unless told otherwise; CI runners rarely have one.
# Keep appimagetool's output. Discarding it reports "failed" and nothing else, which is
# the same mistake that made the smoke job unreadable for two days.
if APPIMAGE_EXTRACT_AND_RUN=1 "$APPIMAGETOOL" "$APPDIR" "$OUT" > "$TMP/appimage.log" 2>&1; then
  echo "built $OUT ($(du -h "$OUT" | cut -f1))"
else
  echo "appimage: appimagetool failed (the .deb is unaffected). Its output:" >&2
  sed 's/^/  /' "$TMP/appimage.log" >&2
fi
