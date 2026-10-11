//! The Win32 calls. This is the only file in the workspace with `unsafe` code; every block says
//! why it is sound. Nothing here trusts the spooler further than it must: sizes it reports are
//! bounded, and strings it returns are read only inside the buffer they were written to.

use std::mem::size_of;
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, COLORONCOLOR, CreateDCW, DEVMODEW, DIB_RGB_COLORS, DM_COLLATE, DM_COLOR, DM_COPIES, DM_DUPLEX,
    DM_IN_BUFFER, DM_ORIENTATION, DM_OUT_BUFFER, DM_PAPERLENGTH, DM_PAPERSIZE, DM_PAPERWIDTH, DMCOLLATE_FALSE, DMCOLLATE_TRUE, DMCOLOR_COLOR,
    DMCOLOR_MONOCHROME, DMDUP_HORIZONTAL, DMDUP_SIMPLEX, DMDUP_VERTICAL, DeleteDC, GetDeviceCaps, HALFTONE, HDC, HORZRES, LOGPIXELSX, LOGPIXELSY,
    PHYSICALHEIGHT, PHYSICALOFFSETX, PHYSICALOFFSETY, PHYSICALWIDTH, ResetDCW, SRCCOPY, SetBrushOrgEx, SetStretchBltMode, StretchDIBits, VERTRES,
};
use windows_sys::Win32::Graphics::Printing::{
    ClosePrinter, DocumentPropertiesW, EnumPrintersW, GetDefaultPrinterW, OpenPrinterW, PRINTER_ENUM_CONNECTIONS, PRINTER_ENUM_LOCAL, PRINTER_HANDLE,
    PRINTER_INFO_4W,
};
use windows_sys::Win32::Storage::Xps::{AbortDoc, DOCINFOW, EndDoc, EndPage, StartDocW, StartPage};

use crate::{Duplex, Error, JobOptions, PageArea, Paper, PaperRequest, Place, Printer, paper_request, stride};

/// The most bytes a printer list or a driver's settings block may claim to need.
const MAX_SPOOLER_BYTES: u32 = 8 << 20;
/// `DMPAPER_USER`: the size is given by its measurements.
const PAPER_CUSTOM: i16 = 256;
/// What `DocumentProperties` returns when it filled the buffer (`IDOK`).
const OK: i32 = 1;

/// `s` as a NUL-terminated UTF-16 string (without any NUL of its own, which would cut it short).
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().filter(|u| *u != 0).chain(std::iter::once(0)).collect()
}

fn failed(what: &str) -> Error {
    // SAFETY: GetLastError takes no arguments and only reads the calling thread's last-error value.
    let code = unsafe { GetLastError() };
    Error(format!("{what} (Windows error {code})"))
}

/// The NUL-terminated UTF-16 string at `p`, when it lies wholly inside the buffer `base..end`.
fn wide_in(p: *const u16, base: usize, end: usize) -> Option<String> {
    let addr = p as usize;
    if p.is_null() || addr < base || addr >= end || !addr.is_multiple_of(2) {
        return None;
    }
    let max = (end - addr) / 2;
    // SAFETY: `addr..addr + max * 2` lies inside the caller's live, initialised buffer (checked
    // above), and `p` is aligned for u16 (checked above).
    let units = unsafe { std::slice::from_raw_parts(p, max) };
    let len = units.iter().position(|u| *u == 0)?;
    Some(String::from_utf16_lossy(units.get(..len)?))
}

fn default_printer() -> Option<String> {
    let mut len = 0u32;
    // SAFETY: a null buffer with length 0 is the documented way to ask for the length needed;
    // `len` is valid for writes.
    unsafe { GetDefaultPrinterW(null_mut(), &mut len) };
    if len == 0 || len > 4096 {
        return None;
    }
    let mut buf = vec![0u16; len as usize];
    // SAFETY: `buf` holds `len` UTF-16 units, the size passed.
    if unsafe { GetDefaultPrinterW(buf.as_mut_ptr(), &mut len) } == 0 {
        return None;
    }
    let end = buf.iter().position(|u| *u == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(buf.get(..end)?))
}

/// The printers installed for this user (local ones and connections to shared ones), the default
/// marked. Empty when there are none or the spooler isn't running.
pub fn printers() -> Vec<Printer> {
    let default = default_printer();
    let flags = PRINTER_ENUM_LOCAL | PRINTER_ENUM_CONNECTIONS;
    let (mut needed, mut count) = (0u32, 0u32);
    // SAFETY: a null buffer of size 0 only reports the size needed; both out-pointers are valid.
    unsafe { EnumPrintersW(flags, null(), 4, null_mut(), 0, &mut needed, &mut count) };
    if needed == 0 || needed > MAX_SPOOLER_BYTES {
        return Vec::new();
    }
    // Words, not bytes: PRINTER_INFO_4W holds pointers, so the buffer must be aligned for them.
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    let bytes = buf.len() * 8;
    // SAFETY: `buf` is valid for writes of `bytes` bytes (its length × 8) and aligned for
    // PRINTER_INFO_4W; `bytes` fits u32 because `needed` is at most MAX_SPOOLER_BYTES.
    if unsafe { EnumPrintersW(flags, null(), 4, buf.as_mut_ptr().cast(), bytes as u32, &mut needed, &mut count) } == 0 {
        return Vec::new();
    }
    let base = buf.as_ptr() as usize;
    let fits = bytes / size_of::<PRINTER_INFO_4W>();
    let mut out: Vec<Printer> = Vec::new();
    for i in 0..(count as usize).min(fits) {
        // SAFETY: `i < fits`, so the i-th structure lies inside `buf`, which is aligned for it
        // and was filled by the spooler (or is zero, which is a valid value: null pointers).
        let info = unsafe { buf.as_ptr().cast::<PRINTER_INFO_4W>().add(i).read() };
        if let Some(name) = wide_in(info.pPrinterName, base, base + bytes)
            && !name.is_empty()
            && !out.iter().any(|p| p.name == name)
        {
            out.push(Printer { default: default.as_deref() == Some(name.as_str()), name });
        }
    }
    out
}

/// A printer's settings (`DEVMODE`): the public structure followed by the driver's private data,
/// in a buffer aligned for it.
struct Settings {
    words: Vec<u64>,
}

impl Settings {
    fn as_ptr(&self) -> *const DEVMODEW {
        self.words.as_ptr().cast()
    }

    fn as_mut_ptr(&mut self) -> *mut DEVMODEW {
        self.words.as_mut_ptr().cast()
    }

    fn get(&self) -> &DEVMODEW {
        // SAFETY: `words` is at least size_of::<DEVMODEW>() bytes (checked where it is
        // created), aligned to 8, and initialised; DEVMODEW is integers and arrays of them, valid
        // for every bit pattern.
        unsafe { &*self.as_ptr() }
    }

    fn get_mut(&mut self) -> &mut DEVMODEW {
        // SAFETY: as in `get`, and `&mut self` makes the borrow unique.
        unsafe { &mut *self.as_mut_ptr() }
    }
}

/// An open print job. Dropping it before [`Job::finish`] cancels the job.
pub struct Job {
    name: Vec<u16>,
    printer: PRINTER_HANDLE,
    dc: HDC,
    settings: Settings,
    paper: Option<PaperRequest>,
    started: bool,
    in_page: bool,
    send: u32,
}

impl Job {
    /// Open a job on `printer` with these options, set up for a first sheet of size `first`.
    pub fn open(printer: &str, options: &JobOptions, first: Paper) -> Result<Job, Error> {
        let name = wide(printer);
        let mut handle = PRINTER_HANDLE { Value: null_mut() };
        // SAFETY: `name` is NUL-terminated and outlives the call; `handle` is valid for writes;
        // null defaults ask for ordinary use access.
        if unsafe { OpenPrinterW(name.as_ptr(), &mut handle, null()) } == 0 || handle.Value.is_null() {
            return Err(failed(&format!("the printer \"{printer}\" could not be opened")));
        }
        // From here `job` owns the handle: an early return closes it (Drop).
        let mut job = Job {
            name,
            printer: handle,
            dc: null_mut(),
            settings: Settings { words: Vec::new() },
            paper: None,
            started: false,
            in_page: false,
            send: 1,
        };
        job.settings = job.read_settings()?;
        job.apply(options, first)?;
        let driver = wide("WINSPOOL");
        // SAFETY: both strings are NUL-terminated and outlive the call; the settings block is the
        // one this printer's driver produced and checked.
        let dc = unsafe { CreateDCW(driver.as_ptr(), job.name.as_ptr(), null(), job.settings.as_ptr()) };
        if dc.is_null() {
            return Err(failed(&format!("the printer \"{printer}\" could not be prepared")));
        }
        job.dc = dc;
        let title = wide(&options.title);
        let info =
            DOCINFOW { cbSize: size_of::<DOCINFOW>() as i32, lpszDocName: title.as_ptr(), lpszOutput: null(), lpszDatatype: null(), fwType: 0 };
        // SAFETY: `dc` is the printer device context just created; `info` and the title it points
        // to outlive the call.
        if unsafe { StartDocW(dc, &info) } <= 0 {
            return Err(failed("the print job could not be started"));
        }
        job.started = true;
        Ok(job)
    }

    /// How many times the caller has to send the sheets: once when the driver makes the copies
    /// itself, otherwise once per copy.
    pub fn copies_to_send(&self) -> u32 {
        self.send
    }

    fn read_settings(&self) -> Result<Settings, Error> {
        // SAFETY: with mode 0 and no buffers the call only returns the size the settings need.
        let size = unsafe { DocumentPropertiesW(null_mut(), self.printer, self.name.as_ptr(), null_mut(), null(), 0) };
        if size < size_of::<DEVMODEW>() as i32 || size as u32 > MAX_SPOOLER_BYTES {
            return Err(failed("the printer's settings could not be read"));
        }
        // One spare word: some drivers write a little past what they asked for.
        let mut settings = Settings { words: vec![0u64; (size as usize).div_ceil(8) + 1] };
        // SAFETY: the buffer holds at least `size` bytes, the amount the driver said it writes.
        if unsafe { DocumentPropertiesW(null_mut(), self.printer, self.name.as_ptr(), settings.as_mut_ptr(), null(), DM_OUT_BUFFER) } != OK {
            return Err(failed("the printer's settings could not be read"));
        }
        Ok(settings)
    }

    /// Have the driver check changed settings and bring its private data in line with them.
    fn merge(&mut self) -> Result<(), Error> {
        let input = self.settings.words.clone();
        // SAFETY: both buffers have the size the driver asked for; the input is a settings block
        // it produced, with only public fields changed.
        let r = unsafe {
            DocumentPropertiesW(
                null_mut(),
                self.printer,
                self.name.as_ptr(),
                self.settings.as_mut_ptr(),
                input.as_ptr().cast(),
                DM_IN_BUFFER | DM_OUT_BUFFER,
            )
        };
        if r == OK { Ok(()) } else { Err(failed("the printer did not accept the print settings")) }
    }

    /// Describe `paper` in the settings. Returns whether anything changed.
    fn set_paper(&mut self, paper: Paper) -> bool {
        let request = paper_request(paper);
        if self.paper == Some(request) {
            return false;
        }
        let dm = self.settings.get_mut();
        // SAFETY: the union's printer half is plain integers, valid for every bit pattern.
        let mut fields = unsafe { dm.Anonymous1.Anonymous1 };
        fields.dmOrientation = if request.landscape { 2 } else { 1 };
        dm.dmFields |= DM_ORIENTATION | DM_PAPERSIZE;
        match request.code {
            Some(code) => {
                fields.dmPaperSize = code;
                dm.dmFields &= !(DM_PAPERLENGTH | DM_PAPERWIDTH);
            }
            None => {
                fields.dmPaperSize = PAPER_CUSTOM;
                (fields.dmPaperWidth, fields.dmPaperLength) = request.tenths_mm;
                dm.dmFields |= DM_PAPERLENGTH | DM_PAPERWIDTH;
            }
        }
        dm.Anonymous1.Anonymous1 = fields;
        self.paper = Some(request);
        true
    }

    fn apply(&mut self, options: &JobOptions, first: Paper) -> Result<(), Error> {
        let copies = options.copies.clamp(1, 9999);
        self.set_paper(first);
        {
            let dm = self.settings.get_mut();
            // SAFETY: the union's printer half is plain integers, valid for every bit pattern.
            let mut fields = unsafe { dm.Anonymous1.Anonymous1 };
            fields.dmCopies = copies as i16;
            dm.Anonymous1.Anonymous1 = fields;
            dm.dmCollate = if options.collate { DMCOLLATE_TRUE } else { DMCOLLATE_FALSE };
            dm.dmDuplex = match options.duplex {
                Duplex::Off => DMDUP_SIMPLEX,
                Duplex::LongEdge => DMDUP_VERTICAL,
                Duplex::ShortEdge => DMDUP_HORIZONTAL,
            };
            dm.dmColor = if options.grayscale { DMCOLOR_MONOCHROME } else { DMCOLOR_COLOR };
            dm.dmFields |= DM_COPIES | DM_COLLATE | DM_DUPLEX | DM_COLOR;
        }
        self.merge()?;
        let dm = self.settings.get();
        // SAFETY: as above.
        let accepted = unsafe { dm.Anonymous1.Anonymous1.dmCopies };
        // A driver that can't make copies says so by dropping the field or the number. Then the
        // sheets are sent once per copy instead (collated).
        if copies > 1 && (dm.dmFields & DM_COPIES == 0 || i64::from(accepted) != i64::from(copies)) {
            let dm = self.settings.get_mut();
            // SAFETY: as above.
            let mut fields = unsafe { dm.Anonymous1.Anonymous1 };
            fields.dmCopies = 1;
            dm.Anonymous1.Anonymous1 = fields;
            self.merge()?;
            self.send = copies;
        }
        Ok(())
    }

    fn cap(&self, index: u32) -> u32 {
        // SAFETY: `dc` is this job's live device context; the call only reads a capability.
        unsafe { GetDeviceCaps(self.dc, index as i32) }.max(0) as u32
    }

    fn area(&self) -> PageArea {
        let printable = (self.cap(HORZRES), self.cap(VERTRES));
        let (w, h) = (self.cap(PHYSICALWIDTH), self.cap(PHYSICALHEIGHT));
        PageArea {
            dpi: (self.cap(LOGPIXELSX).max(1), self.cap(LOGPIXELSY).max(1)),
            // Devices that are not printers report no physical sheet: it is the drawing area.
            paper: (if w > 0 { w } else { printable.0 }, if h > 0 { h } else { printable.1 }),
            offset: (self.cap(PHYSICALOFFSETX), self.cap(PHYSICALOFFSETY)),
            printable,
        }
    }

    /// Start the next sheet, of size `paper`; the printer is told when the size or orientation
    /// changes from the sheet before. Returns the sheet as the printer will feed it.
    pub fn start_page(&mut self, paper: Paper) -> Result<PageArea, Error> {
        if !self.started || self.in_page {
            return Err(Error("the print job is not ready for a new page".into()));
        }
        if self.set_paper(paper) && self.merge().is_ok() {
            // SAFETY: `dc` is live and between pages, where ResetDC is allowed; the settings were
            // just checked by the driver. A driver that refuses keeps the paper it had, and the
            // caller's placement then scales the sheet to fit.
            unsafe { ResetDCW(self.dc, self.settings.as_ptr()) };
        }
        // SAFETY: `dc` is a live printer device context with a started document.
        if unsafe { StartPage(self.dc) } <= 0 {
            return Err(failed("the printer did not start the page"));
        }
        self.in_page = true;
        Ok(self.area())
    }

    /// Draw a band of pixels on the current sheet. `bgr` is what [`crate::pack_bgr`] makes:
    /// `height` bottom-up rows of `width` pixels, three bytes each, padded to [`stride`]. It is
    /// scaled to `at`, given in device pixels from the sheet's top-left corner.
    pub fn draw(&mut self, bgr: &[u8], width: u32, height: u32, at: Place) -> Result<(), Error> {
        let bad = || Error("the page image could not be sent to the printer".into());
        if !self.in_page {
            return Err(Error("no page is open on the printer".into()));
        }
        let need = stride(width).checked_mul(height as usize).ok_or_else(bad)?;
        let int = |v: u32| i32::try_from(v).map_err(|_| bad());
        if width == 0 || height == 0 || bgr.len() < need {
            return Err(bad());
        }
        let (w, h, dw, dh) = (int(width)?, int(height)?, int(at.width)?, int(at.height)?);
        let size = u32::try_from(need).map_err(|_| bad())?;
        let area = self.area();
        // Device coordinates start at the printable area's corner, not the sheet's.
        let x = at.x.saturating_sub(int(area.offset.0)?);
        let y = at.y.saturating_sub(int(area.offset.1)?);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: h,
                biPlanes: 1,
                biBitCount: 24,
                biCompression: BI_RGB,
                biSizeImage: size,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: `dc` is live with a page open. `info` describes a `width` × `height` 24-bit
        // bottom-up bitmap, for which GDI reads stride × height bytes: `bgr` holds at least that
        // many (checked above). Both outlive the calls.
        let lines = unsafe {
            SetStretchBltMode(self.dc, if dw == w && dh == h { COLORONCOLOR } else { HALFTONE });
            // Required after choosing HALFTONE, harmless otherwise.
            SetBrushOrgEx(self.dc, 0, 0, null_mut());
            StretchDIBits(self.dc, x, y, dw, dh, 0, 0, w, h, bgr.as_ptr().cast(), &info, DIB_RGB_COLORS, SRCCOPY)
        };
        if lines <= 0 { Err(failed("the printer did not take the page image")) } else { Ok(()) }
    }

    pub fn end_page(&mut self) -> Result<(), Error> {
        if !self.in_page {
            return Ok(());
        }
        self.in_page = false;
        // SAFETY: `dc` is live and a page is open on it.
        if unsafe { EndPage(self.dc) } <= 0 { Err(failed("the printer did not finish the page")) } else { Ok(()) }
    }

    /// Hand the job to the spooler for printing.
    pub fn finish(mut self) -> Result<(), Error> {
        self.end_page()?;
        // Whatever EndDoc says, the document is over: Drop must not cancel it again.
        self.started = false;
        // SAFETY: `dc` is live with a started document and no open page.
        if unsafe { EndDoc(self.dc) } <= 0 { Err(failed("the print job could not be completed")) } else { Ok(()) }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: each handle is closed once, here, and only if it was opened: `dc` and
        // `printer` are null until their creation succeeds. A job that was started and not
        // finished is cancelled first, so no half-printed document reaches the printer.
        unsafe {
            if !self.dc.is_null() {
                if self.started {
                    AbortDoc(self.dc);
                }
                DeleteDC(self.dc);
            }
            if !self.printer.Value.is_null() {
                ClosePrinter(self.printer);
            }
        }
    }
}
