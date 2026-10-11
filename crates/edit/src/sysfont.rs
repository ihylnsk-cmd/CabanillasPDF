//! A stand-in font taken from the fonts installed on this computer.
//!
//! When replacement text needs a character the document's own (subset) font lacks, the edit
//! falls back to a standard PDF font, so one line of a Cambria document turns into Helvetica.
//! With system fonts switched on ([`use_system_fonts`]), the same family (or its
//! metric-compatible twin: Arial for Helvetica, Carlito for Calibri, …) is looked up among the
//! installed fonts first. The glyphs the text needs are subset and embedded as a Type 0 font
//! (CIDFontType2, Identity-H) with a `/ToUnicode` map, so the new text keeps the document's
//! typeface, prints the same everywhere, and stays searchable and editable.
//!
//! Off by default: a library call gives the same result on every machine. The desktop app and
//! the command-line tool turn it on when they start. The web build has no system fonts.

use std::sync::atomic::{AtomicBool, Ordering};

use pdfcraft_cos::{Dict, Document};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Let edits of existing text use the fonts installed on this computer when the document's own
/// font can't show the new text. Process-wide; off until an application turns it on.
pub fn use_system_fonts(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether [`use_system_fonts`] is on.
pub fn system_fonts_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// What the stand-in should look like: the source font's name and style.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Style<'a> {
    pub base_font: &'a str,
    pub bold: bool,
    pub italic: bool,
    pub serif: bool,
    pub mono: bool,
}

/// An embedded subset of an installed font, ready to show one piece of text.
#[derive(Clone, Debug)]
pub(crate) struct SystemFont {
    /// Its name in the page's `/Font` resources.
    pub name: String,
    /// The family it came from ("Caladea").
    pub family: String,
    /// Each character's glyph in the subset and its advance in ems.
    glyphs: std::collections::HashMap<char, (u16, f64)>,
}

/// The character a font is asked for: tabs and no-break spaces are drawn as spaces.
fn drawn(c: char) -> char {
    if c == '\t' || c == '\u{a0}' { ' ' } else { c }
}

impl SystemFont {
    /// The bytes that show `s` (two per character), when every character is in the subset.
    pub fn encode(&self, s: &str) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(s.len().saturating_mul(2));
        for c in s.chars() {
            let (gid, _) = self.glyphs.get(&drawn(c))?;
            out.extend_from_slice(&gid.to_be_bytes());
        }
        Some(out)
    }

    /// The width of `s` in ems (characters outside the subset count as nothing).
    pub fn advance(&self, s: &str) -> f64 {
        s.chars().filter_map(|c| self.glyphs.get(&drawn(c))).map(|(_, w)| *w).sum()
    }
}

/// Embed a font from this computer that can show `text` in `style` and register it in
/// `fonts_res`. With `generic` false only the source font's own family or its metric twin is
/// accepted; with it true any installed face of the same kind (serif, sans, mono) that has every
/// character will do. `None` when system fonts are off, nothing suitable is installed, or on the
/// web build.
pub(crate) fn embed(doc: &mut Document, fonts_res: &mut Dict, text: &str, style: &Style, generic: bool) -> Option<SystemFont> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        native::embed(doc, fonts_res, text, style, generic)
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (doc, fonts_res, text, style, generic);
        None
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::{Arc, OnceLock};

    use pdfcraft_cos::{Dict, Document, ObjRef, Object, PdfString, Stream};

    use super::{Style, SystemFont, drawn, system_fonts_enabled};

    /// The most distinct characters one stand-in carries (a paragraph, not a book).
    const MAX_GLYPHS: usize = 4096;
    /// The largest font file read (CJK collections are tens of megabytes).
    const MAX_FONT_BYTES: usize = 64 << 20;

    thread_local! {
        /// Tests install their own fonts here, so they don't depend on the machine.
        static OVERRIDE: RefCell<Option<Arc<fontdb::Database>>> = const { RefCell::new(None) };
    }

    /// Run `f` with exactly these font files installed (and system fonts on) for this thread.
    #[cfg(test)]
    pub(crate) fn with_fonts<T>(files: &[&[u8]], f: impl FnOnce() -> T) -> T {
        let mut db = fontdb::Database::new();
        for bytes in files {
            db.load_font_data(bytes.to_vec());
        }
        OVERRIDE.with(|o| *o.borrow_mut() = Some(Arc::new(db)));
        let out = f();
        OVERRIDE.with(|o| *o.borrow_mut() = None);
        out
    }

    fn database() -> Option<Arc<fontdb::Database>> {
        if let Some(db) = OVERRIDE.with(|o| o.borrow().clone()) {
            return Some(db);
        }
        if !system_fonts_enabled() {
            return None;
        }
        static SYSTEM: OnceLock<Arc<fontdb::Database>> = OnceLock::new();
        Some(
            SYSTEM
                .get_or_init(|| {
                    let mut db = fontdb::Database::new();
                    db.load_system_fonts();
                    Arc::new(db)
                })
                .clone(),
        )
    }

    /// "ABCDEF+Calibri-Bold" → "Calibri-Bold".
    pub(crate) fn without_subset_tag(name: &str) -> &str {
        match name.split_once('+') {
            Some((tag, rest)) if tag.len() == 6 && tag.bytes().all(|b| b.is_ascii_uppercase()) => rest,
            _ => name,
        }
    }

    fn norm(s: &str) -> String {
        s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
    }

    /// The family part of a PostScript-style font name, normalised: "TimesNewRomanPS-BoldMT" →
    /// "timesnewroman", "Calibri,Bold" → "calibri".
    pub(crate) fn family_key(name: &str) -> String {
        let name = without_subset_tag(name);
        let base = name.split(['-', ',']).next().unwrap_or(name);
        let mut k = norm(base);
        for style in ["bolditalic", "boldoblique", "bold", "italic", "oblique", "regular"] {
            if k.len() > style.len() + 2 && k.ends_with(style) {
                k.truncate(k.len() - style.len());
            }
        }
        for suffix in ["psmt", "mt", "ps"] {
            if k.len() > suffix.len() + 2 && k.ends_with(suffix) {
                k.truncate(k.len() - suffix.len());
                break;
            }
        }
        k
    }

    /// Families that share the source family's metrics, the closest first: text set in one fits
    /// the space of the other.
    fn twins(key: &str) -> &'static [&'static str] {
        match key {
            "helvetica" | "helveticaneue" | "arial" | "nimbussans" | "liberationsans" | "arimo" => {
                &["Arial", "Helvetica", "Liberation Sans", "Arimo", "Nimbus Sans"]
            }
            "times" | "timesroman" | "timesnewroman" | "nimbusroman" | "liberationserif" | "tinos" => {
                &["Times New Roman", "Times", "Liberation Serif", "Tinos", "Nimbus Roman"]
            }
            "courier" | "couriernew" | "nimbusmono" | "liberationmono" | "cousine" => {
                &["Courier New", "Courier", "Liberation Mono", "Cousine", "Nimbus Mono PS"]
            }
            "calibri" | "carlito" => &["Calibri", "Carlito"],
            "cambria" | "caladea" => &["Cambria", "Caladea"],
            _ => &[],
        }
    }

    fn kind_families(style: &Style) -> &'static [&'static str] {
        if style.mono {
            &["Courier New", "Consolas", "Liberation Mono", "DejaVu Sans Mono", "Noto Sans Mono"]
        } else if style.serif {
            &["Times New Roman", "Liberation Serif", "Georgia", "DejaVu Serif", "Noto Serif"]
        } else {
            &["Arial", "Helvetica", "Liberation Sans", "Segoe UI", "DejaVu Sans", "Noto Sans"]
        }
    }

    /// How far a face is from the wanted weight and slant (0 is a perfect match).
    fn distance(face: &fontdb::FaceInfo, style: &Style) -> i32 {
        let want: i32 = if style.bold { 700 } else { 400 };
        let mut d = (i32::from(face.weight.0) - want).abs();
        if (face.style != fontdb::Style::Normal) != style.italic {
            d += 2000;
        }
        if face.stretch != fontdb::Stretch::Normal {
            d += 500;
        }
        d
    }

    fn family_faces<'a>(db: &'a fontdb::Database, family: &str, style: &Style) -> Vec<&'a fontdb::FaceInfo> {
        let key = norm(family);
        let mut v: Vec<&fontdb::FaceInfo> = db.faces().filter(|f| f.families.iter().any(|(n, _)| norm(n) == key)).collect();
        v.sort_by_key(|f| distance(f, style));
        v
    }

    /// The faces to try, best first: the source font itself, its family, its metric twins and,
    /// with `generic`, the usual faces of its kind and then everything installed.
    fn candidates<'a>(db: &'a fontdb::Database, style: &Style, generic: bool) -> Vec<&'a fontdb::FaceInfo> {
        let mut out: Vec<&fontdb::FaceInfo> = Vec::new();
        let push = |f: &'a fontdb::FaceInfo, out: &mut Vec<&'a fontdb::FaceInfo>| {
            if !out.iter().any(|o| o.id == f.id) {
                out.push(f);
            }
        };
        let wanted = without_subset_tag(style.base_font);
        let ps = norm(wanted);
        if !ps.is_empty() {
            for f in db.faces().filter(|f| norm(&f.post_script_name) == ps) {
                push(f, &mut out);
            }
        }
        let key = family_key(wanted);
        if key.len() >= 3 {
            let mut same: Vec<&fontdb::FaceInfo> = db.faces().filter(|f| f.families.iter().any(|(n, _)| norm(n) == key)).collect();
            same.sort_by_key(|f| distance(f, style));
            for f in same {
                push(f, &mut out);
            }
            for family in twins(&key) {
                if let Some(f) = family_faces(db, family, style).first() {
                    push(f, &mut out);
                }
            }
        }
        if generic {
            for family in kind_families(style) {
                if let Some(f) = family_faces(db, family, style).first() {
                    push(f, &mut out);
                }
            }
            let mut rest: Vec<&fontdb::FaceInfo> = db.faces().collect();
            rest.sort_by_key(|f| distance(f, style));
            for f in rest {
                push(f, &mut out);
            }
        }
        out
    }

    /// Six capitals derived from the subset, so the same edit always writes the same name.
    fn subset_tag(seed: &[u8]) -> String {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in seed {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        (0..6u32).map(|i| char::from(b'A' + ((h >> (i * 5)) % 26) as u8)).collect()
    }

    struct Embedded {
        font: ObjRef,
        family: String,
        glyphs: HashMap<char, (u16, f64)>,
    }

    /// Subset `data` to the characters of `text` and write the font objects. `None` when the face
    /// lacks a character, has no TrueType outlines (CFF faces need a different font type) or
    /// can't be read.
    fn embed_face(doc: &mut Document, data: &[u8], index: u32, text: &str) -> Option<Embedded> {
        if data.len() > MAX_FONT_BYTES {
            return None;
        }
        let face = ttf_parser::Face::parse(data, index).ok()?;
        face.tables().glyf?;
        let upem = f64::from(face.units_per_em());
        if upem <= 0.0 {
            return None;
        }
        let mut remapper = subsetter::GlyphRemapper::new();
        let mut glyphs: HashMap<char, (u16, f64)> = HashMap::new();
        // Subset glyph → the character it shows and its width in thousandths of an em.
        let mut by_glyph: BTreeMap<u16, (char, f64)> = BTreeMap::new();
        for ch in text.chars().map(drawn) {
            if glyphs.contains_key(&ch) {
                continue;
            }
            if ch.is_control() || glyphs.len() >= MAX_GLYPHS {
                return None;
            }
            let gid = face.glyph_index(ch)?;
            let new = remapper.remap(gid.0);
            let width = f64::from(face.glyph_hor_advance(gid).unwrap_or(0)) / upem;
            glyphs.insert(ch, (new, width));
            by_glyph.entry(new).or_insert((ch, width * 1000.0));
        }
        if glyphs.is_empty() {
            return None;
        }
        let subset = subsetter::subset(data, index, &remapper).ok()?;

        let name_of = |id: u16| face.names().into_iter().find(|n| n.name_id == id && n.is_unicode()).and_then(|n| n.to_string());
        let ps_name: String =
            name_of(ttf_parser::name_id::POST_SCRIPT_NAME).unwrap_or_default().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        let ps_name = if ps_name.is_empty() { "Font".to_string() } else { ps_name };
        let family = name_of(ttf_parser::name_id::FAMILY).unwrap_or_else(|| ps_name.clone());
        let mut seed = subset.clone();
        seed.extend_from_slice(text.as_bytes());
        let tagged = format!("{}+{}", subset_tag(&seed), ps_name);

        let scale = 1000.0 / upem;
        let units = |v: i16| Object::Real((f64::from(v) * scale).round());
        let bbox = face.global_bounding_box();
        // Symbolic (the glyphs are addressed by number), plus what a reader uses to pick a
        // substitute and what the editor reads the style back from.
        let mut flags = 4;
        if face.is_monospaced() {
            flags |= 1;
        }
        if face.is_italic() {
            flags |= 64;
        }
        if face.is_bold() {
            flags |= 1 << 18;
        }

        let mut file = Dict::new();
        file.set(b"Length1".to_vec(), i64::try_from(subset.len()).ok()?);
        let file = doc.add(Object::Stream(Stream::flate(file, &subset)));

        let mut descriptor = Dict::new();
        descriptor.set(b"Type".to_vec(), Object::name("FontDescriptor"));
        descriptor.set(b"FontName".to_vec(), Object::name(&tagged));
        descriptor.set(b"FontFamily".to_vec(), Object::String(PdfString::text(&family)));
        descriptor.set(b"Flags".to_vec(), Object::Int(flags));
        descriptor.set(b"FontBBox".to_vec(), Object::Array(vec![units(bbox.x_min), units(bbox.y_min), units(bbox.x_max), units(bbox.y_max)]));
        descriptor.set(b"ItalicAngle".to_vec(), Object::Real(f64::from(face.italic_angle())));
        descriptor.set(b"Ascent".to_vec(), units(face.ascender()));
        descriptor.set(b"Descent".to_vec(), units(face.descender()));
        descriptor.set(b"CapHeight".to_vec(), units(face.capital_height().unwrap_or_else(|| face.ascender())));
        descriptor.set(b"StemV".to_vec(), Object::Int(if face.is_bold() { 120 } else { 80 }));
        descriptor.set(b"FontWeight".to_vec(), Object::Int(i64::from(face.weight().to_number())));
        descriptor.set(b"FontFile2".to_vec(), Object::Ref(file));
        let descriptor = doc.add(Object::Dict(descriptor));

        let mut widths = Vec::with_capacity(by_glyph.len() * 2);
        let mut cmap = String::from(
            "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n\
             /CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
        );
        let pairs: Vec<(u16, char, f64)> = by_glyph.iter().map(|(gid, (ch, w))| (*gid, *ch, *w)).collect();
        for chunk in pairs.chunks(100) {
            cmap.push_str(&format!("{} beginbfchar\n", chunk.len()));
            for (gid, ch, width) in chunk {
                widths.push(Object::Int(i64::from(*gid)));
                widths.push(Object::Array(vec![Object::Real(width.round())]));
                let mut buf = [0u16; 2];
                let hex: String = ch.encode_utf16(&mut buf).iter().map(|u| format!("{u:04X}")).collect();
                cmap.push_str(&format!("<{gid:04X}> <{hex}>\n"));
            }
            cmap.push_str("endbfchar\n");
        }
        cmap.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
        let to_unicode = doc.add(Object::Stream(Stream::flate(Dict::new(), cmap.as_bytes())));

        let mut system_info = Dict::new();
        system_info.set(b"Registry".to_vec(), Object::String(PdfString::literal(b"Adobe".to_vec())));
        system_info.set(b"Ordering".to_vec(), Object::String(PdfString::literal(b"Identity".to_vec())));
        system_info.set(b"Supplement".to_vec(), Object::Int(0));
        let mut cid_font = Dict::new();
        cid_font.set(b"Type".to_vec(), Object::name("Font"));
        cid_font.set(b"Subtype".to_vec(), Object::name("CIDFontType2"));
        cid_font.set(b"BaseFont".to_vec(), Object::name(&tagged));
        cid_font.set(b"CIDSystemInfo".to_vec(), Object::Dict(system_info));
        cid_font.set(b"FontDescriptor".to_vec(), Object::Ref(descriptor));
        cid_font.set(b"DW".to_vec(), Object::Int(1000));
        cid_font.set(b"W".to_vec(), Object::Array(widths));
        cid_font.set(b"CIDToGIDMap".to_vec(), Object::name("Identity"));
        let cid_font = doc.add(Object::Dict(cid_font));

        let mut font = Dict::new();
        font.set(b"Type".to_vec(), Object::name("Font"));
        font.set(b"Subtype".to_vec(), Object::name("Type0"));
        font.set(b"BaseFont".to_vec(), Object::name(&tagged));
        font.set(b"Encoding".to_vec(), Object::name("Identity-H"));
        font.set(b"DescendantFonts".to_vec(), Object::Array(vec![Object::Ref(cid_font)]));
        font.set(b"ToUnicode".to_vec(), Object::Ref(to_unicode));
        Some(Embedded { font: doc.add(Object::Dict(font)), family, glyphs })
    }

    pub(crate) fn embed(doc: &mut Document, fonts_res: &mut Dict, text: &str, style: &Style, generic: bool) -> Option<SystemFont> {
        let db = database()?;
        // Look before writing: a face that lacks a character costs a cmap lookup, not objects.
        let face = candidates(&db, style, generic).into_iter().find(|f| {
            db.with_face_data(f.id, |data, index| {
                ttf_parser::Face::parse(data, index).is_ok_and(|face| {
                    face.tables().glyf.is_some() && text.chars().map(drawn).all(|c| !c.is_control() && face.glyph_index(c).is_some())
                })
            })
            .unwrap_or(false)
        })?;
        let embedded = db.with_face_data(face.id, |data, index| embed_face(doc, data, index, text)).flatten()?;
        let mut name = String::from("PCSy");
        let mut suffix = 0usize;
        while fonts_res.contains(name.as_bytes()) {
            suffix = suffix.saturating_add(1);
            name = format!("PCSy{suffix}");
        }
        fonts_res.set(name.as_bytes().to_vec(), Object::Ref(embedded.font));
        Some(SystemFont { name, family: embedded.family, glyphs: embedded.glyphs })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn family_keys_and_subset_tags() {
            assert_eq!(family_key("ABCDEF+TimesNewRomanPS-BoldMT"), "timesnewroman");
            assert_eq!(family_key("ArialMT"), "arial");
            assert_eq!(family_key("Calibri,Bold"), "calibri");
            assert_eq!(family_key("CourierNewPSMT"), "couriernew");
            assert_eq!(family_key("CAAAAA+Caladea-Regular"), "caladea");
            assert_eq!(without_subset_tag("ABCDEF+Calibri"), "Calibri");
            assert_eq!(without_subset_tag("Calibri"), "Calibri");
            assert_eq!(without_subset_tag("A+B"), "A+B");
            assert_eq!(subset_tag(b"abc").len(), 6);
            assert_eq!(subset_tag(b"abc"), subset_tag(b"abc"));
            assert!(twins("calibri").contains(&"Carlito") && twins("caladea").contains(&"Cambria") && twins("inter").is_empty());
        }
    }
}
