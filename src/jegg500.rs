//! J-EGG500 bathymetric data loader.
//!
//! J-EGG500 is a 500-metre mesh of water depths around Japan, distributed as
//! fixed-format text files named `mesh500_<lat>_<lon>.txt`. Each file covers a
//! 1° × 1° integer-degree tile. Every record contains:
//!
//!   kind (0 or 1), latitude (degrees), longitude (degrees), depth (metres)
//!
//! - kind 0: measured depth or depth derived from a contour line.
//! - kind 1: depth produced by interpolation.
//! - WGS-84 is used for horizontal coordinates.
//! - Depths are positive below the water datum; non-positive values are treated
//!   as land or missing data.
//!
//! The loader reads local files only. It does not download data automatically,
//! does not assume any API, and does not redistribute the raw dataset.

use crate::coordinate_system::cartesian::{XZBBox, XZPoint};
use crate::coordinate_system::geographic::LLBBox;
use crate::coordinate_system::transformation::CoordTransformer;
use crate::water_depth::{BathymetryGrid, BathymetryInterpolation, NonlinearDepthCompression};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Largest bathymetry grid this loader will allocate. Roughly limits resident
/// memory to a few hundred megabytes for the f32 depth buffer.
const MAX_BATHYMETRY_CELLS: usize = 128_000_000;

/// J-EGG500 mesh spacing in metres.
const MESH_SPACING_M: f64 = 500.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JeggSampleKind {
    Measured,
    Interpolated,
}

/// One raw J-EGG500 record.
#[derive(Debug, Clone)]
pub struct JeggSample {
    /// Sample origin: measured/contour-derived or interpolated.
    #[allow(dead_code)]
    pub kind: JeggSampleKind,
    pub lat: f64,
    pub lon: f64,
    pub depth_m: f32,
}

/// How J-EGG500 samples are mapped onto the Minecraft X/Z grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JeggInterpolation {
    /// Use the depth of the J-EGG500 cell that contains the Minecraft block.
    /// This matches the 500 m native resolution and avoids inventing values
    /// across data gaps, but leaves 500 m terraces between cells.
    NearestCell,
    /// Bilinearly blend between adjacent J-EGG500 cell centres. This removes
    /// the 500 m terraces while still returning the measured value at each
    /// cell centre; partial datasets fall back to their valid neighbours.
    #[default]
    Bilinear,
}

impl From<JeggInterpolation> for BathymetryInterpolation {
    fn from(value: JeggInterpolation) -> Self {
        match value {
            JeggInterpolation::NearestCell => BathymetryInterpolation::Nearest,
            JeggInterpolation::Bilinear => BathymetryInterpolation::Bilinear,
        }
    }
}

/// Configuration for the J-EGG500 loading step.
#[derive(Debug, Clone)]
pub struct Jegg500Config {
    /// Directory containing `mesh500_<lat>_<lon>.txt` files.
    pub base_dir: PathBuf,
    /// Interpolation method when sampling the J-EGG500 mesh.
    pub interpolation: JeggInterpolation,
    /// Optional override for the compression applied after loading. When `None`
    /// the default `NonlinearDepthCompression` is used.
    pub compression: Option<NonlinearDepthCompression>,
}

impl Jegg500Config {
    /// Sets the interpolation method. `Bilinear` is the default; `NearestCell`
    /// reproduces the pre-interpolation 500 m terraces.
    pub fn with_interpolation(mut self, interpolation: JeggInterpolation) -> Self {
        self.interpolation = interpolation;
        self
    }
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            interpolation: JeggInterpolation::default(),
            compression: None,
        }
    }
}

/// Parse one whitespace-separated J-EGG500 record.
///
/// The official Fortran format is `I1,F10.5,F10.5,I6`, but real files use
/// variable spacing. We split on whitespace and validate the four fields.
fn parse_jegg500_line(line: &str) -> Result<Option<JeggSample>, String> {
    let line = line.split('%').next().unwrap_or("");
    if line.trim().is_empty() {
        return Ok(None);
    }

    let fields: Vec<&str> = line.split_whitespace().collect();
    if fields.len() < 4 {
        return Err(format!("J-EGG500 line has too few fields: {line:?}"));
    }
    if fields.len() > 4 {
        return Err(format!("J-EGG500 line has too many fields: {line:?}"));
    }

    let kind = match fields[0] {
        "0" => JeggSampleKind::Measured,
        "1" => JeggSampleKind::Interpolated,
        _ => return Err(format!("J-EGG500 kind must be 0 or 1, got {:?}", fields[0])),
    };

    let lat: f64 = fields[1]
        .parse()
        .map_err(|e| format!("J-EGG500 latitude parse error in {line:?}: {e}"))?;
    let lon: f64 = fields[2]
        .parse()
        .map_err(|e| format!("J-EGG500 longitude parse error in {line:?}: {e}"))?;
    let depth: i32 = fields[3]
        .parse()
        .map_err(|e| format!("J-EGG500 depth parse error in {line:?}: {e}"))?;

    if !lat.is_finite() || !lon.is_finite() {
        return Err(format!("J-EGG500 non-finite coordinates in {line:?}"));
    }
    if lat < -90.0 || lat > 90.0 {
        return Err(format!("J-EGG500 latitude out of range: {lat}"));
    }
    if lon < -180.0 || lon > 180.0 {
        return Err(format!("J-EGG500 longitude out of range: {lon}"));
    }

    // Non-positive depths are land or NoData in this dataset.
    if depth <= 0 {
        return Ok(None);
    }

    Ok(Some(JeggSample {
        kind,
        lat,
        lon,
        depth_m: depth as f32,
    }))
}

/// Return the integer-degree tile names that overlap `llbbox`.
fn overlapping_tile_paths(base_dir: &Path, llbbox: &LLBBox) -> Vec<PathBuf> {
    let min_lat = llbbox.min().lat().floor() as i32;
    let max_lat = llbbox.max().lat().ceil() as i32;
    let min_lon = llbbox.min().lng().floor() as i32;
    let max_lon = llbbox.max().lng().ceil() as i32;

    let mut paths = Vec::new();
    for lat in min_lat..=max_lat {
        for lon in min_lon..=max_lon {
            paths.push(base_dir.join(format!("mesh500_{lat}_{lon}.txt")));
        }
    }
    paths
}

/// Read all samples from a single J-EGG500 tile file.
fn read_tile_file(path: &Path) -> Result<Vec<JeggSample>, String> {
    let file = File::open(path)
        .map_err(|e| format!("Failed to open J-EGG500 file {}: {e}", path.display()))?;
    let reader = BufReader::new(file);
    let mut samples = Vec::new();
    for (line_no, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| {
            format!(
                "Failed to read line {} of {}: {e}",
                line_no + 1,
                path.display()
            )
        })?;
        match parse_jegg500_line(&line) {
            Ok(Some(sample)) => samples.push(sample),
            Ok(None) => {}
            Err(e) => {
                return Err(format!("{} line {}: {e}", path.display(), line_no + 1));
            }
        }
    }
    Ok(samples)
}

/// Determine the Minecraft X/Z grid that covers `xzbbox` using the given
/// J-EGG500 cell size.
fn xzbbox_to_grid_bounds(
    xzbbox: &XZBBox,
    cell_size_blocks: usize,
) -> Result<(i32, i32, usize, usize), String> {
    if cell_size_blocks == 0 {
        return Err("J-EGG500 cell size must be non-zero".to_string());
    }

    let min_x = xzbbox.min_x();
    let min_z = xzbbox.min_z();
    let max_x = xzbbox.max_x();
    let max_z = xzbbox.max_z();

    let width_blocks = max_x
        .checked_sub(min_x)
        .and_then(|d| d.checked_add(1))
        .ok_or("J-EGG500 world X span overflow")? as usize;
    let height_blocks = max_z
        .checked_sub(min_z)
        .and_then(|d| d.checked_add(1))
        .ok_or("J-EGG500 world Z span overflow")? as usize;

    let width = width_blocks.div_ceil(cell_size_blocks).max(1);
    let height = height_blocks.div_ceil(cell_size_blocks).max(1);

    width
        .checked_mul(height)
        .filter(|&cells| cells <= MAX_BATHYMETRY_CELLS)
        .ok_or_else(|| {
            format!(
                "J-EGG500 bathymetry grid would be too large: {} cells (limit {})",
                width.saturating_mul(height),
                MAX_BATHYMETRY_CELLS
            )
        })?;

    Ok((min_x, min_z, width, height))
}

/// Map a J-EGG500 sample to a grid cell and accumulate the depth.
#[allow(clippy::too_many_arguments)]
fn accumulate_sample(
    sample: &JeggSample,
    transformer: &CoordTransformer,
    grid_origin_x: i32,
    grid_origin_z: i32,
    cell_size_blocks: usize,
    grid_width: usize,
    grid_height: usize,
    sums: &mut HashMap<(usize, usize), (f64, u32)>,
) {
    use crate::coordinate_system::geographic::LLPoint;
    let llpoint = match LLPoint::new(sample.lat, sample.lon) {
        Ok(p) => p,
        Err(_) => return,
    };
    let XZPoint { x, z } = transformer.transform_point(llpoint);

    let lx = i64::from(x) - i64::from(grid_origin_x);
    let lz = i64::from(z) - i64::from(grid_origin_z);
    if lx < 0 || lz < 0 {
        return;
    }
    let gx = lx as usize / cell_size_blocks;
    let gz = lz as usize / cell_size_blocks;
    if gx >= grid_width || gz >= grid_height {
        return;
    }

    let entry = sums.entry((gx, gz)).or_insert((0.0, 0));
    entry.0 += f64::from(sample.depth_m);
    entry.1 += 1;
}

/// Build a bathymetry grid from local J-EGG500 files.
///
/// The lattice is built in **pre-rotation** world coordinates. When
/// `rotation_degrees` is non-zero, the returned grid carries the matching
/// rotation, so callers can still sample it with final world X/Z. This keeps
/// the 500 m source lattice intact instead of binning rotated samples into an
/// axis-aligned grid, which would leave holes at some angles.
///
/// Returns `Ok(None)` when no data directory was supplied, the directory does
/// not exist, or no overlapping files contain valid samples. This lets the
/// caller fall back to the legacy shoreline-distance water depth.
pub fn load_jegg500_for_bbox(
    config: &Jegg500Config,
    llbbox: &LLBBox,
    scale: f64,
    rotation_degrees: f64,
) -> Result<Option<BathymetryGrid>, String> {
    if !config.base_dir.exists() {
        return Ok(None);
    }
    if !config.base_dir.is_dir() {
        return Err(format!(
            "J-EGG500 path is not a directory: {}",
            config.base_dir.display()
        ));
    }

    let interpolation = BathymetryInterpolation::from(config.interpolation);

    let (transformer, source_xzbbox) = CoordTransformer::llbbox_to_xzbbox(llbbox, scale)
        .map_err(|e| format!("J-EGG500 coordinate transformation failed: {e}"))?;

    // A 500 m J-EGG500 cell maps to this many Minecraft blocks.
    let cell_size_blocks = (MESH_SPACING_M * scale)
        .round()
        .max(1.0)
        .min(usize::MAX as f64) as usize;

    let (grid_origin_x, grid_origin_z, grid_width, grid_height) =
        xzbbox_to_grid_bounds(&source_xzbbox, cell_size_blocks)?;

    let mut sums: HashMap<(usize, usize), (f64, u32)> = HashMap::new();
    let mut any_file = false;
    let mut any_sample = false;
    let mut files_read = 0usize;
    let mut samples_read = 0usize;
    let tile_paths = overlapping_tile_paths(&config.base_dir, llbbox);

    for path in &tile_paths {
        if !path.exists() {
            continue;
        }
        any_file = true;
        let tile_samples = read_tile_file(path)?;
        samples_read += tile_samples.len();
        files_read += 1;
        for sample in tile_samples {
            any_sample = true;
            accumulate_sample(
                &sample,
                &transformer,
                grid_origin_x,
                grid_origin_z,
                cell_size_blocks,
                grid_width,
                grid_height,
                &mut sums,
            );
        }
    }

    if !any_file {
        return Ok(None);
    }
    if !any_sample {
        return Ok(None);
    }

    let total = grid_width
        .checked_mul(grid_height)
        .ok_or("J-EGG500 grid dimension overflow")?;
    let mut depth_m = vec![f32::NAN; total];
    let mut populated_cells = 0usize;
    for (gz, gx) in (0..grid_height).flat_map(|z| (0..grid_width).map(move |x| (z, x))) {
        if let Some((sum, count)) = sums.get(&(gx, gz)) {
            if *count > 0 {
                depth_m[gz * grid_width + gx] = (sum / f64::from(*count)) as f32;
                populated_cells += 1;
            }
        }
    }

    let rotation_center_x = (source_xzbbox.min_x() + source_xzbbox.max_x()) as f64 / 2.0;
    let rotation_center_z = (source_xzbbox.min_z() + source_xzbbox.max_z()) as f64 / 2.0;

    eprintln!(
        "J-EGG500: searched {} candidate tile(s), read {} file(s), \
         {} sample(s), populated {} grid cell(s) ({}x{} cells, {} block(s)/cell, \
         {:?} interpolation, {:.2}° rotation)",
        tile_paths.len(),
        files_read,
        samples_read,
        populated_cells,
        grid_width,
        grid_height,
        cell_size_blocks,
        config.interpolation,
        rotation_degrees,
    );

    Ok(Some(
        BathymetryGrid::new(
            depth_m,
            grid_width,
            grid_height,
            grid_origin_x,
            grid_origin_z,
            cell_size_blocks,
        )
        .map_err(|e| format!("J-EGG500 bathymetry grid creation failed: {e}"))?
        .with_interpolation(interpolation)
        .with_rotation(rotation_degrees, rotation_center_x, rotation_center_z),
    ))
}

/// Convenience: build a `BathymetryGrid` directly from an iterator of samples
/// already projected to Minecraft X/Z. Used by tests with synthetic data.
#[cfg(test)]
pub fn build_bathymetry_grid_from_samples(
    samples: &[JeggSample],
    transformer: &CoordTransformer,
    xzbbox: &XZBBox,
    scale: f64,
) -> Result<BathymetryGrid, String> {
    let cell_size_blocks = (MESH_SPACING_M * scale).max(1.0).min(usize::MAX as f64) as usize;
    let (grid_origin_x, grid_origin_z, grid_width, grid_height) =
        xzbbox_to_grid_bounds(xzbbox, cell_size_blocks)?;

    let mut sums: HashMap<(usize, usize), (f64, u32)> = HashMap::new();
    for sample in samples {
        accumulate_sample(
            sample,
            transformer,
            grid_origin_x,
            grid_origin_z,
            cell_size_blocks,
            grid_width,
            grid_height,
            &mut sums,
        );
    }

    let total = grid_width
        .checked_mul(grid_height)
        .ok_or("J-EGG500 grid dimension overflow")?;
    let mut depth_m = vec![f32::NAN; total];
    for (gz, gx) in (0..grid_height).flat_map(|z| (0..grid_width).map(move |x| (z, x))) {
        if let Some((sum, count)) = sums.get(&(gx, gz)) {
            if *count > 0 {
                depth_m[gz * grid_width + gx] = (sum / f64::from(*count)) as f32;
            }
        }
    }

    BathymetryGrid::new(
        depth_m,
        grid_width,
        grid_height,
        grid_origin_x,
        grid_origin_z,
        cell_size_blocks,
    )
    .map_err(|e| format!("Synthetic bathymetry grid creation failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinate_system::cartesian::XZBBox;
    use crate::coordinate_system::geographic::LLBBox;

    #[test]
    fn parse_valid_measured_line() {
        let s = parse_jegg500_line("0  34.07729 136.22777    24")
            .unwrap()
            .unwrap();
        assert_eq!(s.kind, JeggSampleKind::Measured);
        assert!((s.lat - 34.07729).abs() < 1e-5);
        assert!((s.lon - 136.22777).abs() < 1e-5);
        assert_eq!(s.depth_m, 24.0);
    }

    #[test]
    fn parse_valid_interpolated_line() {
        let s = parse_jegg500_line("1 34.0 136.0 100").unwrap().unwrap();
        assert_eq!(s.kind, JeggSampleKind::Interpolated);
        assert_eq!(s.depth_m, 100.0);
    }

    #[test]
    fn parse_non_positive_depth_is_nodata() {
        assert!(parse_jegg500_line("0 34.0 136.0 0").unwrap().is_none());
        assert!(parse_jegg500_line("0 34.0 136.0 -5").unwrap().is_none());
    }

    #[test]
    fn parse_empty_and_comment_lines() {
        assert!(parse_jegg500_line("").unwrap().is_none());
        assert!(parse_jegg500_line("   ").unwrap().is_none());
        assert!(parse_jegg500_line("% comment").unwrap().is_none());
    }

    #[test]
    fn parse_rejects_bad_kind() {
        assert!(parse_jegg500_line("2 34.0 136.0 10").is_err());
    }

    #[test]
    fn parse_rejects_malformed_numbers() {
        assert!(parse_jegg500_line("0 abc 136.0 10").is_err());
        assert!(parse_jegg500_line("0 34.0 136.0 xyz").is_err());
    }

    #[test]
    fn parse_rejects_out_of_range_coords() {
        assert!(parse_jegg500_line("0 91.0 136.0 10").is_err());
        assert!(parse_jegg500_line("0 34.0 181.0 10").is_err());
    }

    #[test]
    fn overlapping_tile_paths_cover_bbox() {
        let base = PathBuf::from("/tmp");
        let llbbox = LLBBox::new(34.0, 136.0, 35.5, 138.5).unwrap();
        let paths = overlapping_tile_paths(&base, &llbbox);
        assert!(paths.contains(&base.join("mesh500_34_136.txt")));
        assert!(paths.contains(&base.join("mesh500_35_138.txt")));
        assert!(!paths.contains(&base.join("mesh500_33_135.txt")));
    }

    #[test]
    fn config_builder_accepts_interpolation() {
        let config = Jegg500Config::new("/tmp").with_interpolation(JeggInterpolation::NearestCell);
        assert_eq!(config.interpolation, JeggInterpolation::NearestCell);
        assert_eq!(
            BathymetryInterpolation::from(config.interpolation),
            BathymetryInterpolation::Nearest
        );
    }

    #[test]
    fn config_defaults_to_bilinear_interpolation() {
        let config = Jegg500Config::new("/tmp");
        assert_eq!(config.interpolation, JeggInterpolation::Bilinear);
        assert_eq!(
            BathymetryInterpolation::from(config.interpolation),
            BathymetryInterpolation::Bilinear
        );
    }

    #[test]
    fn load_missing_directory_returns_none() {
        let config = Jegg500Config::new("/nonexistent/path/for/jegg500");
        let llbbox = LLBBox::new(34.0, 136.0, 35.0, 137.0).unwrap();
        assert!(load_jegg500_for_bbox(&config, &llbbox, 1.0, 0.0)
            .unwrap()
            .is_none());
    }

    #[test]
    fn xzbbox_to_grid_bounds_rounds_up() {
        let xzbbox = XZBBox::rect_from_min_max(0, 0, 99, 99).unwrap();
        let (ox, oz, w, h) = xzbbox_to_grid_bounds(&xzbbox, 50).unwrap();
        assert_eq!((ox, oz), (0, 0));
        assert_eq!((w, h), (2, 2));
    }

    #[test]
    fn xzbbox_to_grid_bounds_rejects_zero_cell_size() {
        let xzbbox = XZBBox::rect_from_min_max(0, 0, 10, 10).unwrap();
        assert!(xzbbox_to_grid_bounds(&xzbbox, 0).is_err());
    }

    #[test]
    fn synthetic_samples_project_to_expected_cells() {
        let llbbox = LLBBox::new(34.0, 136.0, 35.0, 137.0).unwrap();
        let (transformer, xzbbox) = CoordTransformer::llbbox_to_xzbbox(&llbbox, 1.0).unwrap();
        let samples = vec![JeggSample {
            kind: JeggSampleKind::Measured,
            lat: 34.5,
            lon: 136.5,
            depth_m: 42.0,
        }];
        let grid =
            build_bathymetry_grid_from_samples(&samples, &transformer, &xzbbox, 1.0).unwrap();
        // With scale 1.0 the 500 m cell is 500 blocks; the sample sits near the
        // middle of the world, so it lands in the first (and only) grid cell.
        let xz = transformer.transform_point(
            crate::coordinate_system::geographic::LLPoint::new(34.5, 136.5).unwrap(),
        );
        assert_eq!(grid.sample_depth_m(xz.x, xz.z), Some(42.0));
    }

    #[test]
    fn bathymetry_grid_nodata_outside_samples() {
        let llbbox = LLBBox::new(34.0, 136.0, 35.0, 137.0).unwrap();
        let (transformer, xzbbox) = CoordTransformer::llbbox_to_xzbbox(&llbbox, 1.0).unwrap();
        let samples = vec![JeggSample {
            kind: JeggSampleKind::Measured,
            lat: 34.1,
            lon: 136.1,
            depth_m: 10.0,
        }];
        let grid =
            build_bathymetry_grid_from_samples(&samples, &transformer, &xzbbox, 1.0).unwrap();
        let xz = transformer.transform_point(
            crate::coordinate_system::geographic::LLPoint::new(34.9, 136.9).unwrap(),
        );
        assert_eq!(grid.sample_depth_m(xz.x, xz.z), None);
    }

    #[test]
    fn compression_default_parameters_match_formula() {
        let c = NonlinearDepthCompression::default();
        assert_eq!(c.compress_meters(0.0), 0);
        let expected_100m = (32.0 * (1.0 + 100.0 / 100.0f32).ln()).round() as u8;
        assert_eq!(c.compress_meters(100.0), expected_100m);
        assert_eq!(c.compress_meters(100.0), 22);
    }

    #[test]
    fn compression_handles_nonfinite_and_negative_depths() {
        let c = NonlinearDepthCompression::default();
        assert_eq!(c.compress_meters(f32::NAN), 0);
        assert_eq!(c.compress_meters(f32::INFINITY), 0);
        assert_eq!(c.compress_meters(f32::NEG_INFINITY), 0);
        assert_eq!(c.compress_meters(-10.0), 0);
        assert_eq!(c.compress_meters(0.0), 0);
    }

    #[test]
    fn compression_clamps_to_max_blocks() {
        let c = NonlinearDepthCompression::default();
        assert_eq!(c.compress_meters(10_000.0), c.max_blocks);
        assert_eq!(c.compress_meters(5_000.0), c.max_blocks);
    }

    #[test]
    fn compression_is_monotonic_for_positive_depths() {
        let c = NonlinearDepthCompression::default();
        let mut prev = 0u8;
        for depth in [1.0, 5.0, 10.0, 50.0, 100.0, 200.0, 500.0, 1_000.0, 5_000.0] {
            let blocks = c.compress_meters(depth);
            assert!(
                blocks >= prev,
                "non-monotonic at depth={depth}: {blocks} < {prev}"
            );
            prev = blocks;
        }
        assert_eq!(prev, c.max_blocks);
    }

    /// Integration test with the real J-EGG500 files shipped in `jegg500/`,
    /// covering a 1° × 1° tile around central Japan.
    #[test]
    fn load_real_jegg500_files_produces_grid() {
        let config = Jegg500Config::new("jegg500");
        let llbbox = LLBBox::new(34.0, 136.0, 35.0, 137.0).unwrap();
        let grid = load_jegg500_for_bbox(&config, &llbbox, 1.0, 0.0)
            .unwrap()
            .expect("real J-EGG500 files should load successfully");
        // Derive the expected lattice from the same geographic extent the map
        // uses: ~91 km east-west and ~111 km north-south at this latitude.
        let (_, source_bbox) = CoordTransformer::llbbox_to_xzbbox(&llbbox, 1.0).unwrap();
        let expected_width =
            ((source_bbox.max_x() - source_bbox.min_x() + 1) as usize).div_ceil(500);
        let expected_height =
            ((source_bbox.max_z() - source_bbox.min_z() + 1) as usize).div_ceil(500);
        assert_eq!(grid.cell_size_blocks(), 500);
        assert_eq!(grid.width(), expected_width);
        assert_eq!(grid.height(), expected_height);
        assert_eq!((grid.min_x(), grid.min_z()), (0, 0));
    }

    /// A 90° world rotation must keep every 500 m source sample reachable at
    /// its rotated final-world position.
    #[test]
    fn rotated_real_jegg500_samples_align_with_rotated_world() {
        let config = Jegg500Config::new("jegg500");
        let llbbox = LLBBox::new(34.0, 136.0, 35.0, 137.0).unwrap();
        let source = load_jegg500_for_bbox(&config, &llbbox, 1.0, 0.0)
            .unwrap()
            .expect("real J-EGG500 files should load successfully");
        let rotated = load_jegg500_for_bbox(&config, &llbbox, 1.0, 90.0)
            .unwrap()
            .expect("real J-EGG500 files should load successfully");

        // Rotation must not expand or resample the source lattice.
        assert_eq!(
            (source.width(), source.height()),
            (rotated.width(), rotated.height())
        );

        let (_, source_bbox) = CoordTransformer::llbbox_to_xzbbox(&llbbox, 1.0).unwrap();
        let cx = (source_bbox.min_x() + source_bbox.max_x()) as f64 / 2.0;
        let cz = (source_bbox.min_z() + source_bbox.max_z()) as f64 / 2.0;
        let cell = source.cell_size_blocks() as i32;
        let mut checked = 0usize;

        'rows: for gz in 0..source.height() {
            let z = source.min_z() + gz as i32 * cell + cell / 2;
            for gx in 0..source.width() {
                let x = source.min_x() + gx as i32 * cell + cell / 2;
                let Some(expected) = source.sample_depth_m(x, z) else {
                    continue;
                };

                // Independent forward rotation for a positive (clockwise) 90°.
                let dx = x as f64 - cx;
                let dz = z as f64 - cz;
                let final_x = (-dz + cx).round() as i32;
                let final_z = (dx + cz).round() as i32;

                let actual = rotated
                    .sample_depth_m(final_x, final_z)
                    .unwrap_or_else(|| panic!("no rotated sample at ({final_x},{final_z})"));
                assert!(
                    (actual - expected).abs() < 0.01,
                    "rotated sample ({x},{z}) -> ({final_x},{final_z}) was {actual}, expected {expected}"
                );

                checked += 1;
                if checked >= 2_000 {
                    break 'rows;
                }
            }
        }

        assert!(
            checked >= 2_000,
            "expected at least 2,000 valid rotated samples, got {checked}"
        );
    }

    /// Integration test spanning two latitude tiles; verifies that samples from
    /// both `mesh500_34_136.txt` and `mesh500_35_136.txt` are loaded into the grid.
    #[test]
    fn load_multiple_real_jegg500_tiles() {
        let config = Jegg500Config::new("jegg500");
        let llbbox = LLBBox::new(34.0, 136.0, 36.0, 137.0).unwrap();
        let grid = load_jegg500_for_bbox(&config, &llbbox, 1.0, 0.0)
            .unwrap()
            .expect("real J-EGG500 files should load successfully");
        let (_, source_bbox) = CoordTransformer::llbbox_to_xzbbox(&llbbox, 1.0).unwrap();
        let expected_width =
            ((source_bbox.max_x() - source_bbox.min_x() + 1) as usize).div_ceil(500);
        let expected_height =
            ((source_bbox.max_z() - source_bbox.min_z() + 1) as usize).div_ceil(500);
        assert_eq!(grid.cell_size_blocks(), 500);
        assert_eq!(grid.width(), expected_width);
        assert_eq!(grid.height(), expected_height);
        let valid_cells = (0..grid.height())
            .flat_map(|z| (0..grid.width()).map(move |x| (x, z)))
            .filter(|&(x, z)| {
                grid.sample_depth_m(x as i32 * 500, z as i32 * 500)
                    .is_some()
            })
            .count();
        assert!(
            valid_cells > 5_000,
            "expected thousands of valid cells, got {valid_cells}"
        );
    }

    /// A real adjacent pair with different depths must blend at the shared cell
    /// boundary instead of keeping the nearest-cell step.
    #[test]
    fn real_data_blends_at_cell_boundary() {
        let config = Jegg500Config::new("jegg500");
        let llbbox = LLBBox::new(34.0, 136.0, 35.0, 137.0).unwrap();
        let grid = load_jegg500_for_bbox(&config, &llbbox, 1.0, 0.0)
            .unwrap()
            .expect("real J-EGG500 files should load successfully");
        let nearest = grid
            .clone()
            .with_interpolation(BathymetryInterpolation::Nearest);
        let cell = grid.cell_size_blocks() as i32;

        for gz in 0..grid.height() {
            let z = grid.min_z() + gz as i32 * cell + cell / 2;
            for gx in 0..grid.width() - 1 {
                let left_x = grid.min_x() + gx as i32 * cell + cell / 2;
                let right_x = left_x + cell;
                let (Some(left), Some(right)) = (
                    grid.sample_depth_m(left_x, z),
                    grid.sample_depth_m(right_x, z),
                ) else {
                    continue;
                };
                if (right - left).abs() < 5.0 {
                    continue;
                }

                let boundary_x = left_x + cell / 2;
                let smooth = grid.sample_depth_m(boundary_x, z).unwrap();
                let hard = nearest.sample_depth_m(boundary_x, z).unwrap();
                assert!(
                    (smooth - (left + right) / 2.0).abs() < 0.01,
                    "expected the midpoint average, got {smooth} for {left}/{right}"
                );
                assert!(
                    (hard - left).abs() < f32::EPSILON || (hard - right).abs() < f32::EPSILON,
                    "nearest should return one of the adjacent cells, got {hard}"
                );
                return;
            }
        }

        panic!("real J-EGG500 data contains no adjacent cells with a depth difference");
    }
}
