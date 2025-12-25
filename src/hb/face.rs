use read_fonts::types::{F2Dot14, Fixed, GlyphId};
use read_fonts::{FontRef, TableProvider};
use smallvec::SmallVec;

use super::aat::AatTables;
use super::charmap::{cache_t as cmap_cache_t, Charmap};
use super::glyph_metrics::GlyphMetrics;
use super::glyph_names::GlyphNames;
use super::ot::{LayoutTable, OtCache, OtTables};
use super::ot_layout::TableIndex;
use super::ot_shape::{hb_ot_shape_context_t, shape_internal};
use crate::hb::aat::AatCache;
use crate::hb::tables::TableRanges;
use crate::{script, Feature, GlyphBuffer, NormalizedCoord, ShapePlan, UnicodeBuffer, Variation};

/// Data required for shaping with a single font.
pub struct ShaperData {
    table_ranges: TableRanges,
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
            font_funcs: None,
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
    font_funcs: Option<&'a dyn FontFuncs>,
    instance: Option<&'a ShaperInstance>,
    point_size: Option<f32>,
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
    /// This allows the the glyph scaler's font functions to be used during shaping,
    /// ensuring that both the renderer and shaper use the same metrics. This is useful when
    /// the scaler applies hinting or other adjustments that affect glyph metrics that would
    /// otherwise cause shaped positions to become out of sync.
    pub fn font_funcs(mut self, funcs: &'a dyn FontFuncs) -> Self {
        self.font_funcs = Some(funcs);
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
        hb_font_t {
            units_per_em,
            points_per_em: self.point_size,
            charmap,
            ot_tables,
            aat_tables,
            font_funcs: self.font_funcs.map_or_else(
                || {
                    FontFuncStorage::Default(DefaultFontFuncs(GlyphMetrics::new(
                        &font,
                        &self.data.table_ranges,
                    )))
                },
                |f| FontFuncStorage::Custom(f),
            ),
            font,
        }
    }
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
    font_funcs: FontFuncStorage<'a>,
}

impl<'a> crate::Shaper<'a> {
    /// Returns font's units per EM.
    #[inline]
    pub fn units_per_em(&self) -> i32 {
        self.units_per_em as i32
    }

    /// Returns the currently active normalized coordinates.
    #[inline]
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

    pub(crate) fn glyph_names(&self) -> GlyphNames<'a> {
        GlyphNames::new(&self.font)
    }

    pub(crate) fn font_funcs(&self) -> &dyn FontFuncs {
        &self.font_funcs
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

/// Glyph extent values, measured in font units.
///
/// Note that height is negative, in coordinate systems that grow up.
#[derive(Clone, Copy, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
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

/// Custom font functions used while shaping.
///
/// See [`ShaperBuilder::font_funcs`] for more.
pub trait FontFuncs {
    /// Retrieve the advance width for a specified glyph as a single coordinate value, for horizontal-direction text segments.
    fn glyph_h_advance(&self, font: &crate::Shaper, glyph: GlyphId) -> i32;
    /// Retreive the advance height for a specified glyph as a single coordinate value, for vertical-direction text segments.
    fn glyph_v_advance(&self, font: &crate::Shaper, glyph: GlyphId) -> i32;

    /// Populate the [`GlyphPosition`]s' `x_advance` values for each corresponding [`GlyphInfo`], for horizontal-direction text segments.
    fn glyph_h_advances(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    );
    /// Populate the [`GlyphPosition`]s' `y_advance` values for each corresponding [`GlyphInfo`], for vertical-direction text segments.
    fn glyph_v_advances(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    );

    /// Retrieve the (x, y) coordinates of a glyph's origin, for horizontal-direction text segments.
    fn glyph_h_origin(&self, font: &crate::Shaper, glyph: GlyphId) -> (i32, i32);
    /// Retrieve the (x, y) coordinates of a glyph's origin, for vertical-direction text segments.
    fn glyph_v_origin(&self, font: &crate::Shaper, glyph: GlyphId) -> (i32, i32);

    /// Populate the [`GlyphPosition`]s' `x_offset` and `y_offset` values for each corresponding [`GlyphInfo`], for horizontal-direction text segments.
    fn glyph_h_origins(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    );
    /// Populate the [`GlyphPosition`]s' `x_offset` and `y_offset` values for each corresponding [`GlyphInfo`], for vertical-direction text segments.
    fn glyph_v_origins(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    );

    /// Retrieve the kerning adjustment value for a glyph pair, for horizontal segments.
    fn glyph_h_kerning(&self, font: &crate::Shaper, glyphs: (GlyphId, GlyphId));

    /// Retrieve the extents for a specified glyph.
    fn glyph_extents(&self, font: &crate::Shaper, glyph: GlyphId) -> Option<crate::GlyphExtents>;
}

#[derive(Clone)]
enum FontFuncStorage<'a> {
    Default(DefaultFontFuncs<'a>),
    Custom(&'a dyn FontFuncs),
}
impl FontFuncs for FontFuncStorage<'_> {
    fn glyph_h_advance(&self, font: &crate::Shaper, glyph: GlyphId) -> i32 {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_h_advance(font, glyph),
            FontFuncStorage::Custom(funcs) => funcs.glyph_h_advance(font, glyph),
        }
    }

    fn glyph_h_advances(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    ) {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_h_advances(font, info, pos),
            FontFuncStorage::Custom(funcs) => funcs.glyph_h_advances(font, info, pos),
        }
    }

    fn glyph_v_advance(&self, font: &crate::Shaper, glyph: GlyphId) -> i32 {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_v_advance(font, glyph),
            FontFuncStorage::Custom(funcs) => funcs.glyph_v_advance(font, glyph),
        }
    }

    fn glyph_v_advances(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    ) {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_v_advances(font, info, pos),
            FontFuncStorage::Custom(funcs) => funcs.glyph_v_advances(font, info, pos),
        }
    }

    fn glyph_h_origin(&self, font: &crate::Shaper, glyph: GlyphId) -> (i32, i32) {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_h_origin(font, glyph),
            FontFuncStorage::Custom(funcs) => funcs.glyph_h_origin(font, glyph),
        }
    }

    fn glyph_v_origin(&self, font: &crate::Shaper, glyph: GlyphId) -> (i32, i32) {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_v_origin(font, glyph),
            FontFuncStorage::Custom(funcs) => funcs.glyph_v_origin(font, glyph),
        }
    }

    fn glyph_h_origins(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    ) {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_h_origins(font, info, pos),
            FontFuncStorage::Custom(funcs) => funcs.glyph_h_origins(font, info, pos),
        }
    }

    fn glyph_v_origins(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    ) {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_v_origins(font, info, pos),
            FontFuncStorage::Custom(funcs) => funcs.glyph_v_origins(font, info, pos),
        }
    }

    fn glyph_h_kerning(&self, font: &crate::Shaper, glyphs: (GlyphId, GlyphId)) {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_h_kerning(font, glyphs),
            FontFuncStorage::Custom(funcs) => funcs.glyph_h_kerning(font, glyphs),
        }
    }

    fn glyph_extents(&self, font: &crate::Shaper, glyph: GlyphId) -> Option<crate::GlyphExtents> {
        match self {
            FontFuncStorage::Default(funcs) => funcs.glyph_extents(font, glyph),
            FontFuncStorage::Custom(funcs) => funcs.glyph_extents(font, glyph),
        }
    }
}

/// A default set of font functions.
#[derive(Clone)]
pub struct DefaultFontFuncs<'a>(GlyphMetrics<'a>);
impl<'a> DefaultFontFuncs<'a> {
    /// Creates a new set of default font functions for the given font and shaper data.
    pub fn new(font: &FontRef<'a>, shaper_data: &ShaperData) -> Self {
        Self(GlyphMetrics::new(font, &shaper_data.table_ranges))
    }
}
impl FontFuncs for DefaultFontFuncs<'_> {
    fn glyph_h_advance(&self, font: &crate::Shaper, glyph: GlyphId) -> i32 {
        self.0
            .advance_width(glyph, font.coords())
            .unwrap_or_default()
    }

    fn glyph_h_advances(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    ) {
        self.0.populate_advance_widths(info, pos, font.coords());
    }

    fn glyph_v_advance(&self, font: &crate::Shaper, glyph: GlyphId) -> i32 {
        -self
            .0
            .advance_height(glyph, font.coords())
            .unwrap_or(font.units_per_em())
    }

    fn glyph_v_advances(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    ) {
        self.0.populate_advance_heights(info, pos, font.coords());
    }

    fn glyph_h_origin(&self, font: &crate::Shaper, glyph: GlyphId) -> (i32, i32) {
        _ = (font, glyph);
        (0, 0)
    }

    fn glyph_v_origin(&self, font: &crate::Shaper, glyph: GlyphId) -> (i32, i32) {
        (
            self.glyph_h_advance(font, glyph) / 2,
            self.0.v_origin(glyph, font.coords()).unwrap_or_default(),
        )
    }

    fn glyph_h_origins(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    ) {
        for (info, pos) in info.iter().zip(pos.iter_mut()) {
            let (x, y) = self.glyph_h_origin(font, info.as_glyph());
            pos.x_offset = x;
            pos.y_offset = y;
        }
    }

    fn glyph_v_origins(
        &self,
        font: &crate::Shaper,
        info: &[crate::GlyphInfo],
        pos: &mut [crate::GlyphPosition],
    ) {
        for (info, pos) in info.iter().zip(pos.iter_mut()) {
            let (x, y) = self.glyph_v_origin(font, info.as_glyph());
            pos.x_offset = x;
            pos.y_offset = y;
        }
    }

    fn glyph_h_kerning(&self, font: &crate::Shaper, glyphs: (GlyphId, GlyphId)) {
        _ = (font, glyphs);
        todo!()
    }

    fn glyph_extents(&self, font: &crate::Shaper, glyph: GlyphId) -> Option<crate::GlyphExtents> {
        self.0
            .extents(glyph, font.coords())
            .map(|bbox| crate::GlyphExtents {
                x_bearing: bbox.x_min,
                y_bearing: bbox.y_max,
                width: bbox.x_max - bbox.x_min,
                height: bbox.y_min - bbox.y_max,
            })
    }
}
