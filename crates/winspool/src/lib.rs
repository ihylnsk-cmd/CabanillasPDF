//! Printing through the Windows spooler: list the installed printers, open a job on one with
//! copies, two-sided and colour settings, and send each sheet as a bitmap at the printer's
//! resolution (GDI: `CreateDC`, `StartDoc`, `StartPage`, `StretchDIBits`, `EndPage`, `EndDoc`).
//!
//! Printing bitmaps is what Acrobat calls "Print as image": every driver can do it, and the page
//! comes out exactly as it is drawn on screen. The caller renders; this crate only talks to the
//! spooler.
//!
//! The workspace forbids `unsafe`. Win32 can't be called without it, so this crate holds all of
//! it (`win`), behind the safe types here. Everything that needs no system call (paper sizes,
//! pixel packing) is plain Rust in this file, built and tested on every platform.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
// Off Windows only the portable half is built, and nothing calls its crate-private helpers.
#![cfg_attr(not(windows), allow(dead_code))]

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::{Job, printers};

/// An installed printer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Printer {
    pub name: String,
    pub default: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Duplex {
    #[default]
    Off,
    LongEdge,
    ShortEdge,
}

/// What the spooler is asked for when a job opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobOptions {
    /// The job's name in the print queue.
    pub title: String,
    pub copies: u32,
    pub collate: bool,
    pub duplex: Duplex,
    pub grayscale: bool,
}

/// A sheet as the job lays it out, in points; wider than tall means landscape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Paper {
    pub width: f64,
    pub height: f64,
}

/// The sheet the printer will feed, in its own pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageArea {
    /// Device pixels per inch.
    pub dpi: (u32, u32),
    /// The whole sheet.
    pub paper: (u32, u32),
    /// The top-left corner of the area the printer can mark, on the sheet.
    pub offset: (u32, u32),
    /// The size of that area.
    pub printable: (u32, u32),
}

/// Where a band of pixels goes on the sheet, in device pixels from the sheet's top-left corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Why the spooler refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Windows' paper codes (`DMPAPER_*`) for the sizes drivers know by name, with their portrait
/// size in points.
const PAPERS: &[(i16, f64, f64)] = &[
    (1, 612.0, 792.0),    // Letter
    (5, 612.0, 1008.0),   // Legal
    (3, 792.0, 1224.0),   // Tabloid
    (7, 522.0, 756.0),    // Executive
    (8, 841.89, 1190.55), // A3
    (9, 595.28, 841.89),  // A4
    (11, 419.53, 595.28), // A5
    (12, 728.5, 1031.81), // B4 (JIS)
    (13, 515.91, 728.5),  // B5 (JIS)
    (6, 396.0, 612.0),    // Statement
];

/// How a sheet is described to the driver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PaperRequest {
    pub landscape: bool,
    /// A size the driver knows by name, or `None` for a custom size.
    pub code: Option<i16>,
    /// The portrait size in tenths of a millimetre (for custom sizes).
    pub tenths_mm: (i16, i16),
}

/// The driver's description of `paper`: its orientation and the named size within 2 pt of it,
/// or its measurements.
pub(crate) fn paper_request(paper: Paper) -> PaperRequest {
    let ok = |v: f64| if v.is_finite() && v > 0.0 { v } else { 612.0 };
    let (w, h) = (ok(paper.width), ok(paper.height));
    let landscape = w > h;
    let (short, long) = if landscape { (h, w) } else { (w, h) };
    let code = PAPERS.iter().find(|(_, pw, ph)| (pw - short).abs() <= 2.0 && (ph - long).abs() <= 2.0).map(|(c, _, _)| *c);
    // 1 pt = 25.4 / 72 mm. Clamped to what the field holds (3.2 m).
    let tenths = |pt: f64| (pt * 254.0 / 72.0).round().clamp(1.0, 32_000.0) as i16;
    PaperRequest { landscape, code, tenths_mm: (tenths(short), tenths(long)) }
}

/// Bytes in one row of a 24-bit bitmap `width` pixels wide (rows are padded to four bytes).
pub fn stride(width: u32) -> usize {
    (width as usize).saturating_mul(3).saturating_add(3) & !3
}

/// Rows of opaque RGBA pixels, top row first, as the bottom-up 24-bit BGR rows GDI draws
/// (every printer driver takes those; some mishandle top-down bitmaps). With `grayscale`, each
/// pixel becomes its luminance. `None` when `rgba` is shorter than `width` × `height` pixels or
/// the size overflows.
pub fn pack_bgr(rgba: &[u8], width: u32, height: u32, grayscale: bool) -> Option<Vec<u8>> {
    let (w, h) = (width as usize, height as usize);
    let row = stride(width);
    let need = w.checked_mul(h)?.checked_mul(4)?;
    if rgba.len() < need || w == 0 || h == 0 {
        return None;
    }
    let mut out = vec![0u8; row.checked_mul(h)?];
    for (y, src) in rgba.chunks_exact(w * 4).take(h).enumerate() {
        let start = (h - 1 - y) * row;
        let dst = out.get_mut(start..start + w * 3)?;
        for (d, s) in dst.chunks_exact_mut(3).zip(src.chunks_exact(4)) {
            let (r, g, b) = (s[0], s[1], s[2]);
            if grayscale {
                // Rec. 601 luma, in integers.
                let l = ((u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114 + 500) / 1000) as u8;
                d.copy_from_slice(&[l, l, l]);
            } else {
                d.copy_from_slice(&[b, g, r]);
            }
        }
    }
    Some(out)
}

/// Where a sheet `paper` points in size lands on the printer's sheet: at its real size from the
/// top-left corner when the printer feeds that paper (within 2 %), otherwise scaled to fit and
/// centred, so nothing is cut off when the driver substitutes another size.
pub fn place_sheet(paper: Paper, area: PageArea) -> Place {
    let (dx, dy) = (f64::from(area.dpi.0.max(1)), f64::from(area.dpi.1.max(1)));
    let (pw, ph) = (f64::from(area.paper.0.max(1)), f64::from(area.paper.1.max(1)));
    let ok = |v: f64| if v.is_finite() && v > 0.0 { v } else { 612.0 };
    let (w, h) = (ok(paper.width) / 72.0 * dx, ok(paper.height) / 72.0 * dy);
    if (w - pw).abs() <= pw * 0.02 && (h - ph).abs() <= ph * 0.02 {
        return Place { x: 0, y: 0, width: w.round().max(1.0) as u32, height: h.round().max(1.0) as u32 };
    }
    let k = (pw / w).min(ph / h);
    let (fw, fh) = ((w * k).floor().max(1.0), (h * k).floor().max(1.0));
    Place { x: ((pw - fw) / 2.0).round() as i32, y: ((ph - fh) / 2.0).round() as i32, width: fw as u32, height: fh as u32 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_and_custom_paper() {
        assert_eq!(paper_request(Paper { width: 612.0, height: 792.0 }), PaperRequest { landscape: false, code: Some(1), tenths_mm: (2159, 2794) });
        assert_eq!(paper_request(Paper { width: 841.89, height: 595.28 }), PaperRequest { landscape: true, code: Some(9), tenths_mm: (2100, 2970) });
        assert_eq!(paper_request(Paper { width: 1224.0, height: 792.0 }).code, Some(3));
        let custom = paper_request(Paper { width: 300.0, height: 400.0 });
        assert_eq!((custom.landscape, custom.code, custom.tenths_mm), (false, None, (1058, 1411)));
        // Nonsense sizes fall back to something a driver accepts instead of overflowing.
        let odd = paper_request(Paper { width: f64::NAN, height: 1.0e12 });
        assert_eq!((odd.landscape, odd.tenths_mm), (false, (2159, 32_000)));
    }

    #[test]
    fn pixels_are_packed_bottom_up_in_bgr_rows_padded_to_four_bytes() {
        // 3 × 2: red, green, blue over white, black, grey.
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 0, 255, 100, 100, 100, 255];
        assert_eq!(stride(3), 12);
        let bgr = pack_bgr(&rgba, 3, 2, false).unwrap();
        assert_eq!(bgr.len(), 24);
        assert_eq!(&bgr[..12], &[255, 255, 255, 0, 0, 0, 100, 100, 100, 0, 0, 0], "the bottom row first, then padding");
        assert_eq!(&bgr[12..], &[0, 0, 255, 0, 255, 0, 255, 0, 0, 0, 0, 0]);
        let grey = pack_bgr(&rgba, 3, 2, true).unwrap();
        assert_eq!(&grey[12..21], &[76, 76, 76, 150, 150, 150, 29, 29, 29]);
        assert!(pack_bgr(&rgba, 3, 3, false).is_none() && pack_bgr(&rgba, 0, 2, false).is_none());
        assert!(pack_bgr(&rgba, u32::MAX, u32::MAX, false).is_none());
    }

    #[test]
    fn sheets_print_at_their_size_or_shrink_to_the_paper_fed() {
        let letter = PageArea { dpi: (600, 600), paper: (5100, 6600), offset: (100, 100), printable: (4900, 6400) };
        assert_eq!(place_sheet(Paper { width: 612.0, height: 792.0 }, letter), Place { x: 0, y: 0, width: 5100, height: 6600 });
        // A4 on Letter paper: scaled to the sheet's height and centred.
        let a4 = place_sheet(Paper { width: 595.28, height: 841.89 }, letter);
        assert_eq!((a4.height, a4.y), (6600, 0));
        assert!(a4.width < 5100 && a4.x > 0 && (a4.x * 2 + a4.width as i32 - 5100).abs() <= 1, "{a4:?}");
        // A landscape sheet the driver did not turn the paper for still fits.
        let wide = place_sheet(Paper { width: 792.0, height: 612.0 }, letter);
        assert_eq!((wide.width, wide.x), (5100, 0));
        assert!(wide.height < 6600 && wide.y > 0);
        // Different resolutions across and down the sheet.
        let fax = PageArea { dpi: (200, 100), paper: (1700, 1100), offset: (0, 0), printable: (1700, 1100) };
        assert_eq!(place_sheet(Paper { width: 612.0, height: 792.0 }, fax), Place { x: 0, y: 0, width: 1700, height: 1100 });
    }
}
