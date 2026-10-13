use std::fs;

use crate::coordinate_system::geographic::LLBBox;
use crate::elevation::provider::RawElevationGrid;

struct JeggPoint {
    lat: f64,
    lon: f64,
    depth_m: f64,
}


fn load_jegg500(path: &str) -> Result<Vec<JeggPoint>, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)?;
    let mut points = Vec::new();

    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        let values: Vec<&str> = line.split_whitespace().collect();

        // 4列でない行は読み飛ばす
        if values.len() != 4 {
            continue;
        }

        // 緯度
        let lat: f64 = values[1].parse().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Line {}: invalid latitude: {}", line_number, e),
            )
        })?;

        // 経度
        let lon: f64 = values[2].parse().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Line {}: invalid longitude: {}", line_number, e),
            )
        })?;

        // 水深
        let depth: f64 = values[3].parse().map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Line {}: invalid depth: {}", line_number, e),
            )
        })?;

        // 不正な数値を拒否する
        if !lat.is_finite() || !lon.is_finite() || !depth.is_finite() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Line {}: non-finite number", line_number),
            )
            .into());
        }

        points.push(JeggPoint {
            lat,
            lon,
            depth_m: -depth,
        });
    }

    if points.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "No valid J-EGG500 data rows found",
        )
        .into());
    }

    Ok(points)
}

fn points_to_grid(
    points: &[JeggPoint],
    bbox: &LLBBox,
    grid_width: usize,
    grid_height: usize,
) -> RawElevationGrid {
    let min_lat = bbox.min().lat();
    let max_lat = bbox.max().lat();
    let min_lon = bbox.min().lng();
    let max_lon = bbox.max().lng();

    let mut heights = vec![vec![f64::NAN; grid_width]; grid_height];

    if points.is_empty() || grid_width == 0 || grid_height == 0 {
        return RawElevationGrid {
            heights_meters: heights,
        };
    }

    let lat_range = max_lat - min_lat;
    let lon_range = max_lon - min_lon;

    if lat_range <= 0.0 || lon_range <= 0.0 {
        return RawElevationGrid {
            heights_meters: heights,
        };
    }

    for point in points {
        if point.lat < min_lat
            || point.lat > max_lat
            || point.lon < min_lon
            || point.lon > max_lon
        {
            continue;
        }

        let x = ((point.lon - min_lon) / lon_range
            * (grid_width - 1) as f64)
            .round() as usize;

        let y = ((max_lat - point.lat) / lat_range
            * (grid_height - 1) as f64)
            .round() as usize;

        heights[y][x] = point.depth_m;
    }

    RawElevationGrid {
        heights_meters: heights,
    }
}

pub struct Jegg500;

impl crate::elevation::provider::ElevationProvider for Jegg500 {
    fn name(&self) -> &'static str {
        "jegg500"
    }

    fn coverage_bboxes(&self) -> Option<Vec<LLBBox>> {
        Some(vec![
            LLBBox::new(34.0, 137.0, 34.80875, 137.99997).unwrap(),
        ])
    }

    fn native_resolution_m(&self) -> f64 {
        500.0
    }

    fn fetch_raw(
        &self,
        bbox: &LLBBox,
        grid_width: usize,
        grid_height: usize,
    ) -> Result<RawElevationGrid, Box<dyn std::error::Error>> {
        let points = load_jegg500(
    concat!(env!("CARGO_MANIFEST_DIR"), "data/J-EGG500.date.34_137")
)?;

        Ok(points_to_grid(
            &points,
            bbox,
            grid_width,
            grid_height,
        ))
    }
}
