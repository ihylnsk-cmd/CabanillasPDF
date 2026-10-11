//! Turning a photo or scan of a signature into ink on a clear background.
//!
//! A flatbed scan has white paper, and a fixed "near white" cutoff clears it. A phone photo does
//! not: the paper is grey, darker towards one side, sometimes under the shadow of the phone, and
//! it carries sensor noise. The sheet may not fill the frame either. So:
//!
//! 1. The paper level is measured where it is, on a fine grid over the picture, and every pixel
//!    is judged against the paper around it ([`clear`]). For a clean scan the result is the same
//!    as the fixed cutoff.
//! 2. What is left is ink, plus whatever else in the photo was darker than its surroundings: the
//!    edge of the sheet, a ruled line, the rim of a shadow. The signature is the main body of
//!    dark ink; marks away from it are dropped and the picture is cropped to it ([`isolate`]).

use image::RgbaImage;

/// Share of the paper level at and above which a pixel is paper, for a noise-free picture, and
/// the range below it over which ink fades in. As the fixed cutoff: 245 and 35 of 255.
const CLEAR_AT: f32 = 245.0 / 255.0;
const FADE: f32 = 35.0 / 255.0;
/// How far below the point where paper ends a pixel must be to count as the pen's ink rather
/// than something faint (a ruled line, the rim of a shadow).
const STRONG: f32 = 0.25;
/// Pictures whose background is darker than this are not ink on paper; only near-white is
/// cleared from them.
const DARKEST_PAPER: u8 = 96;
/// A pixel's level relative to the paper around it is counted in 1/256ths, up to 1.5.
const UNIT: f32 = 256.0;
const TOP: usize = 384;

const CLEAR: [u8; 4] = [255, 255, 255, 0];

/// A decoded picture made ready to place: paper cleared, cropped to the signature.
pub(crate) fn prepare(mut rgba: RgbaImage) -> RgbaImage {
    match clear(&mut rgba) {
        Some(strong) => isolate(rgba, &strong),
        None => crop(rgba),
    }
}

/// How light a pixel is: its darkest channel, so coloured ink counts as ink.
fn whiteness(p: &image::Rgba<u8>) -> u8 {
    p.0[0].min(p.0[1]).min(p.0[2])
}

/// The level below which `share` of the counted values fall.
fn percentile(hist: &[u32], share: f32) -> usize {
    let total: u64 = hist.iter().map(|&n| u64::from(n)).sum();
    let want = (total as f64 * f64::from(share)).ceil().max(1.0) as u64;
    let mut seen = 0u64;
    for (level, &n) in hist.iter().enumerate() {
        seen += u64::from(n);
        if seen >= want {
            return level;
        }
    }
    hist.len().saturating_sub(1)
}

/// Make the paper transparent and leave the ink, in place. Returns which pixels are strong ink,
/// or `None` for a picture that is not ink on paper (then only near-white was cleared).
fn clear(rgba: &mut RgbaImage) -> Option<Vec<bool>> {
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    if w == 0 || h == 0 {
        return None;
    }
    let white: Vec<u8> = rgba.pixels().map(whiteness).collect();
    let mut hist = [0u32; 256];
    for &v in &white {
        // A u8 always indexes 256 counters.
        hist[usize::from(v)] += 1;
    }
    let paper = percentile(&hist, 0.85);
    // A picture that comes with a transparent background was prepared by someone: its ink is
    // left alone, and only near-white still in it is cleared (as before this module existed).
    let clear_pixels = rgba.pixels().filter(|p| p.0[3] < 250).count();
    if clear_pixels * 200 > w * h || paper < usize::from(DARKEST_PAPER) {
        for p in rgba.pixels_mut() {
            let keep = ((CLEAR_AT - f32::from(whiteness(p)) / 255.0) / FADE).clamp(0.0, 1.0);
            p.0[3] = (f32::from(p.0[3]) * keep).round() as u8;
        }
        return None;
    }
    let levels = Levels::measure(&white, w, h, paper as f32);
    // Each pixel relative to the paper around it, counted in 1/256ths.
    let mut spread = [0u32; TOP + 1];
    for (y, row) in white.chunks(w).enumerate() {
        for (&v, paper) in row.iter().zip(levels.row(&levels.darkest, y)) {
            let bin = (f32::from(v) / paper * UNIT).round().clamp(0.0, TOP as f32) as usize;
            if let Some(n) = spread.get_mut(bin) {
                *n += 1;
            }
        }
    }
    // Noise: how far paper pixels stray above their middle. Above, not below, because below is
    // where the soft edges of the ink are. Paper stays clear down to the same distance beneath.
    // Only pixels near the paper level are counted: darker is ink, and far lighter is lit
    // paper beside a shadow or a blot, judged against the darker level next to it.
    let (from, to) = ((0.75 * UNIT) as usize, (1.25 * UNIT) as usize);
    let paper_part = spread.get(from..=to).unwrap_or(&[]);
    let middle = percentile(paper_part, 0.5) + from;
    let high = percentile(paper_part, 0.99) + from;
    let noise = high.saturating_sub(middle) as f32 / UNIT;
    let clear_at = (middle as f32 / UNIT - 1.25 * noise - 0.01).clamp(0.70, CLEAR_AT);
    let mut strong = Vec::with_capacity(w * h);
    for (y, (row, white)) in rgba.rows_mut().zip(white.chunks(w)).enumerate() {
        let light = levels.row(&levels.cells, y).zip(levels.row(&levels.darkest, y));
        for ((p, &v), (level, paper)) in row.zip(white).zip(light) {
            let depth = clear_at - f32::from(v) / paper;
            let alpha = (f32::from(p.0[3]) * (depth / FADE).clamp(0.0, 1.0)).round() as u8;
            strong.push(depth >= STRONG && alpha == 255);
            if alpha == 0 {
                p.0 = CLEAR;
                continue;
            }
            // Ink as it would look on white paper in even light.
            let gain = 255.0 / level;
            for c in p.0.iter_mut().take(3) {
                *c = (f32::from(*c) * gain).round().clamp(0.0, 255.0) as u8;
            }
            p.0[3] = alpha;
        }
    }
    // Lone faint pixels are paper noise that fell just under the paper level. A pen stroke,
    // even a faint one, has neighbours.
    let sums = neighbourhood(rgba);
    for (p, &sum) in rgba.pixels_mut().zip(&sums) {
        if sum < 9 * 20 {
            p.0 = CLEAR;
        }
    }
    Some(strong)
}

/// The paper level over the picture: measured per cell of a grid, read back smoothly.
struct Levels {
    cols: usize,
    rows: usize,
    /// The paper level in each cell: how brightly the sheet is lit there.
    cells: Vec<f32>,
    /// The darkest paper level in each cell and the cells around it. Pixels are judged against
    /// this one: at the edge of a shadow the cells are lit on one side and not on the other, and
    /// paper on the dark side must not pass for ink because its cell is mostly lit.
    darkest: Vec<f32>,
    /// For each pixel column: the two grid columns it lies between and how far along.
    across: Vec<(usize, usize, f32)>,
    height: usize,
}

impl Levels {
    fn measure(white: &[u8], w: usize, h: usize, paper: f32) -> Self {
        let long = w.max(h);
        // Cells of a few pixels, so the edge of a shadow is followed closely.
        let cell = (long / 256).max(4);
        let (cols, rows) = (w.div_ceil(cell).max(1), h.div_ceil(cell).max(1));
        // The level a fifth of the cell is lighter than: paper, unless ink covers most of it.
        let mut measured = Vec::with_capacity(cols * rows);
        let mut hist = [0u32; 256];
        for cy in 0..rows {
            let (y0, y1) = (cy * h / rows, (cy + 1) * h / rows);
            for cx in 0..cols {
                let (x0, x1) = (cx * w / cols, (cx + 1) * w / cols);
                hist.fill(0);
                for y in y0..y1 {
                    for &v in white.get(y * w + x0..y * w + x1).unwrap_or(&[]) {
                        // A u8 always indexes 256 counters.
                        hist[usize::from(v)] += 1;
                    }
                }
                measured.push(percentile(&hist, 0.80) as f32);
            }
        }
        // A cell under a pen stroke holds ink, not paper. Take its paper from around it: the
        // lightest cell within reach, then the darkest of those within reach. That closes over
        // anything narrower than twice the reach, a twelfth of the picture, which a pen stroke
        // is and a shadow is not; and it leaves the edge of a shadow where it is.
        let reach = (long / 24).div_ceil(cell).max(2);
        let lightest = spread(&measured, cols, rows, reach, f32::max);
        let closed = spread(&lightest, cols, rows, reach, f32::min);
        // Ink wider still (a blot, a thick marker) is not paper either: nothing counts as paper
        // below a fifth of the picture's paper level. A shadow or a desk is rarely that dark.
        let cells: Vec<f32> = closed.iter().map(|&level| level.max(paper * 0.2).max(1.0)).collect();
        let darkest = spread(&cells, cols, rows, 1, f32::min);
        let across = (0..w).map(|x| between(x, w, cols)).collect();
        Self { cols, rows, cells, darkest, across, height: h }
    }

    /// One of the two levels (`cells` or `darkest`) at each pixel of row `y`.
    fn row<'a>(&'a self, levels: &'a [f32], y: usize) -> impl Iterator<Item = f32> + 'a {
        let (top, bottom, down) = between(y, self.height, self.rows);
        let cell = move |row: usize, col: usize| levels.get(row * self.cols + col).copied().unwrap_or(255.0);
        self.across.iter().map(move |&(left, right, along)| {
            let upper = cell(top, left) + (cell(top, right) - cell(top, left)) * along;
            let lower = cell(bottom, left) + (cell(bottom, right) - cell(bottom, left)) * along;
            (upper + (lower - upper) * down).max(1.0)
        })
    }
}

/// Each cell replaced by `pick` over the cells within `reach` of it, along rows then columns.
fn spread(cells: &[f32], cols: usize, rows: usize, reach: usize, pick: fn(f32, f32) -> f32) -> Vec<f32> {
    let mut across = Vec::with_capacity(cells.len());
    for row in cells.chunks(cols.max(1)) {
        for col in 0..row.len() {
            let window = row.get(col.saturating_sub(reach)..(col + reach + 1).min(row.len())).unwrap_or(&[]);
            across.push(window.iter().copied().reduce(pick).unwrap_or(255.0));
        }
    }
    let mut out = Vec::with_capacity(cells.len());
    for row in 0..rows {
        for col in 0..cols {
            let window = (row.saturating_sub(reach)..(row + reach + 1).min(rows)).filter_map(|r| across.get(r * cols + col).copied());
            out.push(window.reduce(pick).unwrap_or(255.0));
        }
    }
    out
}

/// Position `i` of `len` among `cells` cell centres: the cells either side and the share of the
/// way from the first to the second.
fn between(i: usize, len: usize, cells: usize) -> (usize, usize, f32) {
    let last = cells.max(1) - 1;
    let at = ((i as f32 + 0.5) * cells as f32 / len.max(1) as f32 - 0.5).clamp(0.0, last as f32);
    let first = (at.floor() as usize).min(last);
    (first, (first + 1).min(last), at - first as f32)
}

/// The sum of each pixel's alpha with its eight neighbours'.
fn neighbourhood(rgba: &RgbaImage) -> Vec<u16> {
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    let alpha: Vec<u8> = rgba.pixels().map(|p| p.0[3]).collect();
    // Three across, then three down.
    let mut wide = vec![0u16; w * h];
    for (row, out) in alpha.chunks(w.max(1)).zip(wide.chunks_mut(w.max(1))) {
        for (x, out) in out.iter_mut().enumerate() {
            let range = x.saturating_sub(1)..(x + 2).min(w);
            *out = row.get(range).unwrap_or(&[]).iter().map(|&a| u16::from(a)).sum();
        }
    }
    let mut sums = vec![0u16; w * h];
    for (y, out) in sums.chunks_mut(w.max(1)).enumerate() {
        for near in y.saturating_sub(1)..(y + 2).min(h) {
            let row = wide.get(near * w..(near + 1) * w).unwrap_or(&[]);
            for (out, &n) in out.iter_mut().zip(row) {
                *out += n;
            }
        }
    }
    sums
}

/// A group of grid cells holding strong ink, each within reach of another.
struct Body {
    mass: u64,
    touches_edge: bool,
}

/// Keep the signature, the main body of strong ink, and what lies close to it; clear marks away
/// from it; crop to it.
fn isolate(mut rgba: RgbaImage, strong: &[bool]) -> RgbaImage {
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    let long = w.max(h);
    // A coarse grid; cells within `reach` of each other belong together, so the gap between a
    // first name and a surname, or up to the dot of an i, does not split a signature.
    let cell = (long / 96).max(4);
    let reach = 6usize;
    let (cols, rows) = (w.div_ceil(cell).max(1), h.div_ceil(cell).max(1));
    let mut mass = vec![0u32; cols * rows];
    for (y, row) in strong.chunks(w.max(1)).enumerate() {
        for (x, _) in row.iter().enumerate().filter(|(_, s)| **s) {
            if let Some(n) = mass.get_mut(y / cell * cols + x / cell) {
                *n += 1;
            }
        }
    }
    // A cell holds ink when a stroke crosses it, not when a speck fell in it.
    let least = (cell / 2).max(2) as u32;
    let holds = |col: usize, row: usize| mass.get(row * cols + col).is_some_and(|&n| n >= least);
    // Which body each cell is part of.
    let mut body_of: Vec<Option<usize>> = vec![None; cols * rows];
    let mut bodies: Vec<Body> = Vec::new();
    let mut queue = Vec::new();
    for start in 0..cols * rows {
        if !holds(start % cols, start / cols) || body_of.get(start).copied().flatten().is_some() {
            continue;
        }
        let id = bodies.len();
        let mut body = Body { mass: 0, touches_edge: false };
        if let Some(slot) = body_of.get_mut(start) {
            *slot = Some(id);
        }
        queue.push(start);
        while let Some(at) = queue.pop() {
            let (col, row) = (at % cols, at / cols);
            body.mass += u64::from(mass.get(at).copied().unwrap_or(0));
            body.touches_edge |= col == 0 || row == 0 || col + 1 == cols || row + 1 == rows;
            for r in row.saturating_sub(reach)..(row + reach + 1).min(rows) {
                for c in col.saturating_sub(reach)..(col + reach + 1).min(cols) {
                    let next = r * cols + c;
                    if holds(c, r)
                        && let Some(slot) = body_of.get_mut(next)
                        && slot.is_none()
                    {
                        *slot = Some(id);
                        queue.push(next);
                    }
                }
            }
        }
        bodies.push(body);
    }
    // The signature: the heaviest body. One that runs off the picture is more likely the edge
    // of the sheet or a shadow than a signature, so it has to be four times as heavy to win.
    let weight = |b: &Body| if b.touches_edge { b.mass } else { b.mass.saturating_mul(4) };
    let Some(main) = bodies.iter().max_by_key(|b| weight(b)) else {
        // No strong ink (faint pencil, a blank sheet): nothing to tell apart, crop to what is there.
        return crop(rgba);
    };
    // Other bodies are part of it (a second word, an underline) when they are substantial too.
    // If the signature lies clear of the picture's edges, nothing that runs off them is.
    let kept: Vec<bool> = bodies.iter().map(|b| b.mass.saturating_mul(5) >= main.mass && (main.touches_edge || !b.touches_edge)).collect();
    let keeps = |col: usize, row: usize| body_of.get(row * cols + col).copied().flatten().is_some_and(|id| kept.get(id).copied().unwrap_or(false));
    // Cells within reach of the signature stay as they are, faint strokes included.
    let mut near = vec![false; cols * rows];
    for row in 0..rows {
        for col in 0..cols {
            if keeps(col, row) {
                for r in row.saturating_sub(reach)..(row + reach + 1).min(rows) {
                    for slot in near.get_mut(r * cols + col.saturating_sub(reach)..r * cols + (col + reach + 1).min(cols)).unwrap_or(&mut []) {
                        *slot = true;
                    }
                }
            }
        }
    }
    for (y, row) in rgba.rows_mut().enumerate() {
        for (x, p) in row.enumerate() {
            if !near.get(y / cell * cols + x / cell).copied().unwrap_or(false) {
                p.0 = CLEAR;
            }
        }
    }
    crop(rgba)
}

/// Crop to the ink with a small margin. A picture with no ink comes back as it is.
fn crop(rgba: RgbaImage) -> RgbaImage {
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    if w == 0 || h == 0 {
        return rgba;
    }
    // Solid ink first, so a faint haze around the signature doesn't hold the frame open; any
    // ink at all otherwise.
    let frame = |least: u8| {
        let mut found: Option<[usize; 4]> = None;
        for (i, _) in rgba.pixels().enumerate().filter(|(_, p)| p.0[3] >= least) {
            let (x, y) = (i % w, i / w);
            let f = found.get_or_insert([x, y, x, y]);
            *f = [f[0].min(x), f[1].min(y), f[2].max(x), f[3].max(y)];
        }
        found
    };
    let Some([x0, y0, x1, y1]) = frame(128).or_else(|| frame(32)) else { return rgba };
    let margin = w.max(h) / 50 + 2;
    let (x0, y0) = (x0.saturating_sub(margin), y0.saturating_sub(margin));
    let (x1, y1) = ((x1 + margin).min(w - 1), (y1 + margin).min(h - 1));
    if x0 == 0 && y0 == 0 && x1 == w - 1 && y1 == h - 1 {
        return rgba;
    }
    image::imageops::crop_imm(&rgba, x0 as u32, y0 as u32, (x1 - x0 + 1) as u32, (y1 - y0 + 1) as u32).to_image()
}
