//! Home Assistant control over MQTT, using MQTT discovery.
//!
//! The Playing / Noise type / Volume command topics are retained (HA publishes them
//! with `retain: true`), so the broker holds the desired state and a restarted
//! player picks up where HA left it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use rumqttc::{AsyncClient, Event, EventLoop, LastWill, MqttOptions, Packet, QoS};
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::config::Config;
use crate::noise::Color;
use crate::player::{AudioHealth, Controls};

const TICK: Duration = Duration::from_millis(250);
const RECONNECT_DELAY: Duration = Duration::from_secs(5);
const MAX_SLEEP_MINUTES: u32 = 480;

pub struct Topics {
    base: String,
    discovery_prefix: String,
    node_id: String,
}

impl Topics {
    pub fn new(cfg: &Config) -> Topics {
        Topics {
            base: format!("{}/{}", cfg.mqtt.base_topic, cfg.instance_id),
            discovery_prefix: cfg.mqtt.discovery_prefix.clone(),
            node_id: format!("noise_player_{}", cfg.instance_id.replace('-', "_")),
        }
    }

    fn set(&self, entity: &str) -> String {
        format!("{}/{entity}/set", self.base)
    }

    fn state(&self, entity: &str) -> String {
        format!("{}/{entity}/state", self.base)
    }

    fn availability(&self) -> String {
        format!("{}/availability", self.base)
    }

    fn attributes(&self) -> String {
        format!("{}/status/attributes", self.base)
    }

    fn discovery(&self, component: &str, entity: &str) -> String {
        format!("{}/{component}/{}/{entity}/config", self.discovery_prefix, self.node_id)
    }

    fn ha_status(&self) -> String {
        format!("{}/status", self.discovery_prefix)
    }
}

/// (component, entity) for every discovered entity.
const ENTITIES: [(&str, &str); 6] = [
    ("switch", "playing"),
    ("select", "noise_type"),
    ("number", "volume"),
    ("number", "sleep_timer"),
    ("sensor", "timer_remaining"),
    ("sensor", "status"),
];
/// Entities HA sends commands to. Subscribed in this order so retained type and
/// volume land before a retained `playing = ON` starts playback.
const COMMANDS: [&str; 4] = ["noise_type", "volume", "sleep_timer", "playing"];

fn discovery_configs(cfg: &Config, t: &Topics) -> Vec<(String, Value)> {
    let device = json!({
        "identifiers": [t.node_id],
        "name": cfg.device_name(),
        "manufacturer": "noise-player",
        "model": "Noise Player",
        "sw_version": env!("CARGO_PKG_VERSION"),
    });
    let common = |entity: &str, name: &str| {
        json!({
            "name": name,
            "unique_id": format!("{}_{entity}", t.node_id),
            "state_topic": t.state(entity),
            "availability_topic": t.availability(),
            "device": device,
        })
    };
    let with = |mut base: Value, extra: Value| {
        base.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        base
    };
    let options: Vec<&str> = Color::ALL.iter().map(|c| c.name()).collect();

    let configs = [
        with(common("playing", "Playing"), json!({
            "command_topic": t.set("playing"),
            "retain": true,
            "icon": "mdi:waves",
        })),
        with(common("noise_type", "Noise type"), json!({
            "command_topic": t.set("noise_type"),
            "retain": true,
            "options": options,
            "icon": "mdi:palette",
        })),
        with(common("volume", "Volume"), json!({
            "command_topic": t.set("volume"),
            "retain": true,
            "min": 0, "max": 100, "step": 1,
            "mode": "slider",
            "icon": "mdi:volume-high",
        })),
        with(common("sleep_timer", "Sleep timer"), json!({
            "command_topic": t.set("sleep_timer"),
            "retain": false,
            "min": 0, "max": MAX_SLEEP_MINUTES, "step": 5,
            "mode": "box",
            "unit_of_measurement": "min",
            "icon": "mdi:timer-outline",
        })),
        with(common("timer_remaining", "Timer remaining"), json!({
            "unit_of_measurement": "min",
            "icon": "mdi:timer-sand",
        })),
        with(common("status", "Status"), json!({
            "json_attributes_topic": t.attributes(),
            "icon": "mdi:information-outline",
        })),
    ];
    ENTITIES
        .iter()
        .zip(configs)
        .map(|((component, entity), config)| (t.discovery(component, entity), config))
        .collect()
}

fn mqtt_options(cfg: &Config, t: &Topics) -> MqttOptions {
    let mut opts = MqttOptions::new(
        format!("noise-player-{}", cfg.instance_id),
        &cfg.mqtt.host,
        cfg.mqtt.port,
    );
    opts.set_keep_alive(Duration::from_secs(30));
    opts.set_last_will(LastWill::new(t.availability(), "offline", QoS::AtLeastOnce, true));
    if let Some(user) = &cfg.mqtt.username {
        // rumqttc needs a plain String; it only goes into the CONNECT packet.
        let password = cfg.mqtt.password.as_ref().map(|p| p.expose_secret().to_owned());
        opts.set_credentials(user, password.unwrap_or_default());
    }
    opts
}

/// Sleep timer: counts down only while playing; stopping cancels it.
#[derive(Default)]
struct SleepTimer {
    minutes: u32,
    deadline: Option<Instant>,
}

impl SleepTimer {
    fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline.map(|d| d.saturating_duration_since(now))
    }

    fn remaining_minutes(&self, now: Instant) -> u32 {
        match self.remaining(now) {
            Some(r) => r.as_secs().div_ceil(60) as u32,
            None => self.minutes,
        }
    }
}

#[derive(Clone, PartialEq)]
struct Snapshot {
    playing: bool,
    color: Color,
    volume: u8,
    sleep_minutes: u32,
    remaining: u32,
    error: Option<String>,
}

struct Service {
    cfg: Config,
    topics: Topics,
    client: AsyncClient,
    controls: Arc<Controls>,
    health: Arc<AudioHealth>,
    timer: SleepTimer,
    last_published: Option<Snapshot>,
}

impl Service {
    fn snapshot(&self, now: Instant) -> Snapshot {
        Snapshot {
            playing: self.controls.playing(),
            color: self.controls.color(),
            volume: self.controls.volume(),
            sleep_minutes: self.timer.minutes,
            remaining: self.timer.remaining_minutes(now),
            error: self.health.error(),
        }
    }

    async fn on_connected(&mut self) -> anyhow::Result<()> {
        info!("connected to MQTT broker {}:{}", self.cfg.mqtt.host, self.cfg.mqtt.port);
        let t = &self.topics;
        for entity in COMMANDS {
            self.client.subscribe(t.set(entity), QoS::AtLeastOnce).await?;
        }
        self.client.subscribe(t.ha_status(), QoS::AtLeastOnce).await?;
        self.publish_discovery().await?;
        self.last_published = None;
        self.publish_state_if_changed().await
    }

    async fn publish_discovery(&self) -> anyhow::Result<()> {
        for (topic, config) in discovery_configs(&self.cfg, &self.topics) {
            self.client
                .publish(topic, QoS::AtLeastOnce, true, config.to_string())
                .await?;
        }
        self.client
            .publish(self.topics.availability(), QoS::AtLeastOnce, true, "online")
            .await?;
        Ok(())
    }

    async fn publish_state_if_changed(&mut self) -> anyhow::Result<()> {
        let s = self.snapshot(Instant::now());
        if self.last_published.as_ref() == Some(&s) {
            return Ok(());
        }
        let t = &self.topics;
        let status = match (&s.error, s.playing) {
            (Some(_), _) => "error",
            (None, true) => "playing",
            (None, false) => "stopped",
        };
        let states = [
            ("playing", (if s.playing { "ON" } else { "OFF" }).to_string()),
            ("noise_type", s.color.name().to_string()),
            ("volume", s.volume.to_string()),
            ("sleep_timer", s.sleep_minutes.to_string()),
            ("timer_remaining", s.remaining.to_string()),
            ("status", status.to_string()),
        ];
        for (entity, payload) in states {
            self.client.publish(t.state(entity), QoS::AtLeastOnce, true, payload).await?;
        }
        let attrs = json!({ "error": s.error });
        self.client
            .publish(t.attributes(), QoS::AtLeastOnce, true, attrs.to_string())
            .await?;
        self.last_published = Some(s);
        Ok(())
    }

    async fn on_message(&mut self, topic: &str, payload: &[u8]) -> anyhow::Result<()> {
        let payload = String::from_utf8_lossy(payload);
        let payload = payload.trim();
        let t = &self.topics;
        if topic == t.ha_status() {
            if payload == "online" {
                info!("Home Assistant restarted; republishing discovery");
                self.publish_discovery().await?;
                self.last_published = None;
            }
        } else if topic == t.set("playing") {
            match payload {
                "ON" => self.start_playing(),
                "OFF" => self.stop_playing(),
                // Empty payload = retained command cleared; nothing to do.
                "" => {}
                _ => warn!("ignoring playing command {payload:?}"),
            }
        } else if topic == t.set("noise_type") {
            match Color::from_name(&payload.to_lowercase()) {
                Some(c) => self.controls.set_color(c),
                None if payload.is_empty() => {}
                None => warn!("ignoring noise type {payload:?}"),
            }
        } else if topic == t.set("volume") {
            match payload.parse::<f32>() {
                Ok(v) => self.controls.set_volume(v.round().clamp(0.0, 100.0) as u8),
                Err(_) if payload.is_empty() => {}
                Err(_) => warn!("ignoring volume {payload:?}"),
            }
        } else if topic == t.set("sleep_timer") {
            match payload.parse::<f32>() {
                Ok(m) => self.set_sleep_timer(m.round().clamp(0.0, MAX_SLEEP_MINUTES as f32) as u32),
                Err(_) => warn!("ignoring sleep timer {payload:?}"),
            }
        }
        self.publish_state_if_changed().await
    }

    // Both are no-ops when already in that state, so retained commands re-delivered
    // on reconnect (or our own timer-expiry OFF) don't disturb a running timer.
    fn start_playing(&mut self) {
        if self.controls.playing() {
            return;
        }
        self.controls.set_fade(1.0);
        if self.timer.minutes > 0 && self.timer.deadline.is_none() {
            self.timer.deadline = Some(Instant::now() + minutes(self.timer.minutes));
        }
        self.controls.set_playing(true);
    }

    fn stop_playing(&mut self) {
        if !self.controls.playing() {
            return;
        }
        self.controls.set_playing(false);
        self.timer = SleepTimer::default();
    }

    fn set_sleep_timer(&mut self, mins: u32) {
        self.controls.set_fade(1.0);
        self.timer = SleepTimer {
            minutes: mins,
            deadline: (mins > 0 && self.controls.playing())
                .then(|| Instant::now() + minutes(mins)),
        };
        if mins > 0 {
            info!("sleep timer set to {mins} min");
        }
    }

    async fn tick(&mut self) -> anyhow::Result<()> {
        if let Some(remaining) = self.timer.remaining(Instant::now()) {
            let fade_secs = self.cfg.noise.sleep_fade_seconds.max(0.1);
            let x = (remaining.as_secs_f32() / fade_secs).min(1.0);
            self.controls.set_fade(x * x);
            if remaining.is_zero() {
                info!("sleep timer expired; stopping");
                // Fade is already at 0; it's reset to 1 on the next start.
                self.controls.set_playing(false);
                self.timer = SleepTimer::default();
                // Keep the retained command in step so a reboot doesn't resume playback.
                self.client
                    .publish(self.topics.set("playing"), QoS::AtLeastOnce, true, "OFF")
                    .await?;
            }
        }
        self.publish_state_if_changed().await
    }

    async fn shutdown(&self, eventloop: &mut EventLoop) {
        let offline = self
            .client
            .publish(self.topics.availability(), QoS::AtLeastOnce, true, "offline")
            .await;
        if offline.is_ok() {
            // Best effort: the broker may be unreachable, and the Last Will covers that.
            let _ = disconnect_and_drain(&self.client, eventloop, Duration::from_secs(2)).await;
        }
    }
}

fn minutes(m: u32) -> Duration {
    Duration::from_secs(m as u64 * 60)
}

/// Run until SIGINT/SIGTERM. The audio side runs independently; broker outages
/// only pause control.
pub async fn run(cfg: Config, controls: Arc<Controls>, health: Arc<AudioHealth>) -> anyhow::Result<()> {
    let topics = Topics::new(&cfg);
    let (client, mut eventloop) = AsyncClient::new(mqtt_options(&cfg, &topics), 64);
    let mut svc = Service {
        cfg,
        topics,
        client,
        controls,
        health,
        timer: SleepTimer::default(),
        last_published: None,
    };

    let mut ticker = tokio::time::interval(TICK);
    let mut shutdown = std::pin::pin!(shutdown_signal());
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = ticker.tick() => svc.tick().await?,
            ev = eventloop.poll() => match ev {
                Ok(Event::Incoming(Packet::ConnAck(_))) => svc.on_connected().await?,
                Ok(Event::Incoming(Packet::Publish(p))) => svc.on_message(&p.topic, &p.payload).await?,
                Ok(_) => {}
                Err(e) => {
                    warn!("MQTT connection error: {e}; retrying in {RECONNECT_DELAY:?}");
                    tokio::time::sleep(RECONNECT_DELAY).await;
                }
            },
        }
    }

    info!("shutting down");
    svc.shutdown(&mut eventloop).await;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("installing SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = ctrl_c.await;
}

/// Remove this instance from HA and clear its retained topics.
pub async fn uninstall(cfg: &Config) -> anyhow::Result<()> {
    let topics = Topics::new(cfg);
    let (client, mut eventloop) = AsyncClient::new(mqtt_options(cfg, &topics), 64);
    let mut retained: Vec<String> = discovery_configs(cfg, &topics).into_iter().map(|(t, _)| t).collect();
    retained.extend(COMMANDS.iter().map(|e| topics.set(e)));
    retained.extend(ENTITIES.iter().map(|(_, e)| topics.state(e)));
    retained.push(topics.attributes());
    retained.push(topics.availability());

    for topic in &retained {
        client.publish(topic, QoS::AtLeastOnce, true, Vec::new()).await?;
    }
    wait_for_acks(&mut eventloop, retained.len(), Duration::from_secs(10)).await?;
    disconnect_and_drain(&client, &mut eventloop, Duration::from_secs(2)).await?;
    println!("cleared {} retained topics for {}", retained.len(), topics.node_id);
    Ok(())
}

/// Disconnect, driving the event loop until the connection actually closes so
/// queued publishes are flushed first (the `Disconnect` event fires before the flush).
async fn disconnect_and_drain(
    client: &AsyncClient,
    eventloop: &mut EventLoop,
    timeout: Duration,
) -> anyhow::Result<()> {
    client.disconnect().await?;
    tokio::time::timeout(timeout, async {
        // Once the disconnect is sent, the next poll fails with the closed connection.
        while eventloop.poll().await.is_ok() {}
    })
    .await
    .context("timed out disconnecting from the broker")
}

/// Drive the event loop until `n` publishes have been acknowledged.
async fn wait_for_acks(eventloop: &mut EventLoop, n: usize, timeout: Duration) -> anyhow::Result<()> {
    tokio::time::timeout(timeout, async {
        let mut acked = 0;
        while acked < n {
            if let Event::Incoming(Packet::PubAck(_)) = eventloop.poll().await? {
                acked += 1;
            }
        }
        anyhow::Ok(())
    })
    .await
    .context("timed out waiting for the broker")?
}
