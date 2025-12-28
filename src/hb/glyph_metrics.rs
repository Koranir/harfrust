use crate::hb::face::{hb_glyph_extents_t, FontFuncs};
use crate::{hb::tables::TableRanges, Tag};
use read_fonts::{
    tables::{
        glyf::Glyf,
        gvar::Gvar,
        hmtx::{Hmtx, LongMetric},
        hvar::Hvar,
        loca::Loca,
        mvar::Mvar,
        vmtx::Vmtx,
        vorg::Vorg,
        vvar::Vvar,
    },
    types::{BoundingBox, F2Dot14, Fixed, GlyphId, Point},
    FontRef,
};

#[derive(Clone)]
pub(crate) struct GlyphMetrics<'a> {
    _hmtx: Option<Hmtx<'a>>,
    h_metrics: &'a [LongMetric],
    hvar: Option<Hvar<'a>>,
    vmtx: Option<Vmtx<'a>>,
    vvar: Option<Vvar<'a>>,
    vorg: Option<Vorg<'a>>,
    glyf: Option<GlyfTables<'a>>,
    mvar: Option<Mvar<'a>>,
    num_glyphs: u32,
    upem: u16,
    ascent: i16,
    descent: i16,
}

#[derive(Clone)]
struct GlyfTables<'a> {
    loca: Loca<'a>,
    glyf: Glyf<'a>,
    gvar: Option<Gvar<'a>>,
}

impl<'a> GlyphMetrics<'a> {
    pub fn new(font: &FontRef<'a>, table_ranges: &TableRanges) -> Self {
        let num_glyphs = table_ranges.num_glyphs;
        let upem = table_ranges.units_per_em;
        let hmtx = table_ranges
            .hmtx
            .resolve_data(font)
            .and_then(|data| Hmtx::read(data, table_ranges.num_h_metrics).ok());
        let h_metrics = hmtx
            .as_ref()
            .map(|hmtx| hmtx.h_metrics())
            .unwrap_or_default();
        let hvar = table_ranges.hvar.resolve_table(font);
        let vmtx = table_ranges
            .vmtx
            .resolve_data(font)
            .and_then(|data| Vmtx::read(data, table_ranges.num_v_metrics).ok());
        let vvar = table_ranges.vvar.resolve_table(font);
        let vorg = table_ranges.vorg.resolve_table(font);
        let loca = table_ranges
            .loca
            .resolve_data(font)
            .and_then(|data| Loca::read(data, table_ranges.loca_long).ok());
        let glyf = table_ranges.glyf.resolve_table(font);
        let glyf = if let Some((loca, glyf)) = loca.zip(glyf) {
            let gvar = table_ranges.gvar.resolve_table(font);
            Some(GlyfTables { loca, glyf, gvar })
        } else {
            None
        };
        let mvar = table_ranges.mvar.resolve_table(font);
        let ascent = table_ranges.ascent;
        let descent = table_ranges.descent;
        Self {
            _hmtx: hmtx,
            h_metrics,
            hvar,
            vmtx,
            vvar,
            vorg,
            glyf,
            mvar,
            num_glyphs,
            upem,
            ascent,
            descent,
        }
    }

    pub fn _left_side_bearing(&self, gid: impl Into<GlyphId>, coords: &[F2Dot14]) -> Option<i32> {
        let gid = gid.into();
        let mut bearing = if let Some(hmtx) = self._hmtx.as_ref() {
            hmtx.side_bearing(gid).unwrap_or_default() as i32
        } else if let Some(extents) = self.extents(gid, coords) {
            return Some(extents.x_min);
        } else {
            return None;
        };
        if !coords.is_empty() {
            if let Some(hvar) = self.hvar.as_ref() {
                bearing += hvar.lsb_delta(gid, coords).unwrap_or_default().to_i32();
            } else if let Some(deltas) = self.phantom_deltas(gid, coords) {
                bearing += deltas[0].x.to_i32();
            }
        }
        Some(bearing)
    }

    pub fn top_side_bearing(&self, gid: impl Into<GlyphId>, coords: &[F2Dot14]) -> Option<i32> {
        let gid = gid.into();
        let mut bearing = if let Some(vmtx) = self.vmtx.as_ref() {
            vmtx.side_bearing(gid).unwrap_or_default() as i32
        } else {
            return None;
        };
        if !coords.is_empty() {
            if let Some(vvar) = self.vvar.as_ref() {
                bearing += vvar.tsb_delta(gid, coords).unwrap_or_default().to_i32();
            } else if let Some(deltas) = self.phantom_deltas(gid, coords) {
                bearing += deltas[3].y.to_i32();
            }
        }
        Some(bearing)
    }

    pub fn extents(&self, gid: impl Into<GlyphId>, coords: &[F2Dot14]) -> Option<BoundingBox<i32>> {
        let gid = gid.into();
        let glyf = self.glyf.as_ref()?;
        let glyph = glyf.loca.get_glyf(gid, &glyf.glyf).ok()?;
        let Some(glyph) = glyph else {
            // Return empty extents for empty glyph
            return Some(BoundingBox::default());
        };
        if !coords.is_empty() {
            return None; // TODO https://github.com/harfbuzz/harfrust/pull/52#issuecomment-2878117808
        }
        Some(BoundingBox {
            x_min: glyph.x_min() as i32,
            x_max: glyph.x_max() as i32,
            y_min: glyph.y_min() as i32,
            y_max: glyph.y_max() as i32,
        })
    }

    fn phantom_deltas(&self, gid: GlyphId, coords: &[F2Dot14]) -> Option<[Point<Fixed>; 4]> {
        let glyf = self.glyf.as_ref()?;
        let gvar = glyf.gvar.as_ref()?;
        gvar.phantom_point_deltas(&glyf.glyf, &glyf.loca, coords, gid)
            .ok()?
    }
}

/// A default set of font functions, using the builtin glyph metrics tables.
///
/// If you use hinting or any other process that modifies glyph metrics,
/// you should create your own [`FontFuncs`](crate::FontFuncs) implementation instead of using this struct,
/// otherwise the rendered and shaped metrics will not be in sync.
//
// `hb_ot_font_funcs`, essentially.
pub struct OtFontFuncs<'a>(GlyphMetrics<'a>);
impl<'a> OtFontFuncs<'a> {
    /// Create a default font function set.
    pub fn new(font: &FontRef<'a>, shaper_data: &crate::ShaperData) -> Self {
        Self(GlyphMetrics::new(font, &shaper_data.table_ranges))
    }
}
impl FontFuncs for OtFontFuncs<'_> {
    fn glyph_h_advances(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        advance: &mut [crate::GlyphPosition],
    ) {
        for (info, pos) in glyph.iter().zip(advance.iter_mut()) {
            pos.x_advance = self
                .0
                .h_metrics
                .get(info.glyph_id as usize)
                .or_else(|| self.0.h_metrics.last())
                .map(|metric| metric.advance() as i32)
                .or_else(|| (info.glyph_id < self.0.num_glyphs).then_some(self.0.upem as i32 / 2))
                .unwrap_or_default();
        }
        if !font.coords().is_empty() {
            if let Some(hvar) = self.0.hvar.as_ref() {
                for (info, pos) in glyph.iter().zip(advance.iter_mut()) {
                    pos.x_advance += hvar
                        .advance_width_delta(info.as_glyph(), font.coords())
                        .unwrap_or_default()
                        .to_i32();
                }
            } else {
                for (info, pos) in glyph.iter().zip(advance.iter_mut()) {
                    if let Some(deltas) = self.0.phantom_deltas(info.as_glyph(), font.coords()) {
                        pos.x_advance += deltas[1].x.to_i32() - deltas[0].x.to_i32();
                    }
                }
            }
        }
    }

    fn glyph_v_advances(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        advance: &mut [crate::GlyphPosition],
    ) {
        for (info, pos) in glyph.iter().zip(advance.iter_mut()) {
            pos.y_advance = -self
                .0
                .vmtx
                .as_ref()
                .and_then(|vmtx| vmtx.advance(info.as_glyph()))
                .map_or_else(
                    || self.0.ascent as i32 - self.0.descent as i32,
                    |advance| advance as i32,
                );
        }
        if !font.coords().is_empty() {
            if let Some(vvar) = self.0.vvar.as_ref() {
                for (info, pos) in glyph.iter().zip(advance.iter_mut()) {
                    pos.y_advance -= vvar
                        .advance_height_delta(info.as_glyph(), font.coords())
                        .unwrap_or_default()
                        .to_i32();
                }
            } else {
                for (info, pos) in glyph.iter().zip(advance.iter_mut()) {
                    if let Some(deltas) = self.0.phantom_deltas(info.as_glyph(), font.coords()) {
                        pos.y_advance -= deltas[3].y.to_i32() - deltas[2].y.to_i32();
                    }
                }
            }
        }
    }

    fn glyph_h_origins(
        &self,
        _font: &crate::Shaper,
        _glyph: &[crate::GlyphInfo],
        origin: &mut [crate::GlyphPosition],
    ) -> Result<(), ()> {
        // Horizontal origins default to (0, 0)
        for origin in origin {
            origin.x_offset = 0;
            origin.y_offset = 0;
        }

        Ok(())
    }

    fn non_default_h_origins(&self, _font: &crate::Shaper) -> bool {
        false
    }

    fn glyph_v_origins(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        origin: &mut [crate::GlyphPosition],
    ) -> Result<(), ()> {
        for (info, origin) in glyph.iter().zip(origin.iter_mut()) {
            origin.x_offset = self.glyph_h_advance(font, info.as_glyph()) / 2;
        }

        if let Some(vorg) = self.0.vorg.as_ref() {
            for (info, origin) in glyph.iter().zip(origin.iter_mut()) {
                origin.y_offset = vorg.vertical_origin_y(info.as_glyph()) as i32;
            }

            if !font.coords().is_empty() {
                if let Some(vvar) = self.0.vvar.as_ref() {
                    for (info, origin) in glyph.iter().zip(origin.iter_mut()) {
                        origin.y_offset += vvar
                            .v_org_delta(info.as_glyph(), font.coords())
                            .unwrap_or_default()
                            .to_i32();
                    }
                }
            }

            return Ok(());
        }

        for (info, origin) in glyph.iter().zip(origin.iter_mut()) {
            if let Some(extents) = self.0.extents(info.as_glyph(), font.coords()) {
                origin.y_offset = {
                    let origin = if self.0.vmtx.is_some() {
                        let mut origin = Some(extents.y_max);
                        let tsb = self.0.top_side_bearing(info.as_glyph(), font.coords());
                        if let Some(tsb) = tsb {
                            origin = Some(origin.unwrap() + tsb);
                        } else {
                            origin = None;
                        }
                        if origin.is_some() && !font.coords().is_empty() {
                            if let Some(vvar) = self.0.vvar.as_ref() {
                                origin = Some(
                                    origin.unwrap()
                                        + vvar
                                            .v_org_delta(info.as_glyph(), font.coords())
                                            .unwrap_or_default()
                                            .to_i32(),
                                );
                            }
                        }
                        origin
                    } else {
                        None
                    };

                    if let Some(origin) = origin {
                        origin
                    } else {
                        let mut advance = self.0.ascent as i32 - self.0.descent as i32;
                        if let Some(mvar) = self.0.mvar.as_ref() {
                            advance += mvar
                                .metric_delta(Tag::new(b"hasc"), font.coords())
                                .unwrap_or_default()
                                .to_i32()
                                - mvar
                                    .metric_delta(Tag::new(b"hdsc"), font.coords())
                                    .unwrap_or_default()
                                    .to_i32();
                        }
                        let diff = advance - (extents.y_max - extents.y_min);
                        extents.y_max + (diff >> 1)
                    }
                }
            } else {
                origin.y_offset = self.0.ascent as i32;
                if let Some(mvar) = self.0.mvar.as_ref() {
                    origin.y_offset += mvar
                        .metric_delta(Tag::new(b"hasc"), font.coords())
                        .unwrap_or_default()
                        .to_i32();
                }
            }
        }

        Ok(())
    }

    fn glyph_extents(&self, font: &crate::Shaper, glyph: GlyphId) -> Option<hb_glyph_extents_t> {
        self.0
            .extents(glyph, font.coords())
            .map(|e| hb_glyph_extents_t {
                x_bearing: e.x_min,
                y_bearing: e.y_max,
                width: e.x_max - e.x_min,
                height: e.y_min - e.y_max,
            })
    }
}
