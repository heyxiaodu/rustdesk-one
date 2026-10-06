#!/usr/bin/env python3
"""Generate res/scalable.svg from res/128x128@2x.png.

    python3 scripts/make-scalable-svg.py           # write res/scalable.svg
    python3 scripts/make-scalable-svg.py --check   # exit 1 if it is out of date

The icon is one self-contained SVG wrapping the brand PNG as base64, so a single
file serves every size referenced by the Linux packaging.
"""

from __future__ import annotations

import argparse
import base64
import ctypes
import hashlib
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
SOURCE = REPO_ROOT / "res" / "128x128@2x.png"
OUTPUT = REPO_ROOT / "res" / "scalable.svg"

# Pins the brand pixel data: swapping the source PNG must be a deliberate change.
SOURCE_SHA256 = "e2dfa11f3ac66e47ab1e51400fd37c9ba4c7d057fa37185164e792e57d620f9b"

# flatpak's icon validator starts with gdk_pixbuf_get_file_info(), which sniffs
# only the first SNIFF_BYTES bytes of the file to choose a loader. librsvg reports
# the "svg" format only once it has parsed the <svg> root element, so a root
# element starting past that window reads as "Format not recognized" and
# `flatpak build-export` fails with
#     ... is not a valid icon: Format not recognized
# Everything before <svg> (the XML declaration and the comment below) must
# therefore stay inside the window; check_sniff_window() enforces it on every
# generated file. This is the regression guard for that CI failure.
SNIFF_BYTES = 256

XML_DECL = b'<?xml version="1.0" encoding="UTF-8"?>\n'

# Byte for byte the comment this icon always carried; only its position moved
# (it used to sit above <svg>, which pushed the root element to byte 575). Its
# "Regenerate with" line still names the historical, untracked generator
# analysis/oem/assets/make-scalable-svg.py and is left untouched so this change
# stays a pure move.
COMMENT = (
    b'<!-- NERV Desk Linux menu icon.\n'
    b'     Source: repos/rustdesk/res/128x128@2x.png (byte-identical to flutter/assets/icon.png).\n'
    b'     sha256(source) = e2dfa11f3ac66e47ab1e51400fd37c9ba4c7d057fa37185164e792e57d620f9b\n'
    b'     size(source)   = 43823 bytes, 256x256, sRGBA (transparent background)\n'
    b'     Embedded so a single self-contained file serves every icon size referenced by the\n'
    b'     Linux packaging (res/PKGBUILD, res/rpm*.spec, res/nervdesk.desktop, appimage yml).\n'
    b'     Regenerate with: python3 analysis/oem/assets/make-scalable-svg.py\n'
    b'-->\n'
)

SVG_OPEN = (
    b'<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"\n'
    b'     width="32" height="32" viewBox="0 0 256 256" role="img" aria-label="NERV Desk">'
)

IMAGE_PREFIX = (
    b'<image x="0" y="0" width="256" height="256" preserveAspectRatio="xMidYMid meet"\n'
    b'         xlink:href="data:image/png;base64,'
)

IMAGE_SUFFIX = b'"/>\n</svg>\n'


class SourceError(Exception):
    pass


def read_source() -> bytes:
    try:
        png = SOURCE.read_bytes()
    except FileNotFoundError:
        raise SourceError(f"source not found: {SOURCE}")
    digest = hashlib.sha256(png).hexdigest()
    if digest != SOURCE_SHA256:
        raise SourceError(f"{SOURCE} sha256 is {digest}, expected {SOURCE_SHA256}")
    return png


def check_sniff_window(data: bytes) -> int:
    offset = data.find(b"<svg")
    if offset < 0:
        raise SourceError("generated icon has no <svg> root element")
    if offset > SNIFF_BYTES:
        raise SourceError(
            f"<svg> starts at byte {offset}, past the {SNIFF_BYTES}-byte window "
            "gdk_pixbuf_get_file_info() sniffs; flatpak's icon validator would "
            "reject the file as 'Format not recognized'"
        )
    return offset


def render(png: bytes) -> bytes:
    data = (
        XML_DECL
        + SVG_OPEN
        + b"\n"
        + COMMENT
        + b"  "
        + IMAGE_PREFIX
        + base64.b64encode(png)
        + IMAGE_SUFFIX
    )
    check_sniff_window(data)
    return data


def gdk_pixbuf_format(path: Path) -> str:
    """What gdk-pixbuf sniffs for `path`, or why the probe could not run."""
    try:
        lib = ctypes.CDLL("libgdk_pixbuf-2.0.so.0")
    except OSError:
        return "unavailable (libgdk_pixbuf-2.0.so.0 not found)"
    lib.gdk_pixbuf_get_file_info.restype = ctypes.c_void_p
    lib.gdk_pixbuf_get_file_info.argtypes = [
        ctypes.c_char_p,
        ctypes.POINTER(ctypes.c_int),
        ctypes.POINTER(ctypes.c_int),
    ]
    lib.gdk_pixbuf_format_get_name.restype = ctypes.c_char_p
    lib.gdk_pixbuf_format_get_name.argtypes = [ctypes.c_void_p]
    width, height = ctypes.c_int(0), ctypes.c_int(0)
    fmt = lib.gdk_pixbuf_get_file_info(
        str(path).encode(), ctypes.byref(width), ctypes.byref(height)
    )
    if not fmt:
        return "not recognized"
    name = lib.gdk_pixbuf_format_get_name(fmt).decode("ascii")
    return f"{name} {width.value}x{height.value}"


def report_sniff() -> None:
    if OUTPUT.is_file():
        print(f"make-scalable-svg: gdk_pixbuf_get_file_info -> {gdk_pixbuf_format(OUTPUT)}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if res/scalable.svg is not exactly what this script generates",
    )
    args = parser.parse_args()

    try:
        generated = render(read_source())
    except SourceError as err:
        print(f"make-scalable-svg: error: {err}", file=sys.stderr)
        return 1

    offset = check_sniff_window(generated)
    existing = OUTPUT.read_bytes() if OUTPUT.is_file() else None

    if args.check:
        if existing == generated:
            print(f"make-scalable-svg: {OUTPUT} is up to date (<svg> at byte {offset})")
            report_sniff()
            return 0
        print(
            f"make-scalable-svg: error: {OUTPUT} is out of date; "
            "run scripts/make-scalable-svg.py",
            file=sys.stderr,
        )
        for label, blob in (("on disk", existing), ("generated", generated)):
            if blob is None:
                print(f"  {label}: missing", file=sys.stderr)
            else:
                print(
                    f"  {label}: {len(blob)} bytes "
                    f"sha256 {hashlib.sha256(blob).hexdigest()}",
                    file=sys.stderr,
                )
        return 1

    if existing == generated:
        print(f"make-scalable-svg: {OUTPUT} already up to date (<svg> at byte {offset})")
    else:
        OUTPUT.write_bytes(generated)
        print(
            f"make-scalable-svg: wrote {OUTPUT} "
            f"({len(generated)} bytes, <svg> at byte {offset})"
        )
    report_sniff()
    return 0


if __name__ == "__main__":
    sys.exit(main())
