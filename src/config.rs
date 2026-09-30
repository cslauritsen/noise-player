use std::path::Path;

use anyhow::Context;
use secrecy::SecretString;
use serde::Deserialize;

use crate::noise::Color;

pub const DEFAULT_PATH: &str = "/etc/noise-player/config.toml";

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Identifies this player in topics and HA; one per Pi / room.
    pub instance_id: String,
    /// HA device name; defaults to "Noise Player <instance_id>".
    pub name: Option<String>,
    pub mqtt: Mqtt,
    pub audio: Audio,
    pub noise: Noise,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Mqtt {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    /// Redacted in `Debug` output and zeroed on drop.
    pub password: Option<SecretString>,
    pub discovery_prefix: String,
    pub base_topic: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    /// Output device name or id; the system default when unset.
    pub device: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Noise {
    pub loop_seconds: f32,
    pub highpass_hz: f32,
    /// Used until the broker delivers a retained noise type.
    pub default_type: Color,
    /// Used until the broker delivers a retained volume.
    pub default_volume: u8,
    /// How long the sleep timer takes to fade out before it stops playback.
    pub sleep_fade_seconds: f32,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            instance_id: "default".into(),
            name: None,
            mqtt: Mqtt::default(),
            audio: Audio::default(),
            noise: Noise::default(),
        }
    }
}

impl Default for Mqtt {
    fn default() -> Self {
        Mqtt {
            host: "homeassistant.local".into(),
            port: 1883,
            username: None,
            password: None,
            discovery_prefix: "homeassistant".into(),
            base_topic: "noise_player".into(),
        }
    }
}

impl Default for Noise {
    fn default() -> Self {
        Noise {
            loop_seconds: 60.0,
            highpass_hz: 20.0,
            default_type: Color::Brown,
            default_volume: 40,
            sleep_fade_seconds: 30.0,
        }
    }
}

impl Config {
    /// Load `path` (or the default path, if it exists), then apply environment
    /// overrides. With no file at all, settings come from the environment and defaults.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Config> {
        let path = path.or_else(|| {
            let default = Path::new(DEFAULT_PATH);
            default.exists().then_some(default)
        });
        let mut cfg = match path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading config {}", path.display()))?;
                toml::from_str(&text)
                    .with_context(|| format!("parsing config {}", path.display()))?
            }
            None => Config::default(),
        };
        cfg.apply_env()?;
        anyhow::ensure!(
            !cfg.instance_id.is_empty()
                && cfg
                    .instance_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "instance_id must be non-empty and contain only letters, digits, '_' or '-'"
        );
        Ok(cfg)
    }

    fn apply_env(&mut self) -> anyhow::Result<()> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        if let Some(v) = var("NOISE_PLAYER_INSTANCE_ID") {
            self.instance_id = v;
        }
        if let Some(v) = var("NOISE_PLAYER_NAME") {
            self.name = Some(v);
        }
        if let Some(v) = var("NOISE_PLAYER_MQTT_HOST") {
            self.mqtt.host = v;
        }
        if let Some(v) = var("NOISE_PLAYER_MQTT_PORT") {
            self.mqtt.port = v.parse().context("NOISE_PLAYER_MQTT_PORT must be a port number")?;
        }
        if let Some(v) = var("NOISE_PLAYER_MQTT_USERNAME") {
            self.mqtt.username = Some(v);
        }
        if let Some(v) = var("NOISE_PLAYER_MQTT_PASSWORD") {
            self.mqtt.password = Some(v.into());
        }
        if let Some(v) = var("NOISE_PLAYER_AUDIO_DEVICE") {
            self.audio.device = Some(v);
        }
        Ok(())
    }

    pub fn device_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("Noise Player {}", self.instance_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_redacted_in_debug_output() {
        let cfg: Config = toml::from_str("[mqtt]\npassword = \"hunter2\"").unwrap();
        assert!(cfg.mqtt.password.is_some());
        assert!(!format!("{cfg:?}").contains("hunter2"));
    }
}
