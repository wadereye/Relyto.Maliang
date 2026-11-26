//! 地理标记地图:Web 墨卡托瓦片坐标 + OpenStreetMap 瓦片后台下载。
//!
//! 只负责「坐标换算 + 瓦片取回」;绘制与交互在 `app.rs` 的地图窗里。
//! 瓦片按 `数据目录/map_tiles/z/x/y.png` 落盘缓存,离线时读缓存,
//! 网络不可用时由 UI 显示坐标网格与照片点,不阻塞主线程。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

/// 单张瓦片边长(OSM 标准)。
pub const TILE: f64 = 256.0;

/// 经度 → 瓦片 X(小数;z 层)。
pub fn tile_x(lon: f64, z: u8) -> f64 {
    let n = (1u64 << z) as f64;
    (lon.clamp(-180.0, 180.0) + 180.0) / 360.0 * n
}

/// 纬度 → 瓦片 Y(小数;Web 墨卡托,纬度限制在 ±85.0511)。
pub fn tile_y(lat: f64, z: u8) -> f64 {
    let n = (1u64 << z) as f64;
    let lat = lat.clamp(-85.051_128_779_806_6, 85.051_128_779_806_6);
    let rad = lat.to_radians();
    (1.0 - ((rad.tan() + 1.0 / rad.cos()).ln() / std::f64::consts::PI)) / 2.0 * n
}

/// 瓦片 X(小数)→ 经度。
pub fn lon_of_tile_x(x: f64, z: u8) -> f64 {
    let n = (1u64 << z) as f64;
    x / n * 360.0 - 180.0
}

/// 瓦片 Y(小数)→ 纬度。
pub fn lat_of_tile_y(y: f64, z: u8) -> f64 {
    let n = (1u64 << z) as f64;
    let v = std::f64::consts::PI * (1.0 - 2.0 * y / n);
    v.sinh().atan().to_degrees()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TileKey {
    z: u8,
    x: u32,
    y: u32,
}

/// 后台瓦片下载结果。
pub struct TileFetch {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub result: Result<Vec<u8>, String>,
}

/// 单线程瓦片加载器:请求排队,结果通过 `poll` 回收。
pub struct TileLoader {
    tx: Sender<TileKey>,
    rx: Receiver<TileFetch>,
}

impl TileLoader {
    /// 起后台线程;`cache_dir` 不存在时按需创建。
    pub fn spawn(cache_dir: PathBuf) -> Self {
        let (tx, req_rx) = mpsc::channel::<TileKey>();
        let (res_tx, rx) = mpsc::channel::<TileFetch>();
        std::thread::spawn(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(15)))
                .timeout_connect(Some(Duration::from_secs(6)))
                .build()
                .into();
            while let Ok(req) = req_rx.recv() {
                let path = cache_dir
                    .join(req.z.to_string())
                    .join(req.x.to_string())
                    .join(format!("{}.png", req.y));
                let result = match std::fs::read(&path) {
                    Ok(bytes) => Ok(bytes),
                    Err(_) => fetch_tile(&agent, req.z, req.x, req.y, &path),
                };
                let _ = res_tx.send(TileFetch {
                    z: req.z,
                    x: req.x,
                    y: req.y,
                    result,
                });
            }
        });
        Self { tx, rx }
    }

    pub fn request(&self, z: u8, x: u32, y: u32) {
        let _ = self.tx.send(TileKey { z, x, y });
    }

    /// 取回已完成的结果(非阻塞)。
    pub fn poll(&self) -> Vec<TileFetch> {
        let mut out = Vec::new();
        while let Ok(v) = self.rx.try_recv() {
            out.push(v);
        }
        out
    }
}

fn fetch_tile(
    agent: &ureq::Agent,
    z: u8,
    x: u32,
    y: u32,
    cache: &PathBuf,
) -> Result<Vec<u8>, String> {
    let url = format!("https://tile.openstreetmap.org/{z}/{x}/{y}.png");
    let mut resp = agent
        .get(&url)
        .header("User-Agent", "Maliang/0.3 (offline photo manager map view)")
        .call()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let bytes = resp
        .body_mut()
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(cache, &bytes);
    Ok(bytes)
}

/// 把 (z,x,y) 限制在合法范围(纬度越界直接丢弃)。
pub fn clamp_tile(z: u8, x: i64, y: i64) -> Option<(u8, u32, u32)> {
    let n = 1i64 << z;
    if y < 0 || y >= n {
        return None;
    }
    Some((z, x.rem_euclid(n) as u32, y as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_maps_to_center_tile() {
        assert!((tile_x(0.0, 0) - 0.5).abs() < 1e-9);
        assert!((tile_y(0.0, 0) - 0.5).abs() < 1e-9);
        assert!((tile_x(0.0, 10) - 512.0).abs() < 1e-9);
        assert!((tile_y(0.0, 10) - 512.0).abs() < 1e-9);
    }

    #[test]
    fn roundtrip_lon_lat() {
        for &(lat, lon) in &[(31.23, 121.47), (-33.87, 151.21), (0.0, 0.0)] {
            let x = tile_x(lon, 12);
            let y = tile_y(lat, 12);
            let lon2 = lon_of_tile_x(x, 12);
            let lat2 = lat_of_tile_y(y, 12);
            assert!((lon - lon2).abs() < 1e-9, "{lon} != {lon2}");
            assert!((lat - lat2).abs() < 1e-9, "{lat} != {lat2}");
        }
    }

    #[test]
    fn clamps_out_of_range_y_and_wraps_x() {
        assert_eq!(clamp_tile(2, -1, 0), Some((2, 3, 0)));
        assert_eq!(clamp_tile(2, 4, 3), Some((2, 0, 3)));
        assert_eq!(clamp_tile(2, 0, -1), None);
        assert_eq!(clamp_tile(2, 0, 4), None);
    }
}
