//! Per-cell water depth carving from a chamfer-3-4 distance transform over the LC_WATER mask.

use crate::block_definitions::{
    Block, AIR, BLUE_FLOWER, CLAY, COARSE_DIRT, DEAD_BUSH, DIRT, FERN, GRASS, GRAVEL, KELP,
    KELP_PLANT, LARGE_FERN_LOWER, LARGE_FERN_UPPER, OAK_LEAVES, RED_FLOWER, SAND, SANDSTONE,
    SEAGRASS, SEA_PICKLE, SOUL_SAND, STONE, TALL_GRASS_BOTTOM, TALL_GRASS_TOP,
    TALL_SEAGRASS_BOTTOM, TALL_SEAGRASS_TOP, WATER, WHITE_FLOWER, YELLOW_FLOWER,
};
use crate::coordinate_system::cartesian::{XZBBox, XZPoint};
use crate::floodfill_cache::RoadMaskBitmap;
use crate::ground::Ground;
use crate::land_cover::LC_WATER;
use crate::world_editor::{min_y, WorldEditor};

/// Flat shoal width in chamfer-DT units; slope only starts past it.
const SHOAL_DT_UNITS: u16 = 9;

/// Chamfer-distance saturation cap for the legacy shoreline-derived field.
const DT_MAX: u8 = u8::MAX;

/// Maximum representable water carve depth, in blocks. The legacy mask-based
/// fallback still tops out at 6; bathymetry overrides may use the full u8 range.
const MAX_WATER_DEPTH: i32 = u8::MAX as i32;

/// Cap on water sub-rect cells (bounds memory, keeps u32 indices valid); ~1000 km².
const MAX_WATER_FIELD_CELLS: usize = 1_000_000_000;

/// Frontier capacity carried between components; a bigger one gives its buffer back.
const FRONTIER_KEEP: usize = 1 << 16;

#[inline]
fn nibble_get(buf: &[u8], i: usize) -> u8 {
    let byte = buf[i >> 1];
    if i & 1 == 0 {
        byte & 0x0F
    } else {
        byte >> 4
    }
}

#[inline]
fn nibble_set(buf: &mut [u8], i: usize, v: u8) {
    debug_assert!(v < 16, "nibble value {v} out of range");
    let byte = &mut buf[i >> 1];
    if i & 1 == 0 {
        *byte = (*byte & 0xF0) | (v & 0x0F);
    } else {
        *byte = (*byte & 0x0F) | ((v & 0x0F) << 4);
    }
}

#[inline]
fn bit_get(b: &[u64], i: usize) -> bool {
    (b[i >> 6] >> (i & 63)) & 1 == 1
}

#[inline]
fn bit_set(b: &mut [u64], i: usize) {
    b[i >> 6] |= 1u64 << (i & 63);
}

#[inline]
fn for_each_neighbor(idx: usize, w: usize, h: usize, mut f: impl FnMut(usize)) {
    let i = idx % w;
    let j = idx / w;
    if i > 0 {
        f(idx - 1);
    }
    if i + 1 < w {
        f(idx + 1);
    }
    if j > 0 {
        f(idx - w);
    }
    if j + 1 < h {
        f(idx + w);
    }
}

/// Baked per-cell water carve depths over the water sub-rectangle.
pub struct BigWaterField {
    /// Legacy shoreline-distance depth, nibble-packed (0..=6).
    depth: Vec<u8>,
    /// Optional real-world bathymetry depth after compression, one byte per cell.
    /// A zero entry means that no usable bathymetry sample was available there.
    bathymetry_depth_blocks: Option<Vec<u8>>,
    width: usize,
    height: usize,
    min_x: i32,
    min_z: i32,
}

impl BigWaterField {
    fn empty() -> Self {
        Self {
            depth: Vec::new(),
            bathymetry_depth_blocks: None,
            width: 0,
            height: 0,
            min_x: 0,
            min_z: 0,
        }
    }

    #[inline]
    fn local_idx(&self, x: i32, z: i32) -> Option<usize> {
        let lx = i64::from(x) - i64::from(self.min_x);
        let lz = i64::from(z) - i64::from(self.min_z);
        if lx < 0 || lz < 0 || lx as usize >= self.width || lz as usize >= self.height {
            return None;
        }
        Some(lz as usize * self.width + lx as usize)
    }

    /// Carve depth at the cell; 0 outside the water sub-rect.
    #[inline]
    pub fn depth_at(&self, x: i32, z: i32) -> i32 {
        match self.local_idx(x, z) {
            Some(i) => {
                if let Some(bathy) = self.bathymetry_depth_blocks.as_ref() {
                    let compressed = bathy.get(i).copied().unwrap_or(0);
                    if compressed > 0 {
                        return i32::from(compressed);
                    }
                }
                i32::from(nibble_get(&self.depth, i))
            }
            None => 0,
        }
    }

    /// Max depth in the 7x7 around (x, z); proxy for body width. Edge-safe via depth_at.
    pub fn body_max_7x7(&self, x: i32, z: i32) -> i32 {
        let mut m = 0;
        for dz in -3..=3 {
            for dx in -3..=3 {
                m = m.max(self.depth_at(x + dx, z + dz));
                if m >= MAX_WATER_DEPTH {
                    return m;
                }
            }
        }
        m
    }
}

/// Map a grid-cell span to the block span that samples into it (a safe superset).
pub(crate) fn grid_span_to_block_span(
    g_lo: usize,
    g_hi: usize,
    world_dim: usize,
    grid_dim: usize,
) -> (i32, i32) {
    if grid_dim <= 1 || world_dim <= 1 {
        return (0, world_dim.saturating_sub(1) as i32);
    }
    let f = (world_dim - 1) as f64 / (grid_dim - 1) as f64;
    let lo = ((g_lo as f64 - 0.5) * f).floor() as i32 - 1;
    let hi = ((g_hi as f64 + 0.5) * f).ceil() as i32 + 1;
    (lo.max(0), hi.min(world_dim as i32 - 1))
}

/// How [`BathymetryGrid::sample_depth_m`] reads the regular depth lattice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BathymetryInterpolation {
    /// Return the containing cell's value. Piecewise constant, so adjacent
    /// 500 m J-EGG500 cells meet in a vertical step.
    Nearest,
    /// Bilinearly interpolate between cell centres. Smooths the lattice into a
    /// continuous surface while preserving the sampled value at each centre.
    #[default]
    Bilinear,
}

/// Regular bathymetry samples already projected into Arnis world X/Z coordinates.
/// `depth_m` stores positive metres below the local water datum; non-positive or
/// non-finite values are treated as missing data and fall back to the legacy field.
/// Each sample covers a `cell_size_blocks` square in the X/Z plane. For example,
/// a 500 m J-EGG500 cell maps to 500 blocks at the default `scale = 1.0`
/// (one block per metre).
///
/// Samples are addressed by their cell centres. Bilinear interpolation therefore
/// returns exactly the stored value at a cell centre and a weighted blend on the
/// boundary between cells. Missing corners are skipped and the remaining weights
/// renormalised, so partial datasets stay usable.
///
/// This type deliberately does not parse J-EGG500/JHA files or perform geographic
/// projection. Convert source coordinates into this world-aligned grid in the
/// data-loading layer, where Arnis' map projection and scale are available.
///
/// A rotated world can attach rotation metadata with
/// [`BathymetryGrid::with_rotation`]. The stored lattice stays in pre-rotation
/// world space, while `sample_depth_m` maps final world X/Z back through the
/// inverse rotation. Keeping the source lattice untouched avoids resampling a
/// rotated 500 m grid into holes.
#[derive(Clone, Debug)]
pub struct BathymetryGrid {
    depth_m: Vec<f32>,
    width: usize,
    height: usize,
    min_x: i32,
    min_z: i32,
    cell_size_blocks: usize,
    interpolation: BathymetryInterpolation,
    rotation: Option<BathymetryRotation>,
}

/// Rotation applied to the generated world, stored in the form needed to map
/// final world coordinates back into the unrotated bathymetry lattice.
#[derive(Clone, Copy, Debug)]
struct BathymetryRotation {
    center_x: f64,
    center_z: f64,
    /// Sine/cosine of the forward map rotation (positive angle = clockwise).
    sin_r: f64,
    cos_r: f64,
}

impl BathymetryRotation {
    fn new(angle_degrees: f64, center_x: f64, center_z: f64) -> Option<Self> {
        if !angle_degrees.is_finite()
            || angle_degrees.abs() < f64::EPSILON
            || !center_x.is_finite()
            || !center_z.is_finite()
        {
            return None;
        }
        // Matches `rotate_world`: the user-facing angle is clockwise, while the
        // internal XZ rotation formula is counterclockwise.
        let rad = (-angle_degrees).to_radians();
        Some(Self {
            center_x,
            center_z,
            sin_r: rad.sin(),
            cos_r: rad.cos(),
        })
    }

    /// Inverse of the forward XZ rotation used by `rotate_world`.
    #[inline]
    fn inverse(&self, x: f64, z: f64) -> (f64, f64) {
        let dx = x - self.center_x;
        let dz = z - self.center_z;
        (
            dx * self.cos_r - dz * self.sin_r + self.center_x,
            dx * self.sin_r + dz * self.cos_r + self.center_z,
        )
    }
}

impl BathymetryGrid {
    /// Creates a world-X/Z grid. `min_x`/`min_z` mark the near corner of the first
    /// cell, and `cell_size_blocks` is that cell's width in Minecraft X/Z blocks.
    /// Returns an error when dimensions do not match data or cell size is zero.
    pub fn new(
        depth_m: Vec<f32>,
        width: usize,
        height: usize,
        min_x: i32,
        min_z: i32,
        cell_size_blocks: usize,
    ) -> Result<Self, &'static str> {
        let expected = width
            .checked_mul(height)
            .ok_or("bathymetry dimensions overflow")?;
        if width == 0 || height == 0 {
            return Err("bathymetry grid dimensions must be non-zero");
        }
        if cell_size_blocks == 0 {
            return Err("bathymetry cell size must be non-zero");
        }
        // Ensure the block-space extents cannot overflow when queried.
        width
            .checked_mul(cell_size_blocks)
            .ok_or("bathymetry X extent overflow")?;
        height
            .checked_mul(cell_size_blocks)
            .ok_or("bathymetry Z extent overflow")?;
        if depth_m.len() != expected {
            return Err("bathymetry sample count does not match width * height");
        }
        Ok(Self {
            depth_m,
            width,
            height,
            min_x,
            min_z,
            cell_size_blocks,
            interpolation: BathymetryInterpolation::default(),
            rotation: None,
        })
    }

    /// Selects how the lattice is sampled. Defaults to [`BathymetryInterpolation::Bilinear`].
    pub fn with_interpolation(mut self, interpolation: BathymetryInterpolation) -> Self {
        self.interpolation = interpolation;
        self
    }

    /// Attaches the world rotation used when the grid was generated. Samples are
    /// requested in final world coordinates and inverse-rotated into the stored
    /// pre-rotation lattice. A zero/non-finite angle leaves the grid unrotated.
    pub fn with_rotation(mut self, angle_degrees: f64, center_x: f64, center_z: f64) -> Self {
        self.rotation = BathymetryRotation::new(angle_degrees, center_x, center_z);
        self
    }

    /// Grid width in cells.
    #[inline]
    #[allow(dead_code)]
    pub fn width(&self) -> usize {
        self.width
    }

    #[allow(dead_code)]
    pub fn height(&self) -> usize {
        self.height
    }

    #[allow(dead_code)]
    pub fn cell_size_blocks(&self) -> usize {
        self.cell_size_blocks
    }

    #[allow(dead_code)]
    pub fn min_x(&self) -> i32 {
        self.min_x
    }

    #[allow(dead_code)]
    pub fn min_z(&self) -> i32 {
        self.min_z
    }

    /// Samples the bathymetry surface at world X/Z, returning metres below the
    /// local water datum, or `None` outside the grid or an all-NoData neighbourhood.
    pub fn sample_depth_m(&self, x: i32, z: i32) -> Option<f32> {
        self.sample_depth_m_at(x as f64, z as f64)
    }

    /// Float version used after inverse-rotating a final world coordinate. The
    /// unrotated path keeps the same integer-grid behaviour as before; the
    /// fractional part only matters for rotated worlds.
    fn sample_depth_m_at(&self, mut x: f64, mut z: f64) -> Option<f32> {
        if let Some(rotation) = self.rotation {
            (x, z) = rotation.inverse(x, z);
        }

        let lx = x - f64::from(self.min_x);
        let lz = z - f64::from(self.min_z);
        let span_x = (self.width * self.cell_size_blocks) as f64;
        let span_z = (self.height * self.cell_size_blocks) as f64;
        if lx < 0.0 || lz < 0.0 || lx >= span_x || lz >= span_z {
            return None;
        }

        match self.interpolation {
            BathymetryInterpolation::Nearest => {
                let cell = self.cell_size_blocks as f64;
                let gx = (lx / cell).floor() as usize;
                let gz = (lz / cell).floor() as usize;
                self.depth_value(gx.min(self.width - 1), gz.min(self.height - 1))
            }
            BathymetryInterpolation::Bilinear => self.sample_depth_m_bilinear(lx, lz),
        }
    }

    #[inline]
    fn depth_value(&self, gx: usize, gz: usize) -> Option<f32> {
        let value = *self.depth_m.get(gz * self.width + gx)?;
        (value.is_finite() && value > 0.0).then_some(value)
    }

    /// Cell centres sit at `(gx + 0.5) * cell_size`, so lattice coordinates are
    /// `offset / cell_size - 0.5`. Clamping the corner indices extends the edge
    /// cells by half a cell; positions outside the grid were rejected by the caller.
    fn sample_depth_m_bilinear(&self, lx: f64, lz: f64) -> Option<f32> {
        let cell = self.cell_size_blocks as f64;
        let fx = lx / cell - 0.5;
        let fz = lz / cell - 0.5;
        let x0 = fx.floor() as isize;
        let z0 = fz.floor() as isize;
        let tx = fx - x0 as f64;
        let tz = fz - z0 as f64;

        let mut sum = 0.0;
        let mut weight_sum = 0.0;
        for (dx, dz, weight) in [
            (0, 0, (1.0 - tx) * (1.0 - tz)),
            (1, 0, tx * (1.0 - tz)),
            (0, 1, (1.0 - tx) * tz),
            (1, 1, tx * tz),
        ] {
            if weight <= 0.0 {
                continue;
            }
            let gx = (x0 + dx).clamp(0, self.width as isize - 1) as usize;
            let gz = (z0 + dz).clamp(0, self.height as isize - 1) as usize;
            if let Some(value) = self.depth_value(gx, gz) {
                sum += f64::from(value) * weight;
                weight_sum += weight;
            }
        }

        (weight_sum > 0.0).then(|| (sum / weight_sum) as f32)
    }
}

/// Nonlinear real-depth-to-Minecraft-depth mapping.
///
/// Formula: `blocks = blocks_per_log_unit * ln(1 + depth_m / knee_m)`.
/// Larger `knee_m` preserves more separation in deep water; `max_blocks` limits
/// the generated vertical span. Any positive depth maps to at least one block.
#[derive(Clone, Copy, Debug)]
pub struct NonlinearDepthCompression {
    pub knee_m: f32,
    pub blocks_per_log_unit: f32,
    pub max_blocks: u8,
}

impl Default for NonlinearDepthCompression {
    fn default() -> Self {
        Self {
            knee_m: 100.0,
            blocks_per_log_unit: 32.0,
            max_blocks: 120,
        }
    }
}

impl NonlinearDepthCompression {
    /// Compresses positive water depth in metres to a Minecraft block count.
    #[inline]
    pub fn compress_meters(self, depth_m: f32) -> u8 {
        if !depth_m.is_finite()
            || depth_m <= 0.0
            || !self.knee_m.is_finite()
            || self.knee_m <= 0.0
            || !self.blocks_per_log_unit.is_finite()
            || self.blocks_per_log_unit <= 0.0
            || self.max_blocks == 0
        {
            return 0;
        }
        let blocks = self.blocks_per_log_unit * (1.0 + depth_m / self.knee_m).ln();
        (blocks.round().clamp(1.0, f32::from(self.max_blocks))) as u8
    }
}

/// Padded block rectangle covering every LC_WATER cell. `None` when there is no
/// water or the area exceeds the carving cap (flat water is used then).
fn water_field_bounds(ground: &Ground, xzbbox: &XZBBox) -> Option<(i32, i32, usize, usize)> {
    let min_x = xzbbox.min_x();
    let max_x = xzbbox.max_x();
    let min_z = xzbbox.min_z();
    let max_z = xzbbox.max_z();

    // Water bbox from the small land-cover grid, avoiding a full-world scan.
    let (wmin_x, wmin_z, wmax_x, wmax_z) = ground.lc_water_block_bounds()?;
    let wmin_x = min_x + wmin_x;
    let wmin_z = min_z + wmin_z;
    let wmax_x = min_x + wmax_x;
    let wmax_z = min_z + wmax_z;

    // Pad by one for the shore ring (clamped); keeps the DT identical to full-bbox.
    let smin_x = (wmin_x - 1).max(min_x);
    let smax_x = (wmax_x + 1).min(max_x);
    let smin_z = (wmin_z - 1).max(min_z);
    let smax_z = (wmax_z + 1).min(max_z);
    let sw = (smax_x - smin_x + 1) as usize;
    let sh = (smax_z - smin_z + 1) as usize;
    // Cap cells to bound memory; above it, render flat water instead of crashing.
    match sw.checked_mul(sh) {
        Some(total) if total <= MAX_WATER_FIELD_CELLS => Some((smin_x, smin_z, sw, sh)),
        _ => {
            eprintln!("Warning: water area too large for depth carving; rendering flat water");
            None
        }
    }
}

/// Run a chamfer DT over the LC_WATER mask and bake per-cell carve depth.
pub fn compute_big_water_field(ground: &Ground, xzbbox: &XZBBox) -> BigWaterField {
    let Some((smin_x, smin_z, sw, sh)) = water_field_bounds(ground, xzbbox) else {
        return BigWaterField::empty();
    };
    let total = sw * sh;
    let min_x = xzbbox.min_x();
    let min_z = xzbbox.min_z();
    let smax_x = smin_x + sw as i32 - 1;
    let smax_z = smin_z + sh as i32 - 1;

    let is_lc_water =
        |x: i32, z: i32| ground.cover_class(XZPoint::new(x - min_x, z - min_z)) == LC_WATER;

    // Seed DT: water = DT_MAX (unreached), shore/land = 0.
    let mut dt = vec![0u8; total];
    for z in smin_z..=smax_z {
        let row = (z - smin_z) as usize * sw;
        for x in smin_x..=smax_x {
            if is_lc_water(x, z) {
                dt[row + (x - smin_x) as usize] = DT_MAX;
            }
        }
    }
    chamfer_3_4_dt(&mut dt, sw, sh);

    // Per-component BFS for the max DT, then a second walk bakes each cell's depth.
    // Levels are held in swapped frontiers, so residency is the perimeter, not the body.
    let mut depth = vec![0u8; total.div_ceil(2)];
    let mut visited = vec![0u64; total.div_ceil(64)];
    let mut baked = vec![0u64; total.div_ceil(64)];
    let mut cur: Vec<u32> = Vec::new();
    let mut next: Vec<u32> = Vec::new();
    for start in 0..total {
        if dt[start] == 0 || bit_get(&visited, start) {
            continue;
        }
        let mut comp_max = 0u8;
        bit_set(&mut visited, start);
        cur.clear();
        cur.push(start as u32);
        while !cur.is_empty() {
            next.clear();
            for &c in &cur {
                let idx = c as usize;
                comp_max = comp_max.max(dt[idx]);
                for_each_neighbor(idx, sw, sh, |n| {
                    if dt[n] != 0 && !bit_get(&visited, n) {
                        bit_set(&mut visited, n);
                        next.push(n as u32);
                    }
                });
            }
            std::mem::swap(&mut cur, &mut next);
        }

        let cm = u16::from(comp_max);
        bit_set(&mut baked, start);
        cur.push(start as u32);
        while !cur.is_empty() {
            next.clear();
            for &c in &cur {
                let idx = c as usize;
                let x = smin_x + (idx % sw) as i32;
                let z = smin_z + (idx / sw) as i32;
                let d = ocean_depth_for_cell(x, z, u16::from(dt[idx]), cm);
                nibble_set(&mut depth, idx, d as u8);
                for_each_neighbor(idx, sw, sh, |n| {
                    if dt[n] != 0 && !bit_get(&baked, n) {
                        bit_set(&mut baked, n);
                        next.push(n as u32);
                    }
                });
            }
            std::mem::swap(&mut cur, &mut next);
        }
        cur.shrink_to(FRONTIER_KEEP);
        next.shrink_to(FRONTIER_KEEP);
    }

    BigWaterField {
        depth,
        bathymetry_depth_blocks: None,
        width: sw,
        height: sh,
        min_x: smin_x,
        min_z: smin_z,
    }
}

/// Builds the water sub-rectangle with synthetic shoreline depth left at zero.
/// This keeps water flat when legacy depth is disabled while still giving the
/// bathymetry override pass a frame to write into.
pub fn compute_big_water_field_without_legacy(ground: &Ground, xzbbox: &XZBBox) -> BigWaterField {
    let Some((min_x, min_z, width, height)) = water_field_bounds(ground, xzbbox) else {
        return BigWaterField::empty();
    };
    BigWaterField {
        depth: vec![0u8; (width * height).div_ceil(2)],
        bathymetry_depth_blocks: None,
        width,
        height,
        min_x,
        min_z,
    }
}

/// Computes the ordinary water field, then overrides cells with valid bathymetry
/// samples. Cells outside the supplied grid or marked NoData retain the existing
/// distance-transform result, so partial datasets can be used safely.
pub fn compute_big_water_field_with_bathymetry(
    ground: &Ground,
    xzbbox: &XZBBox,
    bathymetry: &BathymetryGrid,
    compression: NonlinearDepthCompression,
) -> BigWaterField {
    let mut field = compute_big_water_field(ground, xzbbox);
    apply_bathymetry_overrides(&mut field, ground, xzbbox, bathymetry, compression);
    field
}

/// Bathymetry-only water field: the synthetic shoreline base is flat, so cells
/// outside the J-EGG500 data stay flat instead of using the legacy estimate.
pub fn compute_big_water_field_with_bathymetry_only(
    ground: &Ground,
    xzbbox: &XZBBox,
    bathymetry: &BathymetryGrid,
    compression: NonlinearDepthCompression,
) -> BigWaterField {
    let mut field = compute_big_water_field_without_legacy(ground, xzbbox);
    apply_bathymetry_overrides(&mut field, ground, xzbbox, bathymetry, compression);
    field
}

fn apply_bathymetry_overrides(
    field: &mut BigWaterField,
    ground: &Ground,
    xzbbox: &XZBBox,
    bathymetry: &BathymetryGrid,
    compression: NonlinearDepthCompression,
) {
    let total = match field.width.checked_mul(field.height) {
        Some(total) if total > 0 => total,
        _ => return,
    };

    let mut overrides = vec![0u8; total];
    let off_x = xzbbox.min_x();
    let off_z = xzbbox.min_z();
    let mut any_override = false;
    for lz in 0..field.height {
        let z = field.min_z + lz as i32;
        for lx in 0..field.width {
            let x = field.min_x + lx as i32;
            let local = XZPoint::new(x - off_x, z - off_z);
            if ground.cover_class(local) != LC_WATER {
                continue;
            }
            if let Some(depth_m) = bathymetry.sample_depth_m(x, z) {
                let compressed = compression.compress_meters(depth_m);
                if compressed > 0 {
                    overrides[lz * field.width + lx] = compressed;
                    any_override = true;
                }
            }
        }
    }

    if any_override {
        field.bathymetry_depth_blocks = Some(overrides);
    }
}

/// In-place two-sweep chamfer-3-4 DT. Input: 0 = shore, `DT_MAX` = water seed.
fn chamfer_3_4_dt(d: &mut [u8], w: usize, h: usize) {
    let step = |v: u8, add: u8| v.saturating_add(add);
    for j in 0..h {
        for i in 0..w {
            let idx = j * w + i;
            if d[idx] == 0 {
                continue;
            }
            let mut best = d[idx];
            if i > 0 {
                best = best.min(step(d[idx - 1], 3));
            }
            if j > 0 {
                best = best.min(step(d[idx - w], 3));
                if i > 0 {
                    best = best.min(step(d[idx - w - 1], 4));
                }
                if i + 1 < w {
                    best = best.min(step(d[idx - w + 1], 4));
                }
            }
            d[idx] = best;
        }
    }
    for j in (0..h).rev() {
        for i in (0..w).rev() {
            let idx = j * w + i;
            if d[idx] == 0 {
                continue;
            }
            let mut best = d[idx];
            if i + 1 < w {
                best = best.min(step(d[idx + 1], 3));
            }
            if j + 1 < h {
                best = best.min(step(d[idx + w], 3));
                if i > 0 {
                    best = best.min(step(d[idx + w - 1], 4));
                }
                if i + 1 < w {
                    best = best.min(step(d[idx + w + 1], 4));
                }
            }
            d[idx] = best;
        }
    }
}

/// Width-tiered max carve depth. Bigger bodies reach a deeper floor.
#[inline]
fn polygon_local_max(component_max_units: u16) -> i32 {
    if component_max_units < 21 {
        2
    } else if component_max_units < 45 {
        3
    } else if component_max_units < 75 {
        4
    } else {
        6
    }
}

/// Depth from an effective DT: local_max * sqrt(dist/span), tier-clamped.
fn depth_from_dt(dt_eff: f64, component_max_units: u16) -> i32 {
    if dt_eff < f64::from(SHOAL_DT_UNITS) {
        return 0;
    }
    let local_max = polygon_local_max(component_max_units);
    let dist_blocks = (dt_eff - f64::from(SHOAL_DT_UNITS)) / 3.0;
    // Sqrt curve: steep near shore, smooth plateau; kills the linear-band terracing.
    let span: f64 = if component_max_units < 21 {
        6.0
    } else if component_max_units < 45 {
        12.0
    } else if component_max_units < 75 {
        20.0
    } else {
        35.0
    };
    let t = (dist_blocks / span).clamp(0.0, 1.0);
    ((f64::from(local_max) * t.sqrt()).floor() as i32).clamp(0, local_max)
}

/// Per-cell carve depth, with deterministic contour wobble on the bank lines.
fn ocean_depth_for_cell(x: i32, z: i32, dt_units: u16, component_max_units: u16) -> i32 {
    if dt_units == 0 {
        return 0;
    }
    let wobble = (crate::ground_generation::value_noise_01(x, z, 12) - 0.5) * 4.0;
    depth_from_dt(f64::from(dt_units) + wobble, component_max_units)
}

/// Safe upper bound on a map's deepest carve, from the pre-repair water mask.
pub fn estimate_max_carve_depth(
    lc_grid: &[Vec<u8>],
    world_width: usize,
    world_height: usize,
) -> i32 {
    let gh = lc_grid.len();
    let gw = lc_grid.first().map_or(0, Vec::len);
    if gw == 0 || gh == 0 {
        return 0;
    }
    let mut dt = vec![0u8; gw * gh];
    let mut any_water = false;
    for (z, row) in lc_grid.iter().enumerate() {
        for (x, &c) in row.iter().enumerate() {
            if c == LC_WATER {
                dt[z * gw + x] = DT_MAX;
                any_water = true;
            }
        }
    }
    if !any_water {
        return 0;
    }
    chamfer_3_4_dt(&mut dt, gw, gh);
    let grid_max_dt = dt.iter().copied().max().unwrap_or(0);
    // Inclusive-span ratio, matching the grid<->block mapping elsewhere (safe upper bound).
    let wr = world_width.saturating_sub(1).max(1) as f64 / gw.saturating_sub(1).max(1) as f64;
    let hr = world_height.saturating_sub(1).max(1) as f64 / gh.saturating_sub(1).max(1) as f64;
    let block_max_dt = f64::from(grid_max_dt) * wr.max(hr).max(1.0);
    let comp_max = block_max_dt.min(f64::from(u16::MAX)) as u16;
    depth_from_dt(block_max_dt + 2.0, comp_max)
}

/// Place the underwater stack: water column, a layered bed, dunes and vegetation.
pub fn carve_water_column(
    editor: &mut WorldEditor,
    x: i32,
    z: i32,
    water_y: i32,
    depth: i32,
    road_mask: &RoadMaskBitmap,
    bwf: &BigWaterField,
) {
    debug_assert!(
        depth <= MAX_WATER_DEPTH,
        "water carve depth {depth} exceeds the max tier {MAX_WATER_DEPTH}"
    );
    // Clamp to the valid tier range, and keep the bed above bedrock.
    let depth = depth
        .clamp(0, MAX_WATER_DEPTH)
        .min((water_y - min_y() - 2).max(0));
    for dy in 0..=depth {
        editor.set_block_absolute(WATER, x, water_y - dy, z, None, Some(&[]));
    }
    clear_stranded_vegetation(editor, x, z, water_y);
    let bed_y = water_y - depth - 1;

    // Keep the bed plain near causeways so blobs/dunes/veg don't clutter piers.
    let near_bridge = depth >= 2 && !road_mask.is_empty() && bridge_adjacent(road_mask, x, z);

    // Layered bed: gravel base with noise-driven sand/clay/dirt patches over stone.
    let (top_block, under_block) = match depth {
        0..=1 => (SAND, SANDSTONE),
        2..=6 if near_bridge => {
            if crate::ground_generation::value_noise_01(x, z, 6) < 0.4 && depth <= 3 {
                (SAND, STONE)
            } else {
                (GRAVEL, STONE)
            }
        }
        2..=6 => {
            // Coherent jitter breaks concentric depth rings as patches, not speckle.
            let jn = crate::ground_generation::value_noise_01(x + 7, z + 13, 22);
            let jitter = if jn < 0.34 {
                -1
            } else if jn > 0.66 {
                1
            } else {
                0
            };
            let d = (depth + jitter).max(1);
            // Domain-warp the sample coords so patches read organic, not circular.
            let warp_x = crate::ground_generation::value_noise_01(x + 901, z + 33, 52);
            let warp_z = crate::ground_generation::value_noise_01(x + 17, z + 811, 52);
            let wx = x + ((warp_x - 0.5) * 28.0) as i32;
            let wz = z + ((warp_z - 0.5) * 28.0) as i32;
            // Per-block noise, sampled lazily so most cells skip the rare tiers.
            let vn = |dx, dz, s| crate::ground_generation::value_noise_01(wx + dx, wz + dz, s);
            let top = if d <= 1 {
                SAND
            } else if d == 2 {
                if vn(53, 97, 56) > 0.50 {
                    SAND
                } else {
                    GRAVEL
                }
            } else if d >= 5 && vn(727, 911, 8) > 0.96 {
                SOUL_SAND
            } else if vn(73, 109, 64) > 0.74 {
                CLAY
            } else if vn(53, 97, 56) > 0.81 {
                SAND
            } else if vn(211, 41, 44) > 0.88 {
                DIRT
            } else if vn(311, 17, 50) > 0.90 {
                COARSE_DIRT
            } else {
                GRAVEL
            };
            (top, STONE)
        }
        _ => (GRAVEL, STONE),
    };

    if bed_y > min_y() {
        editor.set_block_absolute(top_block, x, bed_y, z, None, Some(&[]));
    }
    // Supports the gravity-affected bed; dropped onto bedrock at the lowest carve.
    if bed_y - 1 > min_y() {
        editor.set_block_absolute(under_block, x, bed_y - 1, z, None, Some(&[]));
    }
    // Backfill stone so neighbour side-faces never expose air under varied beds.
    let fill_to = (bed_y - 2).max(min_y() + 1);
    let fill_from = (bed_y - 12).max(min_y() + 1);
    if fill_from <= fill_to {
        editor.fill_column_absolute(STONE, x, z, fill_from, fill_to, true);
    }

    // Dunes return their crest so veg plants on top instead of inside them.
    // 7x7 body max is sampled lazily; depth 1 always yields amp 0, skip it too.
    let bump = if depth >= 2 && !near_bridge {
        let amp = dune_amp(bwf.body_max_7x7(x, z), depth);
        place_underwater_dunes(editor, x, z, water_y, bed_y, amp, top_block)
    } else {
        0
    };
    if depth >= 3 && !near_bridge {
        place_underwater_vegetation(editor, x, z, water_y, bed_y + bump, depth);
    }
}

/// Loose plants scattered at ground+1, so removing one strands nothing else.
const SURFACE_VEGETATION: &[Block] = &[
    GRASS,
    TALL_GRASS_BOTTOM,
    TALL_GRASS_TOP,
    FERN,
    LARGE_FERN_LOWER,
    LARGE_FERN_UPPER,
    DEAD_BUSH,
    RED_FLOWER,
    YELLOW_FLOWER,
    BLUE_FLOWER,
    WHITE_FLOWER,
    OAK_LEAVES,
];

/// Strip plants left floating on a freshly carved water surface, leaving piers
/// alone. Reading first avoids filling a section with air per water cell.
fn clear_stranded_vegetation(editor: &mut WorldEditor, x: i32, z: i32, water_y: i32) {
    for y in water_y + 1..=water_y + 2 {
        if editor.get_block_absolute(x, y, z).is_some() {
            editor.set_block_absolute(AIR, x, y, z, Some(SURFACE_VEGETATION), None);
        }
    }
    // A trunk rooted on the new surface has to go whole. Trees are placed
    // before the polygon that floods them, and the land-cover water mask is
    // coarser than the carve, so the guards on the tree paths can miss it.
    if editor
        .get_block_absolute(x, water_y + 1, z)
        .is_some_and(is_trunk)
    {
        clear_tree_from(editor, x, water_y + 1, z);
    }
}

fn is_trunk(block: Block) -> bool {
    let name = block.name();
    name.ends_with("_log") || name.ends_with("_stem")
}

fn is_tree_part(block: Block) -> bool {
    is_trunk(block) || block.name().ends_with("_leaves")
}

/// Largest tree the flood will take out. A giant schematic is well under this,
/// and the cap stops a canopy that touches its neighbour from unzipping a wood.
const MAX_TREE_BLOCKS: usize = 4096;

/// Erase the tree connected to (x, y, z). Clearing only the two plant layers
/// would leave the canopy hanging in the air over the water. Blocks are set to
/// air as they are visited, so a revisit sees air and stops; no visited set.
fn clear_tree_from(editor: &mut WorldEditor, x: i32, y: i32, z: i32) {
    let mut stack = vec![(x, y, z)];
    let mut cleared = 0usize;
    while let Some((cx, cy, cz)) = stack.pop() {
        if cleared >= MAX_TREE_BLOCKS {
            break;
        }
        if !editor
            .get_block_absolute(cx, cy, cz)
            .is_some_and(is_tree_part)
        {
            continue;
        }
        editor.set_block_absolute(AIR, cx, cy, cz, None, Some(&[]));
        cleared += 1;
        for (dx, dy, dz) in [
            (1, 0, 0),
            (-1, 0, 0),
            (0, 1, 0),
            (0, -1, 0),
            (0, 0, 1),
            (0, 0, -1),
        ] {
            stack.push((cx + dx, cy + dy, cz + dz));
        }
    }
}

/// True if any cell in the 5x5 area around (x, z), center excluded, carries a road/bridge.
fn bridge_adjacent(road_mask: &RoadMaskBitmap, x: i32, z: i32) -> bool {
    for dz in -2..=2 {
        for dx in -2..=2 {
            if (dx != 0 || dz != 0) && road_mask.contains(x + dx, z + dz) {
                return true;
            }
        }
    }
    false
}

/// Dune amplitude: target keyed off body width (7x7 BWF max), capped below the surface.
fn dune_amp(body_max: i32, depth: i32) -> i32 {
    // Legacy shoreline-distance depths top out at 6; bathymetry overrides may be deeper.
    let target = match body_max {
        ..=3 => 2,
        4..=5 => 3,
        _ => 4,
    };
    target.min((depth - 1).max(0))
}

/// Width-aware multi-octave dunes 1-4 blocks tall on the bed. Returns the
/// placed crest height. Ported from the Teddy fork.
fn place_underwater_dunes(
    editor: &mut WorldEditor,
    x: i32,
    z: i32,
    water_y: i32,
    bed_y: i32,
    amp: i32,
    bed_block: Block,
) -> i32 {
    if amp <= 0 {
        return 0;
    }
    let warp_x = crate::ground_generation::value_noise_01(x + 901, z + 33, 50);
    let warp_z = crate::ground_generation::value_noise_01(x + 17, z + 811, 50);
    let wx = x + ((warp_x - 0.5) * 30.0) as i32;
    let wz = z + ((warp_z - 0.5) * 30.0) as i32;
    let n_large = crate::ground_generation::value_noise_01(wx + 113, wz + 257, 44) as f32;
    let n_med = crate::ground_generation::value_noise_01(wx + 31, wz + 71, 18) as f32;
    let n_sharp = crate::ground_generation::value_noise_01(wx + 7, wz + 11, 10) as f32;
    let h_f = 0.40 * n_large + 0.30 * n_med + 0.30 * n_sharp;
    if h_f < 0.28 {
        return 0;
    }
    let t = (h_f - 0.28) / 0.72;
    let bump = ((t.powf(0.45) * (amp as f32 + 0.99)).floor() as i32).clamp(1, amp);
    for dy in 1..=bump {
        let y = bed_y + dy;
        if y >= water_y {
            return dy - 1;
        }
        editor.set_block_absolute(bed_block, x, y, z, None, Some(&[]));
    }
    bump
}

/// Clumped underwater vegetation on a carved bed: SEAGRASS/TALL_SEAGRASS/SEA_PICKLE
/// meadows, with rarer KELP columns in deeper water. Ported from the Teddy fork.
fn place_underwater_vegetation(
    editor: &mut WorldEditor,
    x: i32,
    z: i32,
    water_y: i32,
    bed_top: i32,
    depth: i32,
) {
    // Two decorrelated noise fields gate clumpy meadows instead of pepper-shot cells.
    let field_noise = crate::ground_generation::value_noise_01(x + 53, z + 89, 30);
    let cluster_noise = crate::ground_generation::value_noise_01(x + 401, z + 17, 10);
    let combined = field_noise * cluster_noise;

    // KELP: deep cells where both noises ring high; tip caps the column.
    let kelp_pick = (crate::land_cover::coord_hash(x + 91, z + 41) % 100) as i32;
    if depth >= 4 && field_noise > 0.78 && cluster_noise > 0.80 && kelp_pick < 25 {
        let plant_top_full = water_y - 1;
        let plant_bottom = bed_top + 1;
        let avail = plant_top_full - plant_bottom;
        if avail >= 3 {
            let hgt_pick = (crate::land_cover::coord_hash(x + 211, z + 503) % 100) as i32;
            let share = 30 + hgt_pick * 70 / 100;
            let used = ((avail as i64 * share as i64) / 100) as i32;
            let used = used.max(3).min(avail);
            let plant_top = plant_bottom + used;
            // Replace water, never air, so plants don't grow into the bed or float.
            for y in plant_bottom..plant_top {
                editor.set_block_absolute(KELP_PLANT, x, y, z, None, Some(&[AIR]));
            }
            editor.set_block_absolute(KELP, x, plant_top, z, None, Some(&[AIR]));
        }
        return;
    }

    // Seagrass meadow mix: 50% SEAGRASS, 35% TALL_SEAGRASS, 15% SEA_PICKLE.
    if combined > 0.42 {
        let dropout = crate::land_cover::coord_hash(x + 17, z + 31) % 10;
        if dropout < 4 {
            let plant_y = bed_top + 1;
            if plant_y < water_y {
                let pick = (crate::land_cover::coord_hash(x + 211, z + 73) % 100) as i32;
                if pick < 50 {
                    editor.set_block_absolute(SEAGRASS, x, plant_y, z, None, Some(&[AIR]));
                } else if pick < 85 && plant_y + 1 < water_y {
                    editor.set_block_absolute(
                        TALL_SEAGRASS_BOTTOM,
                        x,
                        plant_y,
                        z,
                        None,
                        Some(&[AIR]),
                    );
                    editor.set_block_absolute(
                        TALL_SEAGRASS_TOP,
                        x,
                        plant_y + 1,
                        z,
                        None,
                        Some(&[AIR]),
                    );
                } else {
                    editor.set_block_absolute(SEA_PICKLE, x, plant_y, z, None, Some(&[AIR]));
                }
            }
        }
    }
}

/// Post-pass carving every LC_WATER cell, so ESA-only water gets depth too.
pub fn carve_lc_water_pass(
    editor: &mut WorldEditor,
    ground: &Ground,
    xzbbox: &XZBBox,
    bwf: &BigWaterField,
    road_mask: &RoadMaskBitmap,
    tunnel_footprint: &RoadMaskBitmap,
) {
    let x1 = bwf.min_x + bwf.width as i32 - 1;
    let z1 = bwf.min_z + bwf.height as i32 - 1;
    carve_lc_water_region(
        editor,
        ground,
        xzbbox,
        bwf,
        road_mask,
        tunnel_footprint,
        bwf.min_x,
        x1,
        bwf.min_z,
        z1,
    );
}

/// `carve_lc_water_pass` restricted to the inclusive `[iter_min..=iter_max]` block
/// range (intersected with the water sub-rect). Per-tile callers pass strict tile
/// bounds; writes are vertical-only so output is identical regardless of tiling.
#[allow(clippy::too_many_arguments)]
pub fn carve_lc_water_region(
    editor: &mut WorldEditor,
    ground: &Ground,
    xzbbox: &XZBBox,
    bwf: &BigWaterField,
    road_mask: &RoadMaskBitmap,
    tunnel_footprint: &RoadMaskBitmap,
    iter_min_x: i32,
    iter_max_x: i32,
    iter_min_z: i32,
    iter_max_z: i32,
) {
    let off_x = xzbbox.min_x();
    let off_z = xzbbox.min_z();
    // Only the water sub-rect can hold LC_WATER cells; intersect it with the range.
    let x0 = bwf.min_x.max(iter_min_x);
    let x1 = (bwf.min_x + bwf.width as i32 - 1).min(iter_max_x);
    let z0 = bwf.min_z.max(iter_min_z);
    let z1 = (bwf.min_z + bwf.height as i32 - 1).min(iter_max_z);
    for z in z0..=z1 {
        for x in x0..=x1 {
            // Keep road/bridge surfaces (causeways, decks); never carve into a tunnel bore.
            if road_mask.contains(x, z) || tunnel_footprint.contains(x, z) {
                continue;
            }
            let coord = XZPoint::new(x - off_x, z - off_z);
            if ground.cover_class(coord) != LC_WATER {
                continue;
            }
            let mut water_y = ground.water_level(coord);
            let ground_y = editor.get_ground_level(x, z);
            if ground_y > water_y {
                // A bank cell the snap pulled below its terrain: skip. Interior water
                // above a dip is a step inside the body: keep it at its own level.
                if !ground.is_interior_water(coord) {
                    continue;
                }
                water_y = ground_y;
            }
            carve_water_column(editor, x, z, water_y, bwf.depth_at(x, z), road_mask, bwf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dt_distance_from_shore() {
        let (w, h) = (7usize, 7usize);
        let mut d = vec![0u8; w * h];
        for j in 1..6 {
            for i in 1..6 {
                d[j * w + i] = DT_MAX;
            }
        }
        chamfer_3_4_dt(&mut d, w, h);
        assert_eq!(d[0], 0, "shore stays 0");
        assert_eq!(d[w + 1], 3, "edge water is one ortho step from shore");
        assert_eq!(d[3 * w + 3], 9, "centre is three ortho steps in");
    }

    #[test]
    fn depth_zero_inside_shoal_and_on_land() {
        assert_eq!(ocean_depth_for_cell(0, 0, 0, 0), 0);
        assert_eq!(ocean_depth_for_cell(10, 10, 3, 200), 0);
    }

    #[test]
    fn depth_clamps_to_tier_max() {
        assert_eq!(ocean_depth_for_cell(5, 5, u16::from(DT_MAX), 200), 6);
        assert_eq!(polygon_local_max(10), 2);
        assert_eq!(polygon_local_max(60), 4);
        assert_eq!(polygon_local_max(100), 6);
    }

    #[test]
    fn sqrt_depth_monotonic_capped_no_cliffs() {
        for cm in [10u16, 30, 60, 200] {
            let lm = polygon_local_max(cm);
            let mut prev = 0;
            for dt in 0..=300u32 {
                let d = depth_from_dt(f64::from(dt), cm);
                assert!(d >= prev, "non-monotonic at dt={dt} cm={cm}");
                assert!(d - prev <= 1, "step >1 at dt={dt} cm={cm}");
                assert!(d <= lm);
                prev = d;
            }
            assert_eq!(prev, lm, "far water reaches tier max for cm={cm}");
        }
    }

    #[test]
    fn nonlinear_bathymetry_compression_is_monotonic_and_capped() {
        let compression = NonlinearDepthCompression::default();
        assert_eq!(compression.compress_meters(0.0), 0);
        let values = [1.0, 10.0, 50.0, 100.0, 500.0, 1_000.0, 5_000.0];
        let compressed: Vec<u8> = values
            .iter()
            .map(|&depth| compression.compress_meters(depth))
            .collect();
        assert!(compressed.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(compressed
            .iter()
            .all(|&blocks| blocks <= compression.max_blocks));
        assert!(compressed[0] >= 1);
        assert_eq!(*compressed.last().unwrap(), compression.max_blocks);
    }

    #[test]
    fn bathymetry_grid_handles_bounds_and_nodata() {
        let grid = BathymetryGrid::new(vec![10.0, 20.0, f32::NAN, 0.0], 2, 2, 100, -20, 1)
            .unwrap()
            .with_interpolation(BathymetryInterpolation::Nearest);
        assert_eq!(grid.sample_depth_m(100, -20), Some(10.0));
        assert_eq!(grid.sample_depth_m(101, -20), Some(20.0));
        assert_eq!(grid.sample_depth_m(100, -19), None);
        assert_eq!(grid.sample_depth_m(101, -19), None);
        assert_eq!(grid.sample_depth_m(99, -20), None);
    }

    #[test]
    fn bathymetry_bilinear_interpolates_between_cell_centres() {
        let grid = BathymetryGrid::new(vec![10.0, 20.0, 30.0, 40.0], 2, 2, 0, 0, 10).unwrap();
        assert_eq!(grid.sample_depth_m(5, 5), Some(10.0), "first centre");
        assert_eq!(grid.sample_depth_m(15, 5), Some(20.0), "second centre");
        assert_eq!(grid.sample_depth_m(5, 15), Some(30.0), "third centre");
        assert_eq!(grid.sample_depth_m(15, 15), Some(40.0), "fourth centre");
        assert_eq!(
            grid.sample_depth_m(10, 5),
            Some(15.0),
            "horizontal midpoint"
        );
        assert_eq!(
            grid.sample_depth_m(10, 10),
            Some(25.0),
            "cell centre of four"
        );
        assert_eq!(grid.sample_depth_m(0, 0), Some(10.0), "edge clamps");
        assert_eq!(grid.sample_depth_m(19, 19), Some(40.0), "far edge clamps");
        assert_eq!(grid.sample_depth_m(-1, 0), None);
        assert_eq!(grid.sample_depth_m(20, 0), None);
    }

    #[test]
    fn bathymetry_bilinear_renormalises_missing_corners() {
        let grid =
            BathymetryGrid::new(vec![10.0, f32::NAN, f32::NAN, 40.0], 2, 2, 0, 0, 10).unwrap();
        assert_eq!(grid.sample_depth_m(5, 5), Some(10.0));
        assert_eq!(grid.sample_depth_m(15, 15), Some(40.0));
        assert_eq!(grid.sample_depth_m(10, 10), Some(25.0));
        assert_eq!(
            grid.sample_depth_m(10, 5),
            Some(10.0),
            "the missing corner must not dilute the valid one with zero"
        );

        let empty = BathymetryGrid::new(vec![f32::NAN; 4], 2, 2, 0, 0, 10).unwrap();
        assert_eq!(empty.sample_depth_m(5, 5), None);
    }

    #[test]
    fn rotated_bathymetry_samples_final_world_coordinates() {
        // 2x2 lattice with centres at (5,5), (15,5), (5,15), (15,15).
        // Rotating the world 90° clockwise around (10,10) moves those centres
        // to (15,5), (15,15), (5,5) and (5,15) respectively.
        let grid = BathymetryGrid::new(vec![10.0, 20.0, 30.0, 40.0], 2, 2, 0, 0, 10)
            .unwrap()
            .with_rotation(90.0, 10.0, 10.0);

        assert_eq!(grid.sample_depth_m(15, 5), Some(10.0));
        assert_eq!(grid.sample_depth_m(15, 15), Some(20.0));
        assert_eq!(grid.sample_depth_m(5, 5), Some(30.0));
        assert_eq!(grid.sample_depth_m(5, 15), Some(40.0));

        // The final envelope has corners that inverse-rotate outside the
        // pre-rotation lattice. They must be reported as NoData rather than
        // clamped to an edge value.
        assert_eq!(grid.sample_depth_m(0, 0), None);
    }

    #[test]
    fn rotated_bathymetry_handles_non_cardinal_angles() {
        let grid = BathymetryGrid::new(vec![10.0, 20.0, 30.0, 40.0], 2, 2, 0, 0, 10)
            .unwrap()
            .with_rotation(45.0, 10.0, 10.0);

        // (15, 5) rotated 45° clockwise around (10, 10) lands at about
        // (17, 10); inverse-rotating the integer point maps back near the
        // first row's cell centre, not one of the other quadrants.
        let depth = grid.sample_depth_m(17, 10).unwrap();
        assert!(
            (depth - 20.0).abs() < 0.5,
            "45° rotation mapped the sample to the wrong quadrant: {depth}"
        );
    }

    #[test]
    fn zero_rotation_leaves_bathymetry_unchanged() {
        let unrotated = BathymetryGrid::new(vec![10.0, 20.0], 2, 1, 0, 0, 10).unwrap();
        let rotated = BathymetryGrid::new(vec![10.0, 20.0], 2, 1, 0, 0, 10)
            .unwrap()
            .with_rotation(0.0, 10.0, 5.0);

        for x in 0..20 {
            assert_eq!(unrotated.sample_depth_m(x, 5), rotated.sample_depth_m(x, 5));
        }
    }

    #[test]
    fn bathymetry_bilinear_softens_cell_boundary_step() {
        let grid = BathymetryGrid::new(vec![10.0, 30.0], 2, 1, 0, 0, 10).unwrap();
        let left = grid.sample_depth_m(9, 0).unwrap();
        let edge = grid.sample_depth_m(10, 0).unwrap();
        let right = grid.sample_depth_m(11, 0).unwrap();
        assert!(
            left < edge && edge < right,
            "expected a ramp: {left}, {edge}, {right}"
        );
        assert!((edge - left).abs() <= 2.0);
        assert!((right - edge).abs() <= 2.0);

        let nearest = BathymetryGrid::new(vec![10.0, 30.0], 2, 1, 0, 0, 10)
            .unwrap()
            .with_interpolation(BathymetryInterpolation::Nearest);
        assert_eq!(nearest.sample_depth_m(9, 0), Some(10.0));
        assert_eq!(nearest.sample_depth_m(10, 0), Some(30.0));
    }

    #[test]
    fn body_max_7x7_edge_safe() {
        let mut depth = vec![0u8; 8];
        nibble_set(&mut depth, 5, 6); // cell (1,1) in a 4x4 field
        let bwf = BigWaterField {
            depth,
            bathymetry_depth_blocks: None,
            width: 4,
            height: 4,
            min_x: 0,
            min_z: 0,
        };
        assert_eq!(
            bwf.body_max_7x7(0, 0),
            6,
            "corner window clips but finds (1,1)"
        );
        assert_eq!(bwf.body_max_7x7(-3, -3), 0, "mostly-outside window");
        assert_eq!(bwf.body_max_7x7(10, 10), 0, "fully outside");
        assert_eq!(BigWaterField::empty().body_max_7x7(5, 5), 0);
    }

    #[test]
    fn estimate_no_water_is_zero() {
        let grid = vec![vec![0u8; 16]; 16];
        assert_eq!(estimate_max_carve_depth(&grid, 16, 16), 0);
    }

    #[test]
    fn estimate_large_open_water_reaches_max() {
        let grid = vec![vec![LC_WATER; 32]; 32];
        assert_eq!(estimate_max_carve_depth(&grid, 32, 32), 6);
    }

    #[test]
    fn grid_span_covers_all_mapped_blocks() {
        // Brute-force every block that samples into grid cells [1,2]; the span must cover them.
        let (world_dim, grid_dim) = (100usize, 10usize);
        let (g_lo, g_hi) = (1usize, 2usize);
        let (lo, hi) = grid_span_to_block_span(g_lo, g_hi, world_dim, grid_dim);
        for bx in 0..world_dim {
            let gx =
                ((bx as f64 / (world_dim - 1) as f64) * (grid_dim - 1) as f64).round() as usize;
            if gx >= g_lo && gx <= g_hi {
                assert!(
                    bx as i32 >= lo && bx as i32 <= hi,
                    "block {bx} (grid {gx}) not covered by span [{lo},{hi}]"
                );
            }
        }
    }

    #[test]
    fn dune_amp_keys_off_body_max_capped_by_depth() {
        assert_eq!(dune_amp(2, 1), 0);
        assert_eq!(dune_amp(3, 6), 2);
        assert_eq!(dune_amp(5, 6), 3);
        assert_eq!(dune_amp(6, 2), 1);
        assert_eq!(dune_amp(6, 6), 4);
        assert_eq!(dune_amp(0, 0), 0);
    }

    #[test]
    fn bridge_adjacent_detects_neighborhood() {
        let bbox = XZBBox::rect_from_min_max(0, 0, 31, 31).unwrap();
        let mut mask = RoadMaskBitmap::new(&bbox);
        mask.set(12, 10);
        assert!(
            bridge_adjacent(&mask, 10, 10),
            "two cells away is inside the 5x5 area"
        );
        assert!(
            !bridge_adjacent(&mask, 15, 15),
            "three cells away is outside"
        );
        assert!(
            !bridge_adjacent(&mask, 12, 10),
            "the road cell itself is excluded"
        );
        let empty = RoadMaskBitmap::new(&bbox);
        assert!(!bridge_adjacent(&empty, 10, 10));
    }

    #[test]
    fn nibble_round_trip() {
        let mut buf = vec![0u8; 4];
        for (i, v) in [0u8, 6, 3, 1, 5, 2, 4, 0].iter().enumerate() {
            nibble_set(&mut buf, i, *v);
        }
        for (i, v) in [0u8, 6, 3, 1, 5, 2, 4, 0].iter().enumerate() {
            assert_eq!(nibble_get(&buf, i), *v);
        }
    }
}

#[cfg(test)]
mod water_field_traversal_tests {
    use super::*;
    use crate::ground::test_support::ground_with_land_cover_and_elevation;
    use crate::land_cover::LandCoverData;

    fn all_water_ground(side: usize) -> Ground {
        let grid = vec![vec![LC_WATER; side]; side];
        let (w, h) = (grid[0].len(), grid.len());
        let lc = LandCoverData {
            grid,
            water_distance: vec![vec![0u8; w]; h],
            water_blend_cache: once_cell::sync::OnceCell::new(),
            width: w,
            height: h,
            cells_per_meter: 1.0,
        };
        ground_with_land_cover_and_elevation(lc, w, h)
    }

    #[test]
    fn bathymetry_override_increases_depth_beyond_legacy_max() {
        let side = 48usize;
        let bbox = XZBBox::rect_from_min_max(0, 0, side as i32 - 1, side as i32 - 1).unwrap();
        let ground = all_water_ground(side);
        let legacy = compute_big_water_field(&ground, &bbox);
        assert!(legacy.depth_at(side as i32 / 2, side as i32 / 2) <= 6);

        // One big bathymetry cell covering the whole world, 1000 m deep.
        let bathymetry =
            BathymetryGrid::new(vec![1000.0f32], 1, 1, bbox.min_x(), bbox.min_z(), side).unwrap();
        let with_bathy = compute_big_water_field_with_bathymetry(
            &ground,
            &bbox,
            &bathymetry,
            NonlinearDepthCompression::default(),
        );
        let d = with_bathy.depth_at(side as i32 / 2, side as i32 / 2);
        assert!(d > 6, "bathymetry should exceed legacy max, got {d}");
        assert_eq!(
            d,
            NonlinearDepthCompression::default().compress_meters(1000.0) as i32
        );
    }

    #[test]
    fn bathymetry_missing_cells_fall_back_to_legacy() {
        let side = 48usize;
        let bbox = XZBBox::rect_from_min_max(0, 0, side as i32 - 1, side as i32 - 1).unwrap();
        let ground = all_water_ground(side);
        let legacy = compute_big_water_field(&ground, &bbox);

        // Bathymetry grid with valid data only in the first cell.
        let mut depths = vec![f32::NAN; 4];
        depths[0] = 50.0;
        let bathymetry = BathymetryGrid::new(depths, 2, 2, 0, 0, side / 2).unwrap();
        let with_bathy = compute_big_water_field_with_bathymetry(
            &ground,
            &bbox,
            &bathymetry,
            NonlinearDepthCompression::default(),
        );

        // First quadrant uses bathymetry.
        assert!(with_bathy.depth_at(5, 5) > legacy.depth_at(5, 5));
        // Opposite quadrant has no bathymetry and keeps legacy depth.
        assert_eq!(
            with_bathy.depth_at(side as i32 - 5, side as i32 - 5),
            legacy.depth_at(side as i32 - 5, side as i32 - 5)
        );
    }

    #[test]
    fn without_legacy_water_field_is_flat() {
        let side = 48usize;
        let bbox = XZBBox::rect_from_min_max(0, 0, side as i32 - 1, side as i32 - 1).unwrap();
        let ground = all_water_ground(side);
        let field = compute_big_water_field_without_legacy(&ground, &bbox);

        assert!(field.width > 0, "the water frame must still cover the body");
        for z in 0..side as i32 {
            for x in 0..side as i32 {
                assert_eq!(field.depth_at(x, z), 0, "flat water at ({x},{z})");
            }
        }
    }

    #[test]
    fn bathymetry_only_does_not_fall_back_to_legacy() {
        let side = 48usize;
        let bbox = XZBBox::rect_from_min_max(0, 0, side as i32 - 1, side as i32 - 1).unwrap();
        let ground = all_water_ground(side);
        let legacy = compute_big_water_field(&ground, &bbox);

        let mut depths = vec![f32::NAN; 4];
        depths[0] = 50.0;
        let bathymetry = BathymetryGrid::new(depths, 2, 2, 0, 0, side / 2).unwrap();
        let field = compute_big_water_field_with_bathymetry_only(
            &ground,
            &bbox,
            &bathymetry,
            NonlinearDepthCompression::default(),
        );

        assert_eq!(
            field.depth_at(5, 5),
            i32::from(NonlinearDepthCompression::default().compress_meters(50.0))
        );
        assert!(legacy.depth_at(36, 36) > 0);
        assert_eq!(
            field.depth_at(36, 36),
            0,
            "no-data cells must stay flat instead of using the legacy estimate"
        );
    }

    #[test]
    fn bathymetry_outside_water_subrect_is_ignored() {
        let side = 48usize;
        let bbox = XZBBox::rect_from_min_max(0, 0, side as i32 - 1, side as i32 - 1).unwrap();
        let ground = all_water_ground(side);

        // Bathymetry grid starts far outside the world.
        let bathymetry = BathymetryGrid::new(
            vec![1000.0f32],
            1,
            1,
            side as i32 + 100,
            side as i32 + 100,
            10,
        )
        .unwrap();
        let with_bathy = compute_big_water_field_with_bathymetry(
            &ground,
            &bbox,
            &bathymetry,
            NonlinearDepthCompression::default(),
        );
        let legacy = compute_big_water_field(&ground, &bbox);
        for z in 0..side as i32 {
            for x in 0..side as i32 {
                assert_eq!(
                    with_bathy.depth_at(x, z),
                    legacy.depth_at(x, z),
                    "outside bathymetry leaked at ({x},{z})"
                );
            }
        }
    }

    #[test]
    fn bathymetry_bilinear_blends_across_cell_boundary_in_the_field() {
        let side = 48usize;
        let bbox = XZBBox::rect_from_min_max(0, 0, side as i32 - 1, side as i32 - 1).unwrap();
        let ground = all_water_ground(side);
        let bathymetry =
            BathymetryGrid::new(vec![100.0, 1000.0, 100.0, 1000.0], 2, 2, 0, 0, side / 2).unwrap();
        let field = compute_big_water_field_with_bathymetry(
            &ground,
            &bbox,
            &bathymetry,
            NonlinearDepthCompression::default(),
        );

        let left = field.depth_at(12, 12);
        let edge = field.depth_at(24, 12);
        let right = field.depth_at(36, 12);
        assert_eq!(left, 22, "100 m at the first cell centre");
        assert_eq!(right, 77, "1000 m at the second cell centre");
        assert!(
            left < edge && edge < right,
            "the cell boundary must interpolate, got {left}, {edge}, {right}"
        );
    }

    const SIDE: usize = 48;

    fn ground_from(grid: Vec<Vec<u8>>) -> Ground {
        let (w, h) = (grid[0].len(), grid.len());
        let lc = LandCoverData {
            grid,
            water_distance: vec![vec![0u8; w]; h],
            water_blend_cache: once_cell::sync::OnceCell::new(),
            width: w,
            height: h,
            cells_per_meter: 1.0,
        };
        ground_with_land_cover_and_elevation(lc, w, h)
    }

    fn water_grid(cells: impl Fn(usize, usize) -> bool) -> Vec<Vec<u8>> {
        (0..SIDE)
            .map(|z| {
                (0..SIDE)
                    .map(|x| if cells(x, z) { LC_WATER } else { 10 })
                    .collect()
            })
            .collect()
    }

    /// Reference traversal: one growing index list per component, walked with a head
    /// pointer, with the depths baked once the whole component has been collected.
    fn single_pass_reference(
        ground: &Ground,
        xzbbox: &XZBBox,
    ) -> (Vec<u8>, usize, usize, i32, i32) {
        let (min_x, max_x) = (xzbbox.min_x(), xzbbox.max_x());
        let (min_z, max_z) = (xzbbox.min_z(), xzbbox.max_z());
        let (wmin_x, wmin_z, wmax_x, wmax_z) = match ground.lc_water_block_bounds() {
            Some((lx, lz, hx, hz)) => (min_x + lx, min_z + lz, min_x + hx, min_z + hz),
            None => return (Vec::new(), 0, 0, 0, 0),
        };
        let smin_x = (wmin_x - 1).max(min_x);
        let smax_x = (wmax_x + 1).min(max_x);
        let smin_z = (wmin_z - 1).max(min_z);
        let smax_z = (wmax_z + 1).min(max_z);
        let sw = (smax_x - smin_x + 1) as usize;
        let sh = (smax_z - smin_z + 1) as usize;
        let total = sw * sh;

        let mut dt = vec![0u8; total];
        for z in smin_z..=smax_z {
            let row = (z - smin_z) as usize * sw;
            for x in smin_x..=smax_x {
                if ground.cover_class(XZPoint::new(x - min_x, z - min_z)) == LC_WATER {
                    dt[row + (x - smin_x) as usize] = DT_MAX;
                }
            }
        }
        chamfer_3_4_dt(&mut dt, sw, sh);

        let mut depth = vec![0u8; total.div_ceil(2)];
        let mut visited = vec![0u64; total.div_ceil(64)];
        let mut comp: Vec<u32> = Vec::new();
        for start in 0..total {
            if dt[start] == 0 || bit_get(&visited, start) {
                continue;
            }
            comp.clear();
            comp.push(start as u32);
            bit_set(&mut visited, start);
            let mut comp_max = 0u8;
            let mut head = 0;
            while head < comp.len() {
                let idx = comp[head] as usize;
                head += 1;
                comp_max = comp_max.max(dt[idx]);
                let i = idx % sw;
                let j = idx / sw;
                let mut visit = |n: usize, comp: &mut Vec<u32>| {
                    if dt[n] != 0 && !bit_get(&visited, n) {
                        bit_set(&mut visited, n);
                        comp.push(n as u32);
                    }
                };
                if i > 0 {
                    visit(idx - 1, &mut comp);
                }
                if i + 1 < sw {
                    visit(idx + 1, &mut comp);
                }
                if j > 0 {
                    visit(idx - sw, &mut comp);
                }
                if j + 1 < sh {
                    visit(idx + sw, &mut comp);
                }
            }
            let cm = u16::from(comp_max);
            for &c in &comp {
                let idx = c as usize;
                let x = smin_x + (idx % sw) as i32;
                let z = smin_z + (idx / sw) as i32;
                let d = ocean_depth_for_cell(x, z, u16::from(dt[idx]), cm);
                nibble_set(&mut depth, idx, d as u8);
            }
        }
        (depth, sw, sh, smin_x, smin_z)
    }

    fn assert_matches_reference(label: &str, grid: Vec<Vec<u8>>) -> BigWaterField {
        let bbox = XZBBox::rect_from_min_max(0, 0, SIDE as i32 - 1, SIDE as i32 - 1).unwrap();
        let ground = ground_from(grid);
        let field = compute_big_water_field(&ground, &bbox);
        let (depth, sw, sh, smin_x, smin_z) = single_pass_reference(&ground, &bbox);
        assert_eq!((field.width, field.height), (sw, sh), "{label}: sub-rect");
        assert_eq!(
            (field.min_x, field.min_z),
            (smin_x, smin_z),
            "{label}: origin"
        );
        assert_eq!(field.depth, depth, "{label}: baked depths differ");
        assert!(
            depth.iter().any(|&b| b != 0),
            "{label}: no depth was carved, the comparison is vacuous"
        );
        field
    }

    #[test]
    fn frontier_walk_bakes_the_same_depths_as_the_single_pass_walk() {
        assert_matches_reference(
            "one open lake",
            water_grid(|x, z| (8..40).contains(&x) && (8..40).contains(&z)),
        );

        assert_matches_reference(
            "two components of different size",
            water_grid(|x, z| {
                ((2..22).contains(&x) && (2..22).contains(&z))
                    || ((30..44).contains(&x) && (30..44).contains(&z))
            }),
        );

        // Diagonal-only contact: 4-connectivity must keep these two components apart,
        // so each keeps its own comp_max and its own depth tier.
        assert_matches_reference(
            "diagonally touching bodies",
            water_grid(|x, z| {
                ((2..24).contains(&x) && (2..24).contains(&z))
                    || ((24..46).contains(&x) && (24..46).contains(&z))
            }),
        );

        // A ring around an island, plus a corridor: the re-walk has to reach every cell
        // through a shape where a level frontier and a head-pointer queue diverge in order.
        assert_matches_reference(
            "ring with an island and a corridor",
            water_grid(|x, z| {
                let ring = (4..36).contains(&x)
                    && (4..36).contains(&z)
                    && !((14..26).contains(&x) && (14..26).contains(&z));
                let corridor = (36..46).contains(&x) && (18..22).contains(&z);
                ring || corridor
            }),
        );
    }

    #[test]
    fn a_second_body_does_not_change_the_first_ones_depths() {
        let lone = assert_matches_reference(
            "lone lake",
            water_grid(|x, z| (2..22).contains(&x) && (2..22).contains(&z)),
        );
        let paired = assert_matches_reference(
            "same lake beside a larger one",
            water_grid(|x, z| {
                ((2..22).contains(&x) && (2..22).contains(&z))
                    || ((26..46).contains(&x) && (26..46).contains(&z))
            }),
        );
        for z in 2..22 {
            for x in 2..22 {
                assert_eq!(
                    lone.depth_at(x, z),
                    paired.depth_at(x, z),
                    "cell ({x},{z}) changed when an unrelated component was added"
                );
            }
        }
    }
}
