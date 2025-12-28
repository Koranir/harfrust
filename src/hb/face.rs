use read_fonts::types::{F2Dot14, Fixed, GlyphId};
use read_fonts::{FontRef, TableProvider};
use smallvec::SmallVec;

use super::aat::AatTables;
use super::charmap::{cache_t as cmap_cache_t, Charmap};
use super::glyph_names::GlyphNames;
use super::ot::{LayoutTable, OtCache, OtTables};
use super::ot_layout::TableIndex;
use super::ot_shape::{hb_ot_shape_context_t, shape_internal};
use crate::hb::aat::AatCache;
use crate::hb::glyph_metrics::OtFontFuncs;
use crate::hb::tables::TableRanges;
use crate::{script, Feature, GlyphBuffer, NormalizedCoord, ShapePlan, UnicodeBuffer, Variation};

/// Data required for shaping with a single font.
pub struct ShaperData {
    pub(crate) table_ranges: TableRanges,
    ot_cache: OtCache,
    aat_cache: AatCache,
    cmap_cache: cmap_cache_t,
}

impl ShaperData {
    /// Creates new cached shaper data for the given font.
    pub fn new(font: &FontRef) -> Self {
        let ot_cache = OtCache::new(font);
        let aat_cache = AatCache::new(font);
        let table_ranges = TableRanges::new(font);
        let cmap_cache = cmap_cache_t::new();
        Self {
            table_ranges,
            ot_cache,
            aat_cache,
            cmap_cache,
        }
    }

    /// Returns a builder for constructing a new shaper with the given
    /// font.
    pub fn shaper<'a>(&'a self, font: &FontRef<'a>) -> ShaperBuilder<'a> {
        ShaperBuilder {
            data: self,
            font: font.clone(),
            instance: None,
            point_size: None,
            class: None,
        }
    }
}

// Maximum number of coordinates to store inline before spilling to the
// heap.
//
// Any value between 5 and 11 yields a SmallVec footprint of 32 bytes.
const MAX_INLINE_COORDS: usize = 11;

/// An instance of a variable font.
#[derive(Clone, Default, Debug)]
pub struct ShaperInstance {
    coords: SmallVec<[F2Dot14; MAX_INLINE_COORDS]>,
    pub(crate) feature_variations: [Option<u32>; 2],
    // TODO: this is a good place to hang variation specific caches
}

impl ShaperInstance {
    /// Creates a new shaper instance for the given font from the specified
    /// list of variation settings.
    ///
    /// The setting values are in user space and the order is insignificant.
    pub fn from_variations<V>(font: &FontRef, variations: V) -> Self
    where
        V: IntoIterator,
        V::Item: Into<Variation>,
    {
        let mut this = Self::default();
        this.set_variations(font, variations);
        this
    }

    /// Creates a new shaper instance for the given font from the specified
    /// set of normalized coordinates.
    ///
    /// The sequence of coordinates is expected to be in axis order.
    pub fn from_coords(font: &FontRef, coords: impl IntoIterator<Item = NormalizedCoord>) -> Self {
        let mut this = Self::default();
        this.set_coords(font, coords);
        this
    }

    /// Creates a new shaper instance for the given font using the variation
    /// position from the named instance at the specified index.
    pub fn from_named_instance(font: &FontRef, index: usize) -> Self {
        let mut this = Self::default();
        this.set_named_instance(font, index);
        this
    }

    /// Returns the underlying set of normalized coordinates.
    pub fn coords(&self) -> &[F2Dot14] {
        &self.coords
    }

    /// Resets the instance for the given font and variation settings.
    pub fn set_variations<V>(&mut self, font: &FontRef, variations: V)
    where
        V: IntoIterator,
        V::Item: Into<Variation>,
    {
        self.coords.clear();
        if let Ok(fvar) = font.fvar() {
            self.coords
                .resize(fvar.axis_count() as usize, F2Dot14::ZERO);
            fvar.user_to_normalized(
                font.avar().ok().as_ref(),
                variations
                    .into_iter()
                    .map(Into::into)
                    .map(|var| (var.tag, Fixed::from_f64(var.value as _))),
                self.coords.as_mut_slice(),
            );
            self.check_default();
            self.set_feature_variations(font);
        }
    }

    /// Resets the instance for the given font and normalized coordinates.
    pub fn set_coords(&mut self, font: &FontRef, coords: impl IntoIterator<Item = F2Dot14>) {
        self.coords.clear();
        if let Ok(fvar) = font.fvar() {
            let count = fvar.axis_count() as usize;
            self.coords.reserve(count);
            self.coords.extend(coords.into_iter().take(count));
            self.check_default();
            self.set_feature_variations(font);
        }
    }

    /// Resets the instance for the given font using the variation
    /// position from the named instance at the specified index.
    pub fn set_named_instance(&mut self, font: &FontRef, index: usize) {
        self.coords.clear();
        if let Ok(fvar) = font.fvar() {
            if let Ok((axes, instance)) = fvar
                .axis_instance_arrays()
                .and_then(|arrays| Ok((arrays.axes(), arrays.instances().get(index)?)))
            {
                self.set_variations(
                    font,
                    axes.iter()
                        .zip(instance.coordinates)
                        .map(|(axis, coord)| (axis.axis_tag(), coord.get().to_f32())),
                );
            }
        }
    }

    fn set_feature_variations(&mut self, font: &FontRef) {
        self.feature_variations = [None; 2];
        if self.coords.is_empty() {
            return;
        }
        self.feature_variations[0] = font
            .gsub()
            .ok()
            .and_then(|t| LayoutTable::Gsub(t).feature_variation_index(&self.coords));
        self.feature_variations[1] = font
            .gpos()
            .ok()
            .and_then(|t| LayoutTable::Gpos(t).feature_variation_index(&self.coords));
    }

    fn check_default(&mut self) {
        if self.coords.iter().all(|coord| *coord == F2Dot14::ZERO) {
            self.coords.clear();
        }
    }
}

/// Builder type for constructing a [`Shaper`](crate::Shaper).
pub struct ShaperBuilder<'a> {
    data: &'a ShaperData,
    font: FontRef<'a>,
    instance: Option<&'a ShaperInstance>,
    point_size: Option<f32>,
    class: Option<hb_font_funcs_t<'a>>,
}

impl<'a> ShaperBuilder<'a> {
    /// Sets an optional instance for the shaper.
    ///
    /// This defines the variable font configuration.
    pub fn instance(mut self, instance: Option<&'a ShaperInstance>) -> Self {
        self.instance = instance;
        self
    }

    /// Sets the point size for the shaper.
    ///
    /// This controls adjustments provided by the tracking table.
    pub fn point_size(mut self, size: Option<f32>) -> Self {
        self.point_size = size;
        self
    }

    /// Sets custom font functions for the shaper.
    ///
    /// This allows processes like glyph hinting that effect glyph metrics to be recognised by the shaper.
    pub fn font_funcs<T: FontFuncs + 'a>(self, funcs: T) -> Self {
        self.font_funcs_boxed(std::sync::Arc::new(funcs))
    }

    /// Sets custom font functions for the shaper.
    ///
    /// This allows processes like glyph hinting that effect glyph metrics to be recognised by the shaper.
    pub fn font_funcs_boxed(mut self, class: hb_font_funcs_t<'a>) -> Self {
        self.class = Some(class);
        self
    }

    /// Builds the shaper with the current configuration.
    pub fn build(self) -> crate::Shaper<'a> {
        let font = self.font;
        let units_per_em = self.data.table_ranges.units_per_em;
        let charmap = Charmap::new(&font, &self.data.table_ranges, &self.data.cmap_cache);
        let (coords, feature_variations) = self
            .instance
            .map(|instance| (instance.coords(), instance.feature_variations))
            .unwrap_or_default();
        let ot_tables = OtTables::new(
            &font,
            &self.data.ot_cache,
            &self.data.table_ranges,
            coords,
            feature_variations,
        );
        let aat_tables = AatTables::new(&font, &self.data.aat_cache, &self.data.table_ranges);
        let class = self
            .class
            .unwrap_or_else(|| std::sync::Arc::new(OtFontFuncs::new(&font, self.data)));
        hb_font_t {
            font,
            units_per_em,
            points_per_em: self.point_size,
            charmap,
            ot_tables,
            aat_tables,
            class,
        }
    }
}

fn delegate_to_multi<F: FontFuncs + ?Sized, O>(
    f: fn(&F, &crate::Shaper, &[crate::GlyphInfo], &mut [crate::GlyphPosition]),
    ff: &F,
    font: &crate::Shaper,
    glyph: GlyphId,
    m: impl FnOnce(crate::GlyphPosition) -> O,
) -> O {
    let mut info = crate::GlyphInfo::default();
    let mut pos = crate::GlyphPosition::default();
    info.glyph_id = glyph.to_u32();
    f(
        ff,
        font,
        std::slice::from_ref(&info),
        std::slice::from_mut(&mut pos),
    );
    m(pos)
}
fn delegate_to_multires<F: FontFuncs + ?Sized, O>(
    f: fn(&F, &crate::Shaper, &[crate::GlyphInfo], &mut [crate::GlyphPosition]) -> Result<(), ()>,
    ff: &F,
    font: &crate::Shaper,
    glyph: GlyphId,
    m: impl FnOnce(crate::GlyphPosition) -> O,
) -> Result<O, ()> {
    let mut info = crate::GlyphInfo::default();
    let mut pos = crate::GlyphPosition::default();
    info.glyph_id = glyph.to_u32();
    f(
        ff,
        font,
        std::slice::from_ref(&info),
        std::slice::from_mut(&mut pos),
    )?;
    Ok(m(pos))
}

// TODO: Investigate using a custom `Cow<'a, dyn FontFuncs>` here.
type hb_font_funcs_t<'a> = std::sync::Arc<dyn FontFuncs + 'a>;

/// A set of font functions used by the shaper to retrieve glyph metrics and other font data.
///
/// These functions should be overriden to reflect any modifications to the font metrics, such as hinting, that are applied per-glyph.
pub trait FontFuncs {
    // HarfBuzz doesn't use the line extents functions. They can be retreived from Skrifa/Read-Fonts if needed.
    // fn font_h_extents(&self, font: &crate::Shaper) -> Option<hb_font_extents_t>;
    // fn font_v_extents(&self, font: &crate::Shaper) -> Option<hb_font_extents_t>;

    // I don't think these will need to be overriden by a consumer. We only support Read-Fonts as a backend.
    // fn nominal_glyph(&self, font: &crate::Shaper, codepoint: char) -> Option<GlyphId>;
    // fn nominal_glyphs(
    //     &self,
    //     font: &crate::Shaper,
    //     count: usize,
    //     codepoint: &mut dyn FnMut(usize) -> Option<u32>,
    //     glyph: &mut dyn FnMut(usize, GlyphId) -> bool,
    // ) -> usize;
    // fn variation_glyph(
    //     &self,
    //     font: &crate::Shaper,
    //     codepoint: char,
    //     variation_selector: char,
    // ) -> Option<GlyphId>;

    /// Retrieve the advance for a specific glyph, in horizontal-direction text segments. Returns a value in font coordinates.
    fn glyph_h_advance(&self, font: &crate::Shaper, glyph: GlyphId) -> i32 {
        delegate_to_multi(Self::glyph_h_advances, self, font, glyph, |p| p.x_advance)
    }
    /// Retrieve the advance for a specific glyph, in vertical-direction text segments. Returns a value in font coordinates.
    fn glyph_v_advance(&self, font: &crate::Shaper, glyph: GlyphId) -> i32 {
        delegate_to_multi(Self::glyph_v_advances, self, font, glyph, |p| p.y_advance)
    }
    /// Retrieve the advances for a specific sequence of glyphs into `advance[n].x_advance`, in horizontal-direction text segments.
    fn glyph_h_advances(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        advance: &mut [crate::GlyphPosition],
    );
    /// Retrieve the advances for a specific sequence of glyphs into `advance[n].y_advance`, in vertical-direction text segments.
    fn glyph_v_advances(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        advance: &mut [crate::GlyphPosition],
    );

    /// Retrieve the (x, y) coordinates of the origin for a glyph, for horizontal-direction text segments.
    fn glyph_h_origin(&self, font: &crate::Shaper, glyph: GlyphId) -> Result<(i32, i32), ()> {
        delegate_to_multires(Self::glyph_h_origins, self, font, glyph, |p| {
            (p.x_offset, p.y_offset)
        })
    }
    /// Retrieve the (x, y) coordinates of the origin for a glyph, for vertical-direction text segments.
    fn glyph_v_origin(&self, font: &crate::Shaper, glyph: GlyphId) -> Result<(i32, i32), ()> {
        delegate_to_multires(Self::glyph_v_origins, self, font, glyph, |p| {
            (p.x_offset, p.y_offset)
        })
    }
    /// Retrieve the origins for a specific sequence of glyphs into `origin[n].{x,y}_offset`, for horizontal-direction text segments.
    fn glyph_h_origins(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        origin: &mut [crate::GlyphPosition],
    ) -> Result<(), ()>;
    /// Retrieve the origins for a specific sequence of glyphs into `origin[n].{x,y}_offset`, for vertical-direction text segments.
    fn glyph_v_origins(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        origin: &mut [crate::GlyphPosition],
    ) -> Result<(), ()>;

    /// Flag if the values returned by `glyph_h_origins` won't always be `(0, 0)`.
    fn non_default_h_origins(&self, font: &crate::Shaper) -> bool;

    // Deprecated in HarfBuzz

    // fn glyph_h_kerning(
    //     &self,
    //     font: &crate::Shaper,
    //     first_glyph: GlyphId,
    //     second_glyph: GlyphId,
    // ) -> i32;
    // fn glyph_v_kerning(
    //     &self,
    //     font: &crate::Shaper,
    //     first_glyph: GlyphId,
    //     second_glyph: GlyphId,
    // ) -> i32;

    /// Retrieve the extents for a specified glyph.
    fn glyph_extents(&self, font: &crate::Shaper, glyph: GlyphId) -> Option<crate::GlyphExtents>;

    // HarfBuzz has some more cosmetic/draw functions that are better off done by Skrifa/Read-Fonts.
}

/// A configured shaper.
#[derive(Clone)]
pub struct hb_font_t<'a> {
    pub(crate) font: FontRef<'a>,
    pub(crate) units_per_em: u16,
    pub(crate) points_per_em: Option<f32>,
    charmap: Charmap<'a>,
    pub(crate) ot_tables: OtTables<'a>,
    pub(crate) aat_tables: AatTables<'a>,
    class: hb_font_funcs_t<'a>,
}

impl<'a> crate::Shaper<'a> {
    /// Returns font's units per EM.
    #[inline]
    pub fn units_per_em(&self) -> i32 {
        self.units_per_em as i32
    }

    /// Returns the currently active normalized coordinates.
    pub fn coords(&self) -> &'a [NormalizedCoord] {
        self.ot_tables.coords
    }

    /// Shapes the buffer content using provided font and features.
    ///
    /// Consumes the buffer. You can then run [`GlyphBuffer::clear`] to get the [`UnicodeBuffer`] back
    /// without allocating a new one.
    ///
    /// If you plan to shape multiple strings, prefer [`shape_with_plan`](Self::shape_with_plan).
    /// This is because [`ShapePlan`](crate::ShapePlan) initialization is pretty slow and should preferably
    /// be called once for each shaping configuration.
    pub fn shape(&self, buffer: UnicodeBuffer, features: &[Feature]) -> GlyphBuffer {
        let plan = ShapePlan::new(
            self,
            buffer.0.direction,
            buffer.0.script,
            buffer.0.language.as_ref(),
            features,
        );
        self.shape_with_plan(&plan, buffer, features)
    }

    /// Shapes the buffer content using the provided font and plan.
    ///
    /// Consumes the buffer. You can then run [`GlyphBuffer::clear`] to get the [`UnicodeBuffer`] back
    /// without allocating a new one.
    ///
    /// It is up to the caller to ensure that the shape plan matches the properties of the provided
    /// buffer, otherwise the shaping result will likely be incorrect.
    ///
    /// # Panics
    ///
    /// Will panic when debugging assertions are enabled if the buffer and plan have mismatched
    /// properties.
    pub fn shape_with_plan(
        &self,
        plan: &ShapePlan,
        buffer: UnicodeBuffer,
        features: &[Feature],
    ) -> GlyphBuffer {
        let mut buffer = buffer.0;
        buffer.enter();

        assert_eq!(
            buffer.direction, plan.direction,
            "Buffer direction does not match plan direction: {:?} != {:?}",
            buffer.direction, plan.direction
        );
        assert_eq!(
            buffer.script.unwrap_or(script::UNKNOWN),
            plan.script.unwrap_or(script::UNKNOWN),
            "Buffer script does not match plan script: {:?} != {:?}",
            buffer.script.unwrap_or(script::UNKNOWN),
            plan.script.unwrap_or(script::UNKNOWN)
        );

        if buffer.len > 0 {
            // Save the original direction, we use it later.
            let target_direction = buffer.direction;
            shape_internal(&mut hb_ot_shape_context_t {
                plan,
                face: self,
                buffer: &mut buffer,
                target_direction,
                features,
            });
        }

        buffer.leave();

        GlyphBuffer(buffer)
    }

    pub(crate) fn has_glyph(&self, c: u32) -> bool {
        self.get_nominal_glyph(c).is_some()
    }

    pub(crate) fn get_nominal_glyph(&self, c: u32) -> Option<GlyphId> {
        self.charmap.map(c)
    }

    pub(crate) fn get_nominal_variant_glyph(&self, c: u32, vs: u32) -> Option<GlyphId> {
        self.charmap.map_variant(c, vs)
    }

    pub(crate) fn glyph_h_advance(&self, glyph: GlyphId) -> i32 {
        self.class.glyph_h_advance(self, glyph)
    }

    pub(crate) fn glyph_v_advance(&self, glyph: GlyphId) -> i32 {
        self.class.glyph_v_advance(self, glyph)
    }

    pub(crate) fn glyph_h_advances(
        &self,
        glyph: &[crate::GlyphInfo],
        advance: &mut [crate::GlyphPosition],
    ) {
        self.class.glyph_h_advances(self, glyph, advance);
    }

    pub(crate) fn glyph_v_advances(
        &self,
        glyph: &[crate::GlyphInfo],
        advance: &mut [crate::GlyphPosition],
    ) {
        self.class.glyph_v_advances(self, glyph, advance);
    }

    /* pub(crate) fn glyph_h_origin(&self, glyph: GlyphId) -> Result<(i32, i32), ()> {
        self.class.glyph_h_origin(self, glyph)
    }

    pub(crate) fn glyph_v_origin(&self, glyph: GlyphId) -> Result<(i32, i32), ()> {
        self.class.glyph_v_origin(self, glyph)
    } */

    pub(crate) fn non_default_h_origins(&self) -> bool {
        self.class.non_default_h_origins(self)
    }

    pub(crate) fn glyph_h_origins(
        &self,
        glyph: &[crate::GlyphInfo],
        origin: &mut [crate::GlyphPosition],
    ) -> Result<(), ()> {
        self.class.glyph_h_origins(self, glyph, origin)
    }

    pub(crate) fn glyph_v_origins(
        &self,
        glyph: &[crate::GlyphInfo],
        origin: &mut [crate::GlyphPosition],
    ) -> Result<(), ()> {
        self.class.glyph_v_origins(self, glyph, origin)
    }

    pub(crate) fn glyph_extents(&self, glyph: GlyphId) -> Option<hb_glyph_extents_t> {
        self.class.glyph_extents(self, glyph)
    }

    pub(crate) fn apply_glyph_origins_with_fallback<const VERTICAL: bool>(
        &self,
        glyph: &[crate::GlyphInfo],
        origin: &mut [crate::GlyphPosition],
        mult: i32,
    ) {
        // let has_ascender = false;
        // let ascender = 0;

        let mut origin_scratch = [crate::GlyphPosition::default(); 32];
        let mut offset = 0;
        let count = glyph.len();
        while offset < count {
            let len = std::cmp::min(count - offset, origin_scratch.len());

            if let Err(()) = if VERTICAL {
                Self::glyph_v_origins
            } else {
                Self::glyph_h_origins
            }(
                self,
                &glyph[offset..offset + len],
                &mut origin_scratch[..len],
            ) {
                // TODO: Implement fallback
                // match if VERTICAL { self.glyph_h_origins } else { self.glyph_v_origins } (
                //     self,
                //     &glyph[offset..offset + len],
                //     &mut origin_scratch[offset..offset + len],
                // ) {
                //     Ok(()) => {
                //     }
                //     Err(()) => {
                //         origin_scratch[..len].fill(crate::GlyphPosition::default());
                //     }
                // }
            }

            match mult {
                1 => {
                    for i in 0..len {
                        origin[offset + i].x_offset += origin_scratch[i].x_offset;
                        origin[offset + i].y_offset += origin_scratch[i].y_offset;
                    }
                }
                -1 => {
                    for i in 0..len {
                        origin[offset + i].x_offset -= origin_scratch[i].x_offset;
                        origin[offset + i].y_offset -= origin_scratch[i].y_offset;
                    }
                }
                _ => unreachable!(),
            }

            offset += len;
        }
    }

    pub(crate) fn glyph_names(&self) -> GlyphNames<'a> {
        GlyphNames::new(&self.font)
    }

    pub(crate) fn layout_table(&self, table_index: TableIndex) -> Option<LayoutTable<'a>> {
        match table_index {
            TableIndex::GSUB => self
                .ot_tables
                .gsub
                .as_ref()
                .map(|table| LayoutTable::Gsub(table.table.clone())),
            TableIndex::GPOS => self
                .ot_tables
                .gpos
                .as_ref()
                .map(|table| LayoutTable::Gpos(table.table.clone())),
        }
    }

    pub(crate) fn layout_tables(&self) -> impl Iterator<Item = (TableIndex, LayoutTable<'a>)> + '_ {
        TableIndex::iter().filter_map(move |idx| self.layout_table(idx).map(|table| (idx, table)))
    }
}

#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
/// Glyph extent values, measured in font units.
///
/// Note that height is negative, in coordinate systems that grow up.
pub struct hb_glyph_extents_t {
    /// Distance from the x-origin to the left extremum of the glyph.
    pub x_bearing: i32,
    /// Distance from the top extremum of the glyph to the y-origin.
    pub y_bearing: i32,
    /// Distance from the left extremum of the glyph to the right extremum.
    pub width: i32,
    /// Distance from the top extremum of the glyph to the bottom extremum.
    pub height: i32,
}

/// Adjusts font metrics for synthetic slant/emboldening, wrapping another [`FontFuncs`] implementation.
pub struct SyntheticFontFuncs<T> {
    /// The wrapped font function implementation.
    pub base: T,

    /// The horizontal emboldening strength.
    pub x_strength: i32,
    /// The vertical emboldening strength.
    pub y_strength: i32,
    /// If true, the emboldening is applied in-place, without changing the glyph advances.
    pub embolden_in_place: bool,

    /// The slant ratio.
    pub slant: f32,
}
impl<T> SyntheticFontFuncs<T> {
    /// Creates new synthetic font functions wrapping the given base implementation, accounting for emboldening.
    ///
    /// - `embolden_in_place`: If true, the emboldening is applied in-place, without changing the glyph advances.
    pub fn new_bold(base: T, x_strength: i32, y_strength: i32, embolden_in_place: bool) -> Self {
        Self {
            base,
            x_strength,
            y_strength,
            embolden_in_place,
            slant: 0.0,
        }
    }

    /// Creates new synthetic font functions wrapping the given base implementation, accounting for slanting.
    ///
    /// - `slant`: The slant ratio, where a 0.2 slant would result in a point 1 unit up being shifted horizontally by 0.2 units.
    pub fn new_slant(base: T, slant: f32) -> Self {
        Self {
            base,
            x_strength: 0,
            y_strength: 0,
            embolden_in_place: false,
            slant,
        }
    }
}
impl<T: FontFuncs> FontFuncs for SyntheticFontFuncs<T> {
    fn glyph_h_advances(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        advance: &mut [crate::GlyphPosition],
    ) {
        self.base.glyph_h_advances(font, glyph, advance);

        if self.x_strength != 0 && !self.embolden_in_place {
            for pos in advance.iter_mut().filter(|p| p.x_advance != 0) {
                pos.x_advance += self.x_strength;
            }
        }
    }

    fn glyph_v_advances(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        advance: &mut [crate::GlyphPosition],
    ) {
        self.base.glyph_v_advances(font, glyph, advance);

        if self.y_strength != 0 && !self.embolden_in_place {
            for pos in advance.iter_mut().filter(|p| p.y_advance != 0) {
                pos.y_advance += self.y_strength;
            }
        }
    }

    fn glyph_h_origins(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        origin: &mut [crate::GlyphPosition],
    ) -> Result<(), ()> {
        self.base.glyph_h_origins(font, glyph, origin)?;

        for pos in origin.iter_mut() {
            if !self.embolden_in_place {
                pos.x_offset += self.x_strength;
                pos.y_offset += self.y_strength;
            }
        }

        Ok(())
    }

    fn non_default_h_origins(&self, font: &crate::Shaper) -> bool {
        self.base.non_default_h_origins(font) || !self.embolden_in_place
    }

    fn glyph_v_origins(
        &self,
        font: &crate::Shaper,
        glyph: &[crate::GlyphInfo],
        origin: &mut [crate::GlyphPosition],
    ) -> Result<(), ()> {
        self.base.glyph_v_origins(font, glyph, origin)?;

        for pos in origin.iter_mut() {
            if !self.embolden_in_place {
                pos.x_offset += self.x_strength;
                pos.y_offset += self.y_strength;
            }
        }

        Ok(())
    }

    fn glyph_extents(&self, font: &crate::Shaper, glyph: GlyphId) -> Option<hb_glyph_extents_t> {
        // HarfRust doesn't support drawing glyph outlines yet, so we can't get perfectly accurate synthesised extents.

        let mut extents = self.base.glyph_extents(font, glyph)?;

        if self.slant != 0.0 {
            let mut x1 = extents.x_bearing;
            let y1 = extents.y_bearing;
            let mut x2 = extents.x_bearing + extents.width;
            let y2 = extents.y_bearing + extents.height;

            x1 += std::cmp::min_by(
                y1 as f32 * self.slant,
                y2 as f32 * self.slant,
                f32::total_cmp,
            )
            .floor() as i32;
            x2 += std::cmp::max_by(
                y1 as f32 * self.slant,
                y2 as f32 * self.slant,
                f32::total_cmp,
            )
            .ceil() as i32;

            extents.x_bearing = x1;
            extents.width = x2 - x1;
        }

        if self.x_strength != 0 || self.y_strength != 0 {
            extents.y_bearing += self.y_strength;
            extents.height -= self.y_strength;

            extents.x_bearing -= self.x_strength / 2;
            extents.width += self.x_strength;
        }

        Some(extents)
    }
}
