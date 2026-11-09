//! Momentary push button on a Raspberry Pi GPIO line that toggles playback.
//!
//! The button is wired between the GPIO pin and ground; the internal pull-up
//! keeps the line high until it is pressed. Debouncing is done by the kernel.

use std::time::Duration;

use anyhow::Context;
use gpiocdev::line::{Bias, EdgeDetection, EdgeKind};
use gpiocdev::Request;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::Button;

/// Start watching the button. Each press sends `()` on the returned channel.
///
/// A missing or inaccessible GPIO device is logged and leaves the channel
/// silent, so a wiring or permission problem never takes the player down.
pub fn spawn(cfg: &Button) -> Option<mpsc::UnboundedReceiver<()>> {
    let line = cfg.gpio?;
    let request = match open(cfg, line) {
        Ok(r) => r,
        Err(e) => {
            warn!("button disabled: {e:#}");
            return None;
        }
    };
    info!("button on GPIO {line} ({})", cfg.chip.display());

    let (tx, rx) = mpsc::unbounded_channel();
    let spawned = std::thread::Builder::new().name("button".into()).spawn(move || {
        loop {
            match request.read_edge_event() {
                Ok(ev) if ev.kind == EdgeKind::Falling => {
                    if tx.send(()).is_err() {
                        return; // service shut down
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("button disabled: reading GPIO events failed: {e}");
                    return;
                }
            }
        }
    });
    if let Err(e) = spawned {
        warn!("button disabled: couldn't start thread: {e}");
        return None;
    }
    Some(rx)
}

fn open(cfg: &Button, line: u32) -> anyhow::Result<Request> {
    Request::builder()
        .on_chip(&cfg.chip)
        .with_consumer("noise-player")
        .with_line(line)
        .as_input()
        .with_bias(Bias::PullUp)
        .with_edge_detection(EdgeDetection::FallingEdge)
        .with_debounce_period(Duration::from_millis(cfg.debounce_ms))
        .request()
        .with_context(|| format!("requesting GPIO {line} on {}", cfg.chip.display()))
}
