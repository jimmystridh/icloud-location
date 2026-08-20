//! Coordinate-system normalization for Apple servers in China.

use serde::{Deserialize, Serialize};

const EARTH_SEMI_MAJOR_AXIS: f64 = 6_378_245.0;
const EARTH_ECCENTRICITY_SQUARED: f64 = 0.006_693_421_883_570_923;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChinaCoordinates {
    #[default]
    Unchanged,
    Gcj02,
    Bd09,
}

pub(crate) fn to_wgs84(latitude: f64, longitude: f64, source: ChinaCoordinates) -> (f64, f64) {
    match source {
        ChinaCoordinates::Unchanged => (latitude, longitude),
        ChinaCoordinates::Gcj02 => gcj02_to_wgs84(latitude, longitude),
        ChinaCoordinates::Bd09 => {
            let (gcj_latitude, gcj_longitude) = bd09_to_gcj02(latitude, longitude);
            gcj02_to_wgs84(gcj_latitude, gcj_longitude)
        }
    }
}

fn gcj02_to_wgs84(latitude: f64, longitude: f64) -> (f64, f64) {
    let mut latitude_delta = transform_latitude(latitude - 35.0, longitude - 105.0);
    let mut longitude_delta = transform_longitude(latitude - 35.0, longitude - 105.0);
    let radians = latitude.to_radians();
    let sine = radians.sin();
    let magic = 1.0 - EARTH_ECCENTRICITY_SQUARED * sine * sine;
    let magic_root = magic.sqrt();

    latitude_delta = latitude_delta.to_degrees()
        / ((EARTH_SEMI_MAJOR_AXIS * (1.0 - EARTH_ECCENTRICITY_SQUARED)) / (magic * magic_root));
    longitude_delta =
        longitude_delta.to_degrees() / (EARTH_SEMI_MAJOR_AXIS / magic_root * radians.cos());

    (latitude - latitude_delta, longitude - longitude_delta)
}

fn bd09_to_gcj02(latitude: f64, longitude: f64) -> (f64, f64) {
    let y = latitude - 0.006;
    let x = longitude - 0.0065;
    let z = x.hypot(y) - 0.000_02 * (y * std::f64::consts::PI * 3000.0 / 180.0).sin();
    let theta = y.atan2(x) - 0.000_003 * (x * std::f64::consts::PI * 3000.0 / 180.0).cos();
    (z * theta.sin(), z * theta.cos())
}

fn transform_latitude(latitude: f64, longitude: f64) -> f64 {
    let mut result = -100.0
        + 2.0 * longitude
        + 3.0 * latitude
        + 0.2 * latitude * latitude
        + 0.1 * longitude * latitude
        + 0.2 * longitude.abs().sqrt();
    result += (20.0 * (6.0 * longitude * std::f64::consts::PI).sin()
        + 20.0 * (2.0 * longitude * std::f64::consts::PI).sin())
        * 2.0
        / 3.0;
    result += (20.0 * (latitude * std::f64::consts::PI).sin()
        + 40.0 * (latitude / 3.0 * std::f64::consts::PI).sin())
        * 2.0
        / 3.0;
    result += (160.0 * (latitude / 12.0 * std::f64::consts::PI).sin()
        + 320.0 * (latitude * std::f64::consts::PI / 30.0).sin())
        * 2.0
        / 3.0;
    result
}

fn transform_longitude(latitude: f64, longitude: f64) -> f64 {
    let mut result = 300.0
        + longitude
        + 2.0 * latitude
        + 0.1 * longitude * longitude
        + 0.1 * longitude * latitude
        + 0.1 * longitude.abs().sqrt();
    result += (20.0 * (6.0 * longitude * std::f64::consts::PI).sin()
        + 20.0 * (2.0 * longitude * std::f64::consts::PI).sin())
        * 2.0
        / 3.0;
    result += (20.0 * (longitude * std::f64::consts::PI).sin()
        + 40.0 * (longitude / 3.0 * std::f64::consts::PI).sin())
        * 2.0
        / 3.0;
    result += (150.0 * (longitude / 12.0 * std::f64::consts::PI).sin()
        + 300.0 * (longitude * std::f64::consts::PI / 30.0).sin())
        * 2.0
        / 3.0;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcj02_coordinates_are_converted_to_wgs84() {
        let (latitude, longitude) = to_wgs84(39.908_823, 116.397_470, ChinaCoordinates::Gcj02);

        assert!((latitude - 39.907_419_501_926_654).abs() < 1e-12);
        assert!((longitude - 116.391_226_417_577_84).abs() < 1e-12);
    }

    #[test]
    fn bd09_coordinates_are_converted_to_wgs84() {
        let (latitude, longitude) = to_wgs84(39.915, 116.404, ChinaCoordinates::Bd09);

        assert!((latitude - 39.907_253_214_522_02).abs() < 1e-12);
        assert!((longitude - 116.391_383_699_512_83).abs() < 1e-12);
    }
}
