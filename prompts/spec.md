# Noise Player
This is a project to generate and play brown, pink, or white noise, implemented in Rust.
It runs as a long-lived headless service on a machine with speakers attached and is
controlled from Home Assistant.

## Goals
- Play continuous brown (primary), pink, or white noise with no audible seam, click, or pop.
- Be fully controllable from Home Assistant: on/off, noise type, volume, sleep timer.
- Run unattended as a system service; recover cleanly from MQTT or audio-device outages.
- Low CPU at steady state (playback is a buffer copy, not live synthesis).

## Non-goals (for v1)
- Playing arbitrary audio files or streaming to other devices.
- A GUI or web UI of its own (Home Assistant is the UI).
- Multi-room sync.

# CnC
The system is designed to be controlled from Home Assistant via MQTT, using
[HA MQTT Discovery](https://www.home-assistant.io/integrations/mqtt/#mqtt-discovery) so the
entities appear automatically with no YAML on the HA side.

## Why MQTT
- HA already has first-class MQTT support; discovery gives us native entities for free.
- Decouples the player from HA (it keeps working if HA restarts; state is retained on the broker).
- Alternatives considered:
  - *Custom HA integration + HTTP API*: would allow a native `media_player` entity, but means
    maintaining Python code inside HA. Not needed — see below.
  - *ESPHome native API*: not a good fit for a non-ESP device.

## Media player entity (optional, no custom code)
If a single media-player card is wanted in HA, wrap the MQTT entities with HA's built-in
[Universal Media Player](https://www.home-assistant.io/integrations/universal/) in YAML:
Playing → on/off, Volume → volume, Noise type → source. An example config goes in
`deploy/ha-universal-media-player.yaml`.

## Device & Entities
All entities are grouped under one HA device (`noise_player_<instance_id>`).

| Entity           | HA component | Values / Range                     | Notes                                   |
|------------------|--------------|------------------------------------|-----------------------------------------|
| Playing          | `switch`     | ON / OFF                           | Fades in/out rather than hard start/stop |
| Noise type       | `select`     | `brown`, `pink`, `white`           | Crossfades when changed while playing    |
| Volume           | `number`     | 0–100 (slider)                     | Perceptual (dB-mapped) curve, ramped     |
| Sleep timer      | `number`     | 0–480 minutes (0 = off)            | Fades out over the last N seconds        |
| Timer remaining  | `sensor`     | minutes                            | Read-only                                |
| Status           | `sensor`     | `playing`, `stopped`, `error`      | Error detail as attribute                |

## Topics
Base topic: `noise_player/<instance_id>/`
- `.../<entity>/set` — commands from HA (entities: `playing`, `noise_type`, `volume`,
  `sleep_timer`). **Retained** for Playing, Noise type and Volume
  (discovery config sets `"retain": true`, so HA publishes them retained); not retained for
  Sleep timer, so a reboot never restarts an old timer.
- `.../<entity>/state` — state published by the player (retained)
- `.../availability` — `online` / `offline`, with `offline` registered as the MQTT Last Will
- Discovery configs published (retained) under `homeassistant/<component>/noise_player_<instance_id>/<entity>/config`

## Behavior
- The broker is the source of truth for desired state; there is no local state file.
- On startup: generate loops, connect, subscribe to the `/set` topics. The broker delivers
  the retained Playing / Noise type / Volume commands and the player applies them, so after
  a reboot or power loss it returns to whatever HA last asked for (playing or stopped).
  Then publish discovery, availability and current state.
- Until the broker is reached (or if a topic has no retained value), use config defaults
  and stay **stopped**.
- When the player changes state on its own (sleep timer expires), it also publishes a
  retained `OFF` to `playing/set`, so the retained command never goes stale and a
  later reboot won't resume playback the timer already stopped.
- Retained commands are cleared (empty retained payload) by `noise-player uninstall` when
  a Pi is retired, so HA doesn't keep a ghost device.
- Sleep timer counts down only while playing (set it while stopped and it starts with
  playback); turning playback off cancels it. The last `sleep_fade_seconds` fade out.
- Re-publish discovery when HA comes back online (`homeassistant/status` = `online`).
- Reconnect to the broker with backoff; playback is unaffected by broker outages.

# Noise Types
My primary focus is brown noise. I'd like to generate a loopable sample that won't
have a detectable "seam" between the loops.

## Generation approach: spectral synthesis (seamless by construction)
Build the spectrum directly and inverse-FFT it. The output of an inverse DFT is
inherently periodic, so the last sample flows into the first exactly like any other
pair of adjacent samples — **there is no seam to hide**, no crossfade needed.

1. Choose loop length `N` (e.g. 60 s × 48 kHz = 2,880,000 samples).
2. For each frequency bin `k`, set magnitude by noise color and a uniformly random phase:
   - white: `1`
   - pink: `1 / sqrt(f)`   (power ∝ 1/f)
   - brown: `1 / f`        (power ∝ 1/f²)
3. Zero the DC bin and apply a gentle high-pass roll-off (default ~20 Hz, configurable)
   so brown noise doesn't carry inaudible sub-bass that wastes headroom/drives woofers.
   Optionally a gentle low-pass for a "softer" brown.
4. Inverse real FFT (`realfft` crate) → time-domain loop.
5. Normalize to a target peak (e.g. −3 dBFS) to leave room for volume ramping.
6. Stereo: generate L and R with independent random phases so the sound is wide and
   decorrelated rather than mono-in-the-middle.

Why not the classic approaches:
- *Integrated white noise (random walk)* drifts and must be leaky/high-passed, and its
  endpoints don't match, so the loop needs a crossfade — which briefly changes the
  spectrum and is exactly the "seam" we want to avoid.
- *Live synthesis* (no loop at all) is also seamless and is a reasonable fallback, but
  spectral synthesis gives precise control over the spectrum and near-zero runtime CPU.

## Loop parameters (configurable)
- Sample rate: match output device (default 48 kHz).
- Loop length: default 60 s (long enough that repetition isn't perceptible).
- Seed: optional, for reproducible output.
- Loops are generated at startup (or cached to disk) and held in memory: 60 s stereo f32
  @ 48 kHz ≈ 23 MB per color.

# Audio Output
- Output via `cpal` (ALSA / PipeWire on Linux, CoreAudio on macOS).
- Output device selectable by name in config; default device otherwise.
- All level changes (start, stop, volume, type change, sleep timer) are ramped
  (e.g. 50 ms for volume, 1–2 s fades for start/stop, equal-power crossfade for type change)
  so there are never clicks.
- The real-time audio callback does no allocation, locking, or I/O; control changes are
  passed in via lock-free atomics/ring buffer.
- If the audio device disappears, report `error` status and retry periodically.

# Configuration
TOML file (path via `--config`, default `/etc/noise-player/config.toml`); the MQTT
password can come from `NOISE_PLAYER_MQTT_PASSWORD`. Annotated example:
`deploy/config.example.toml`. The sample rate is the output device's default (cpal
prefers 48 kHz); loops are generated to match.

# CLI
- `noise-player run --config <path>` — run the service.
- `noise-player uninstall --config <path>` — clear this instance's retained discovery,
  command and state topics (stop the service first).
- `noise-player render --color brown --seconds 60 out.wav` — write a loop to WAV for
  listening tests.
- `noise-player play --color brown --volume 40` — play locally, controlled from stdin.
- `noise-player devices` — list audio outputs (to find the USB speaker's name).

# Tech Stack
- `cpal` — audio output
- `realfft` / `rustfft` — spectral synthesis
- `rand` (+ `rand_chacha` for seeded RNG)
- `rumqttc` + `tokio` — MQTT client
- `serde` + `toml` — config; `serde_json` — discovery payloads
- `secrecy` — MQTT password (redacted from `Debug`, zeroed on drop)
- `clap` — CLI; `tracing` — logging
- `hound` — WAV export

# Deployment
- Target: one Raspberry Pi per room, running Raspberry Pi OS 64-bit (`aarch64`), with a
  USB speaker as the only audio output.
- Audio goes straight to ALSA; no PulseAudio/PipeWire needed on a headless Pi.
- Runs under Docker Compose (`compose.yaml`) with `restart: unless-stopped` instead of a
  systemd unit. The container gets `/dev/snd` and runs as a non-root user in `audio`.
- Settings come from `NOISE_PLAYER_*` variables in `.env`; a mounted `config.toml` is
  optional and only needed for the less common settings.
- Build: the multi-stage `Dockerfile` compiles in `rust:1-bookworm` (arm64) and ships a
  `debian:bookworm-slim` runtime with `libasound2`. Build on the Pi with
  `docker compose up -d --build`, or on an Apple Silicon Mac (native arm64) and copy the
  image over with `docker save | ssh pi docker load`.
  `docker build --target binary --output target/pi .` exports just the binary.
- The loop is generated at startup (a few seconds on a Pi).
- The USB speaker may enumerate late at boot or be unplugged; the audio-device retry
  covers both cases.

# Testing
- Unit: generated spectra match target slopes (white / pink / brown ≈ 0 / −3 / −6 dB per octave).
- Unit: loop seam check — the sample-to-sample delta across the wrap point is
  statistically indistinguishable from deltas elsewhere.
- Integration: MQTT command → state round-trip against a local broker (e.g. mosquitto in Docker).
- Manual: listen to `render` output looped in an audio editor.

# Future Ideas
- Extra colors (blue/violet/grey), custom EQ presets.
- Gradual fade-in "wake" mode.
- Native HA `media_player` via a custom integration, if the Universal Media Player wrapper
  turns out not to be enough.
