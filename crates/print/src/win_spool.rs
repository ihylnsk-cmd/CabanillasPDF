//! Printing on Windows. The print-ready PDF's sheets are drawn at the printer's resolution and
//! handed to the spooler as bitmaps (`pdfcraft-winspool`), which every printer driver accepts and
//! which puts on paper exactly what the screen shows. Copies, two-sided printing, colour and the
//! paper for each sheet go to the driver with the job, on top of the printer's own preferences.

use std::sync::Arc;

use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest, RequestKind, Tile, device_pixels};
use pdfcraft_winspool as winspool;

use super::{Duplex, Job, Printer, send_order};
use crate::PrintError;

/// The finest the sheets are drawn, in dots per inch. Laser text is crisp at 600; a printer that
/// reports more gets these pixels stretched, one that reports less gets its own resolution.
const MAX_DPI: u32 = 600;
/// The renderer's largest tile side, in pixels: a sheet is drawn and sent in pieces this big.
const TILE: u32 = 4096;

fn spool(e: winspool::Error) -> PrintError {
    PrintError::Spool(e.to_string())
}

pub(super) fn printers() -> Vec<Printer> {
    winspool::printers().into_iter().map(|p| Printer { name: p.name, default: p.default }).collect()
}

pub(super) fn submit(pdf: &[u8], job: &Job) -> Result<String, PrintError> {
    let bytes = Arc::new(pdf.to_vec());
    let doc = pdfcraft_cos::Document::open(bytes.clone())?;
    let sheets: Vec<winspool::Paper> = pdfcraft_model::pages(&doc)
        .iter()
        .map(|p| {
            let (width, height) = p.display_size(&doc);
            winspool::Paper { width, height }
        })
        .collect();
    let first = *sheets.first().ok_or(PrintError::NoPages)?;
    let printer = match &job.printer {
        Some(name) => name.clone(),
        None => winspool::printers()
            .into_iter()
            .find(|p| p.default)
            .map(|p| p.name)
            .ok_or_else(|| PrintError::Spool("no default printer is set; choose a printer".into()))?,
    };
    let options = winspool::JobOptions {
        title: job.title.clone(),
        copies: job.copies.clamp(1, 999),
        collate: job.collate,
        duplex: match job.duplex {
            Duplex::Off => winspool::Duplex::Off,
            Duplex::LongEdge => winspool::Duplex::LongEdge,
            Duplex::ShortEdge => winspool::Duplex::ShortEdge,
        },
        grayscale: job.grayscale,
    };
    let mut out = winspool::Job::open(&printer, &options, first).map_err(spool)?;
    let mut renderer = PageRenderer::new(bytes, RenderConfig::default());
    for step in send_order(sheets.len(), out.copies_to_send(), job.collate, job.duplex != Duplex::Off) {
        match step.and_then(|i| sheets.get(i).map(|paper| (i, *paper))) {
            Some((index, paper)) => send_sheet(&mut out, &mut renderer, index, paper, job.grayscale)?,
            // A blank side, so the next copy starts on a fresh sheet of paper.
            None => {
                out.start_page(first).map_err(spool)?;
                out.end_page().map_err(spool)?;
            }
        }
    }
    out.finish().map_err(spool)?;
    Ok(String::new())
}

/// Draw sheet `index` and send it as one page of the job.
fn send_sheet(out: &mut winspool::Job, renderer: &mut PageRenderer, index: usize, paper: winspool::Paper, grayscale: bool) -> Result<(), PrintError> {
    let failed = |why: &str| PrintError::Spool(format!("sheet {} could not be printed: {why}", index + 1));
    let area = out.start_page(paper).map_err(spool)?;
    let place = winspool::place_sheet(paper, area);
    // Pixels per point: the printer's resolution up to MAX_DPI, and never more than the sheet
    // gets on paper (a sheet shrunk to fit other paper needs fewer).
    let dpi = area.dpi.0.min(area.dpi.1).clamp(72, MAX_DPI);
    let fit = f64::from(place.width) / paper.width.max(1.0);
    let scale = (f64::from(dpi) / 72.0).min(fit).max(0.25) as f32;
    let (w, h) = (device_pixels(paper.width as f32, scale), device_pixels(paper.height as f32, scale));
    // Tile edges on the paper, from the same rounding on both sides of an edge: no seams.
    let across = |x: u32| place.x.saturating_add((f64::from(x) * f64::from(place.width) / f64::from(w.max(1))).round() as i32);
    let down = |y: u32| place.y.saturating_add((f64::from(y) * f64::from(place.height) / f64::from(h.max(1))).round() as i32);
    let mut y = 0;
    while y < h {
        let rows = (h - y).min(TILE);
        let mut x = 0;
        while x < w {
            let cols = (w - x).min(TILE);
            let tile =
                renderer.render(RenderRequest { page: index, kind: RequestKind::Pixels, tile: Some(Tile { x, y, w: cols, h: rows }), scale, tag: 0 });
            if let Some(e) = &tile.error {
                return Err(failed(e));
            }
            let bgr = winspool::pack_bgr(&tile.rgba, tile.width, tile.height, grayscale).ok_or_else(|| failed("the page image is incomplete"))?;
            let at = winspool::Place {
                x: across(x),
                y: down(y),
                width: (across(x + tile.width) - across(x)).max(1) as u32,
                height: (down(y + tile.height) - down(y)).max(1) as u32,
            };
            out.draw(&bgr, tile.width, tile.height, at).map_err(spool)?;
            x += cols;
        }
        y += rows;
    }
    out.end_page().map_err(spool)
}
