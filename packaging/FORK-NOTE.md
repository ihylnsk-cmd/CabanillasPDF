# Packaging in this modified build

The scripts in this directory are PdfCraft's own installer and package recipes, kept so the
fork stays easy to compare with upstream. This build was delivered as a portable program and
did **not** use them.

Before publishing installers or packages built from them, give the fork its own identity so it
cannot be mistaken for, or upgrade over, an official PdfCraft install:

- `windows/pdfcraft.wxs`: the MSI `UpgradeCode`, `Manufacturer` and product name.
- `linux/`: the application id `ai.storyteller.pdfcraft` (desktop file, AppStream metadata, MIME
  file, Flatpak manifest) and the `vendor` / `maintainer` fields in `nfpm.yaml`.
- `macos/Info.plist.in`: the bundle identifier and the signing identity.

The links in these files now point at the PdfCraft source repository only.
