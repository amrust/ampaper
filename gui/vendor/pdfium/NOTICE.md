# PDFium binary distribution

This directory ships pre-built PDFium binaries so ampaper-gui works
out of the box on a fresh checkout — no internet round-trip on first
build, which matters for an archival tool that should still build
in 10 years.

## What's PDFium?

PDFium is the PDF rendering engine maintained by Google. It's the
same engine Chromium / Chrome / Edge use to display PDFs. Source
and project home: https://pdfium.googlesource.com/pdfium/.

ampaper-gui loads it dynamically (via `pdfium-render`) on the
Decode tab whenever the user drops a PDF input — for arbitrary
scanner-produced PDFs (JPEG2000, CCITT-fax, color profiles), no
pure-Rust alternative is robust enough.

## Licenses

PDFium itself is dual-licensed under **Apache License 2.0** OR
**BSD-3-Clause** at your option. Both permit redistribution
including in derivative works as long as the license texts are
preserved with the binary.

The pre-built binaries in `win64/` (and any future per-target
subdirectories) come from the **bblanchon/pdfium-binaries** project
(<https://github.com/bblanchon/pdfium-binaries>), which packages
upstream PDFium builds. The `LICENSE` file alongside this NOTICE is
the bblanchon repackaging license (**MIT**). The PDFium source and
license texts are reachable from the upstream project linked above.

ampaper itself is **GPL-3.0-or-later**. PDFium's Apache-2.0 / BSD-3
terms are GPL-compatible, so this bundling is clean.

## Versions

See `VERSION` for the upstream PDFium version. Update the binary by
fetching a new release from <https://github.com/bblanchon/pdfium-binaries/releases>
and dropping the platform's `bin/pdfium.dll` (or `lib/libpdfium.so`,
or `lib/libpdfium.dylib`) into the matching subdirectory here, then
copying the new `LICENSE` and `VERSION` files alongside.

## Why dynamic, not static?

We dynamic-load via `libloading`. Static linking is supported by
`pdfium-render`'s `static` feature but adds ~15 MB to the executable
and the cross-platform story is finicky. Dynamic + bundled DLL keeps
the executable lean and the ship-it footprint a tidy two-file pair
(`ampaper-gui.exe` + `pdfium.dll`).
