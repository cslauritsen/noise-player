//! DS18B20 (and other 1-Wire thermometer) readings via the kernel `w1_therm` driver.
//!
//! Enable the bus with `dtoverlay=w1-gpio,gpiopin=4` in `/boot/firmware/config.txt`.
//! A 3-wire sensor (VDD, GND, DQ) needs a 4.7 kΩ pull-up between DQ and 3.3 V.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::Temperature;

/// Latest reading in tenths of a degree Celsius; `None` while the sensor can't be read.
pub type Reading = Option<i32>;

/// The DS18B20 reports exactly 85 °C when it powers up without having converted.
const POWER_ON_RESET_MILLI: i32 = 85_000;

pub fn spawn(cfg: &Temperature) -> Option<watch::Receiver<Reading>> {
    if !cfg.enabled {
        return None;
    }
    let (tx, rx) = watch::channel(None);
    let dir = cfg.devices_dir.clone();
    let sensor = cfg.sensor.clone();
    let interval = Duration::from_secs_f32(cfg.interval_seconds.max(1.0));
    let spawned = std::thread::Builder::new().name("temperature".into()).spawn(move || {
        let mut reported = None;
        loop {
            let reading = find_sensor(&dir, sensor.as_deref())
                .and_then(|path| read_milli_celsius(&path))
                .map(|m| (m as f32 / 100.0).round() as i32);
            match &reading {
                Ok(_) if reported != Some(true) => info!("temperature sensor reading"),
                Err(e) if reported != Some(false) => warn!("temperature unavailable: {e}"),
                _ => {}
            }
            reported = Some(reading.is_ok());
            if tx.send(reading.ok()).is_err() {
                return; // service shut down
            }
            std::thread::sleep(interval);
        }
    });
    if let Err(e) = spawned {
        warn!("temperature disabled: couldn't start thread: {e}");
        return None;
    }
    Some(rx)
}

/// The configured sensor's `temperature` file, else the first thermometer on the bus.
fn find_sensor(dir: &Path, wanted: Option<&str>) -> Result<PathBuf, String> {
    if let Some(id) = wanted {
        return Ok(dir.join(id).join("temperature"));
    }
    let mut found: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| format!("reading {}: {e} (is the w1-gpio overlay enabled?)", dir.display()))?
        .filter_map(|e| e.ok())
        .filter(|e| {
            // 28 = DS18B20, 22 = DS1822, 10 = DS18S20
            let name = e.file_name();
            let name = name.to_string_lossy();
            ["28-", "22-", "10-"].iter().any(|p| name.starts_with(p))
        })
        .map(|e| e.path().join("temperature"))
        .collect();
    found.sort();
    found.into_iter().next().ok_or_else(|| {
        format!("no 1-Wire thermometer under {} (check wiring and pull-up)", dir.display())
    })
}

fn read_milli_celsius(path: &Path) -> Result<i32, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    parse_milli_celsius(&text)
}

fn parse_milli_celsius(text: &str) -> Result<i32, String> {
    let milli: i32 = text.trim().parse().map_err(|_| format!("unexpected reading {:?}", text.trim()))?;
    if milli == POWER_ON_RESET_MILLI {
        return Err("sensor returned its power-on value (85 °C); check power and pull-up".into());
    }
    Ok(milli)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sysfs_reading() {
        assert_eq!(parse_milli_celsius("21562\n"), Ok(21562));
        assert_eq!(parse_milli_celsius("-1250\n"), Ok(-1250));
        assert!(parse_milli_celsius("85000\n").is_err());
        assert!(parse_milli_celsius("garbage").is_err());
    }

    #[test]
    fn picks_first_thermometer_and_ignores_bus_masters() {
        let dir = std::env::temp_dir().join(format!("noise-player-w1-{}", std::process::id()));
        for d in ["w1_bus_master1", "28-bbbb", "28-aaaa"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let found = find_sensor(&dir, None).unwrap();
        let by_id = find_sensor(&dir, Some("28-bbbb")).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, dir.join("28-aaaa/temperature"));
        assert_eq!(by_id, dir.join("28-bbbb/temperature"));
        assert!(find_sensor(&dir, None).is_err());
    }
}
