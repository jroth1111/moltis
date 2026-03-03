//! CDP screencast: start/stop frame streaming and frame acknowledgment.
//!
//! `start_screencast` opens a `Page.startScreencast` session and spawns a
//! background task that forwards decoded frames to a broadcast channel.
//! Each frame is acknowledged so Chrome keeps delivering more frames.

use {
    base64::{Engine, engine::general_purpose::STANDARD as BASE64},
    chromiumoxide::{
        Page,
        cdp::browser_protocol::page::{
            ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
            StopScreencastParams,
        },
    },
    futures::StreamExt,
    tokio::{sync::broadcast, task::JoinHandle},
    tracing::{debug, warn},
};

use crate::error::Error;

/// A decoded screencast frame.
#[derive(Debug, Clone)]
pub struct ScreencastFrame {
    /// Raw image bytes (PNG or JPEG, depending on the format requested).
    pub data: Vec<u8>,
    /// CDP-reported frame timestamp.
    pub timestamp: f64,
}

/// Handle to an active screencast session.
///
/// Dropping this handle does **not** stop the screencast; call
/// [`stop_screencast`] explicitly to stop the CDP session.
/// The background task exits once the event stream closes (i.e., after
/// [`stop_screencast`] is called).
pub struct ScreencastHandle {
    /// Subscribe here to receive decoded frames.
    pub frames_rx: broadcast::Receiver<ScreencastFrame>,
    /// Background task that reads CDP events and broadcasts frames.
    _task: JoinHandle<()>,
}

impl std::fmt::Debug for ScreencastHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScreencastHandle").finish_non_exhaustive()
    }
}

// ── CDP helpers ──────────────────────────────────────────────────────────────

/// Start a screencast session on `page` and return a handle for receiving frames.
///
/// - `format`: `"jpeg"` or `"png"` (defaults to `"jpeg"` on invalid input).
/// - `quality`: 0–100 (only applies to JPEG).
/// - `every_nth`: deliver every Nth frame (1 = every frame).
pub async fn start_screencast(
    page: &Page,
    format: &str,
    quality: u8,
    every_nth: u32,
) -> Result<ScreencastHandle, Error> {
    let fmt = match format.to_lowercase().as_str() {
        "png" => StartScreencastFormat::Png,
        _ => StartScreencastFormat::Jpeg,
    };

    let params = StartScreencastParams::builder()
        .format(fmt)
        .quality(quality as i64)
        .every_nth_frame(every_nth as i64)
        .build();

    page.execute(params)
        .await
        .map_err(|e| Error::Cdp(format!("Page.startScreencast failed: {e}")))?;

    // Subscribe to frame events from the page.
    let mut event_stream = page
        .event_listener::<chromiumoxide::cdp::browser_protocol::page::EventScreencastFrame>()
        .await
        .map_err(|e| Error::Cdp(format!("event_listener setup failed: {e}")))?;

    let (tx, rx) = broadcast::channel::<ScreencastFrame>(16);
    let page_clone = page.clone();

    let task = tokio::spawn(async move {
        while let Some(event) = event_stream.next().await {
            // Decode the base64-encoded frame data.
            let raw = match BASE64.decode(event.data.as_ref() as &str) {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(error = %e, "failed to decode screencast frame");
                    continue;
                },
            };

            let timestamp = event
                .metadata
                .timestamp
                .as_ref()
                .map(|t| *t.inner())
                .unwrap_or(0.0);
            let frame = ScreencastFrame {
                data: raw,
                timestamp,
            };

            // Acknowledge the frame so Chrome sends the next one.
            if let Err(e) = page_clone
                .execute(ScreencastFrameAckParams::new(event.session_id))
                .await
            {
                warn!(error = %e, "failed to ack screencast frame");
            }

            // Broadcast the frame; ignore send errors (no subscribers).
            let _ = tx.send(frame);
            debug!("screencast frame forwarded");
        }
        debug!("screencast event stream closed");
    });

    Ok(ScreencastHandle {
        frames_rx: rx,
        _task: task,
    })
}

/// Stop the active screencast session on `page`.
///
/// After this call, the background task started by [`start_screencast`] will
/// exit once the event stream drains.
pub async fn stop_screencast(page: &Page) -> Result<(), Error> {
    page.execute(StopScreencastParams::default())
        .await
        .map_err(|e| Error::Cdp(format!("Page.stopScreencast failed: {e}")))?;

    Ok(())
}
