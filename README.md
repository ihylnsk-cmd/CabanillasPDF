# PdfCraft, modified build

This is a fork of [PdfCraft](https://github.com/storytold/pdfcraft), the open-source PDF application
written in Rust by the ArtCraft team and the PdfCraft contributors. It was forked from commit
`5d59060` (PdfCraft 0.5.0) on 2026-10-10 and merged with PdfCraft's `main` at `bbbe600`
(PdfCraft 0.6.0) on 2026-10-13. It is **not** an official PdfCraft release and is not endorsed
by the ArtCraft team.

The fork lives at https://github.com/ihylnsk-cmd/CabanillasPDF on the branch `fork`; `main` there
is PdfCraft as published. `git diff main...fork` shows what this build changes.

PdfCraft's own README, with its full feature list, is in the
[PdfCraft repository](https://github.com/storytold/pdfcraft#readme).

## What it is for

Day-to-day PDF work on Windows (it also builds for Linux and macOS):

| Need | Where it is in the app |
| --- | --- |
| Edit text that is already in a PDF | **All tools ▸ Edit a PDF ▸ Edit text & images**, then click the text and type. Click outside the box to finish. |
| Fill in forms | Click a field and type. For forms without fields: **All tools ▸ Fill & Sign** (Add text, Checkmark, Date). |
| Add a picture of a signature | **All tools ▸ Fill & Sign ▸ Add signature ▸ Image ▸ Browse…**, then **Apply** and click where it goes. The signature is remembered for next time. |
| Search | **Ctrl+F**. |
| Print | **Ctrl+P**. |
| Redact | **All tools ▸ Redact a PDF**. Mark by hand with **Redact text and images**, or use **Find text and redact** for words, phrases and patterns (phone numbers, e-mail addresses, Social Security numbers, card numbers, dates). Then **Redact all** and save. |

Applying redactions removes the text and image data under the marks from the saved file; it does
not just draw a box over them.

## Installing

Windows: unzip the folder anywhere and run `pdfcraft.exe`. There is no installer and it needs no
administrator rights. Settings and the saved signature are kept per Windows user, in
`%APPDATA%\PdfCraft`. To open PDFs with it from Explorer: right-click a PDF ▸ Open with ▸ Choose
another app ▸ `pdfcraft.exe`.

Linux: unpack the archive and run `./pdfcraft`.

The "extras" archive is optional. It holds the `models` folder that Scan & OCR needs, which
must sit beside the program (`pdfcraft.exe` and `models` in the same folder), and
`pdfcraft-cli`, the command-line program.

## What this build changes

Everything in the table exists in PdfCraft itself. This build fixes and adds the following.

1. **Editing text on lines that mix fonts.** PdfCraft treats each change of font on a line (a bold
   name, a word in italics) as a separate piece. Editing one piece left the others where they
   were, so longer text was set over what followed it, and a paragraph that opened after a bold
   name was re-wrapped with every line starting where the first one did. In this build the pieces
   after the edited one move with its end, the space between pieces is kept, and a paragraph
   whose first line starts somewhere else (after a lead-in, indented, or hanging) keeps its own
   left edge and width (`crates/edit/src/text.rs`).
2. **Edited text keeps the document's typeface.** When the text you type needs letters that the
   PDF's embedded font does not contain (most PDFs embed only the letters they use), PdfCraft fell
   back to a built-in Helvetica, Times or Courier lookalike. This build first looks for the same
   family among the fonts installed on the computer, then for a metric-compatible one (Arial and
   Liberation Sans, Calibri and Carlito, and so on), and embeds only the letters used
   (`crates/edit/src/sysfont.rs`). TrueType fonts only; otherwise the old fallback applies.
3. **Redaction patterns no longer miss numbers that sit next to other numbers.** In tables and
   lists the phone-number pattern joined a number with the digits of the next cell and skipped
   both, and the card-number pattern did the same. Fixed in `crates/redact/src/patterns.rs`.
4. **Signature pictures can be phone photos.** PdfCraft accepted pictures up to 4 megapixels and
   cleared only near-white paper, so a phone photo was refused and a scan on grey paper kept a
   grey box. This build reads pictures up to 50 megapixels, shrinks them, measures the paper
   where it is (grey paper, uneven light, the shadow of the phone), makes it transparent, drops
   what lies away from the signature (the edge of the sheet, the desk) and crops to the ink
   (`crates/engine/src/signature_paper.rs`). Pictures that already have a transparent background
   are only cropped.
5. **A short line no longer pulls in the first line of the paragraph below it** when text is
   grouped into paragraphs for editing (`crates/edit/src/text.rs`).
6. **ArtCraft branding removed**, as PdfCraft's brand terms require of modified versions: the
   ArtCraft logos, the Discord button and the community links are gone, and About says
   "Modified build". The app no longer checks PdfCraft's releases for updates (an update would
   replace this build with the official one), so it makes no network connections of its own.
   The program is still called PdfCraft; rename it before distributing it widely if it should
   not be confused with the original.

**Printing on Windows is PdfCraft's own.** The first version of this fork (commit `a5bdbe9`) was
made before PdfCraft could print on Windows and carried its own printing code. PdfCraft 0.6.0
prints on Windows itself, and since the merge this build uses that and nothing else.

## Limits worth knowing

- **Text editing is not a word processor.** A line that mixes fonts is edited piece by piece: the
  rest of the line moves along, but is not re-wrapped, so a much longer piece pushes the line
  into the margin (edit the paragraph after it to re-wrap that). A paragraph that needs more
  lines after an edit grows downwards over whatever is below it. Justified paragraphs come out
  ragged on the right. Undo (Ctrl+Z) takes any of it back.
- When the document's font is not installed on the computer and its embedded letters do not
  cover the new text, the nearest installed standard font is used instead (Times New Roman for
  serif text, Arial, Courier New, or their free equivalents). If none of those is installed
  either, a built-in lookalike is used and spacing on a mixed-font line is approximate.
- Text in scanned pages is a picture. Run **Scan & OCR** first to search it or to find text to
  redact; marking an area by hand works without it.
- Printing on Windows runs a PowerShell script that renders each page with Windows' built-in
  PDF component and sends it to the printer as an image. It needs PowerShell (part of Windows 10
  and 11); where PowerShell is blocked, the Print dialog offers only Save as PDF. A page can
  print slightly differently from how it looks on screen, and long documents make large jobs.
- Edit text before you redact. A redaction box stays where it was drawn, so re-wrapping a
  paragraph afterwards moves the text under it. The redacted text itself is gone either way.

## Building

Rust 1.90 or newer.

```
cargo build --release -p pdfcraft -p pdfcraft-cli
```

The Windows programs in this delivery were cross-compiled on Linux:

```
rustup target add x86_64-pc-windows-gnu          # needs the mingw-w64 toolchain
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
  cargo build --release -p pdfcraft -p pdfcraft-cli --target x86_64-pc-windows-gnu
```

A native build on Windows (`cargo build --release -p pdfcraft`) works the same way.

`cargo xtask models <folder>` downloads the two OCR model files (checked against their SHA-256);
the app looks for them in a `models` folder beside the program.

`pdfcraft-cli` runs the same engine without a window (`pdfcraft-cli tools` lists what it can do),
which is useful for batch work and for testing.

The installer recipes under `packaging/` are PdfCraft's and were not used; see
[packaging/FORK-NOTE.md](packaging/FORK-NOTE.md) before publishing installers from them.

## Tested, and not tested

Tested after the merge with PdfCraft 0.6.0, on Linux and (the Windows programs) under Wine:

- Editing text, filling forms, adding a signature picture, searching and redacting, through the
  app's window and through `pdfcraft-cli`, on documents made by LibreOffice, Chrome, pdfLaTeX,
  reportlab and fpdf2. Saved files were checked with qpdf and Poppler; redacted text was
  confirmed absent from the saved files with two independent text extractors.
- Text edits: 58 line edits across eight documents, each compared pixel by pixel with the
  original outside the edited row (57 identical; the other is a rotated page that the
  comparison itself does not handle, checked by eye). Mixed-font lines and paragraphs after a
  bold lead-in, edited in the Windows app and read back from the saved file.
- Printing on Linux, from the app to a virtual CUPS printer.
- No network use: the Linux app was traced for twenty seconds with a document open and opened
  no network sockets.
- Signature pictures: twelve synthetic photos and scans (12-megapixel phone photos, grey paper,
  soft and hard shadows, a desk around the sheet, ruled paper, pencil, marker, low light).
- The automated tests of every crate that was changed, and the lints PdfCraft requires.

Not tested:

- **Printing on Windows, at all.** It needs PowerShell and Windows' PDF component, which Wine
  does not have: under Wine the app finds no printers and offers only Save as PDF. PdfCraft's
  tests that run the print script only run on Windows, so they did not run here either; only
  the two that check how the printer list is read and how a job is handed to the script did.
  No page has been printed from this build on Windows. Print a test page on each kind of
  printer in use (especially label and large-format printers, and two-sided printing) before
  relying on it.
- A real Windows computer in general: the Windows programs were run only under Wine.
- Real photographs of signatures: the test pictures were generated.
- PdfCraft's full test suite: the suites for the changed areas were run, not all of the roughly
  forty window-level suites.
- The macOS and web builds, which this fork did not touch but also did not build.

The programs are not code-signed, so Windows SmartScreen will warn the first time they run.

## Licence and credits

MIT or Apache-2.0, at your option, as PdfCraft itself: see `LICENSE-MIT`, `LICENSE-APACHE` and
`NOTICE`. Based on PdfCraft by the ArtCraft team and the PdfCraft contributors. The ArtCraft name
and logos are trademarks of the ArtCraft team and are not used by this build.
