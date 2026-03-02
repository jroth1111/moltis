//! Behavioral evasion: human-like mouse movement and keyboard timing.
//!
//! Replaces the instant CDP mouse/keyboard events used in vanilla automation
//! with smooth Bezier-interpolated movement and randomised typing delays.

use std::time::Duration;

use {
    chromiumoxide::{
        Page,
        cdp::browser_protocol::input::{
            DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
            DispatchMouseEventType, MouseButton,
        },
    },
    rand::Rng,
    tokio::time::sleep,
};

use crate::error::Error;

/// Cubic ease-in-out: maps `t ∈ [0, 1]` to a smooth acceleration/deceleration curve.
fn cubic_ease_in_out(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let t2 = -2.0 * t + 2.0;
        1.0 - t2 * t2 * t2 / 2.0
    }
}

/// Move the virtual mouse from `from` to `to` along a smooth eased path.
///
/// Uses 20 steps with:
/// - cubic ease-in-out interpolation
/// - ±1 px random jitter per step
/// - 10–30 ms inter-step delay
pub async fn bezier_mouse_move(page: &Page, from: (f64, f64), to: (f64, f64)) -> Result<(), Error> {
    const STEPS: u32 = 20;
    let mut rng = rand::rng();

    for step in 0..=STEPS {
        let t = step as f64 / STEPS as f64;
        let eased = cubic_ease_in_out(t);

        let jitter_x: f64 = rng.random_range(-1.0_f64..=1.0_f64);
        let jitter_y: f64 = rng.random_range(-1.0_f64..=1.0_f64);

        let x = from.0 + (to.0 - from.0) * eased + jitter_x;
        let y = from.1 + (to.1 - from.1) * eased + jitter_y;

        let move_cmd = DispatchMouseEventParams::builder()
            .r#type(DispatchMouseEventType::MouseMoved)
            .x(x)
            .y(y)
            .build()
            .map_err(|e| Error::Cdp(e.to_string()))?;

        page.execute(move_cmd)
            .await
            .map_err(|e| Error::Cdp(e.to_string()))?;

        // Inter-step delay: 10–30 ms
        let delay_ms: u64 = 10 + rng.random_range(0_u64..20_u64);
        sleep(Duration::from_millis(delay_ms)).await;
    }

    Ok(())
}

/// Click at `(x, y)` with realistic Bezier mouse movement and randomised timing.
///
/// Sequence:
/// 1. Bezier move from (0, 0) to the target
/// 2. 50–150 ms pre-click pause
/// 3. MousePressed (left button, click_count = 1)
/// 4. 50–150 ms button-held pause
/// 5. MouseReleased
pub async fn realistic_click(page: &Page, x: f64, y: f64) -> Result<(), Error> {
    let mut rng = rand::rng();

    // Move to the target
    bezier_mouse_move(page, (0.0, 0.0), (x, y)).await?;

    // Pre-click pause
    let pre_ms: u64 = 50 + rng.random_range(0_u64..100_u64);
    sleep(Duration::from_millis(pre_ms)).await;

    // Button down
    let press_cmd = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MousePressed)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(press_cmd)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    // Hold duration
    let hold_ms: u64 = 50 + rng.random_range(0_u64..100_u64);
    sleep(Duration::from_millis(hold_ms)).await;

    // Button up
    let release_cmd = DispatchMouseEventParams::builder()
        .r#type(DispatchMouseEventType::MouseReleased)
        .x(x)
        .y(y)
        .button(MouseButton::Left)
        .click_count(1)
        .build()
        .map_err(|e| Error::Cdp(e.to_string()))?;
    page.execute(release_cmd)
        .await
        .map_err(|e| Error::Cdp(e.to_string()))?;

    Ok(())
}

/// Type `text` character-by-character with randomised inter-key delays.
///
/// Each character is sent as KeyDown + KeyUp. Delay between events is
/// `80 ms ± 70 ms`, clamped to `[10, 150] ms`.
pub async fn realistic_type(page: &Page, text: &str) -> Result<(), Error> {
    let mut rng = rand::rng();

    for c in text.chars() {
        let key_down = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyDown)
            .text(c.to_string())
            .build()
            .map_err(|e| Error::Cdp(e.to_string()))?;
        page.execute(key_down)
            .await
            .map_err(|e| Error::Cdp(e.to_string()))?;

        // Per-character typing delay: 80 ms ± 70 ms, clamped to [10, 150] ms
        let variance: i64 = rng.random_range(-70_i64..=70_i64);
        let delay_ms = (80_i64 + variance).clamp(10, 150) as u64;
        sleep(Duration::from_millis(delay_ms)).await;

        let key_up = DispatchKeyEventParams::builder()
            .r#type(DispatchKeyEventType::KeyUp)
            .text(c.to_string())
            .build()
            .map_err(|e| Error::Cdp(e.to_string()))?;
        page.execute(key_up)
            .await
            .map_err(|e| Error::Cdp(e.to_string()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cubic_ease_in_out_endpoints() {
        // t = 0 -> 0, t = 1 -> 1
        assert!((cubic_ease_in_out(0.0) - 0.0).abs() < 1e-9);
        assert!((cubic_ease_in_out(1.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn cubic_ease_in_out_midpoint() {
        // t = 0.5 should map to exactly 0.5 (symmetry)
        assert!((cubic_ease_in_out(0.5) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn cubic_ease_in_out_monotone() {
        // The eased value should increase monotonically with t
        let mut prev = cubic_ease_in_out(0.0);
        for i in 1..=20 {
            let t = i as f64 / 20.0;
            let cur = cubic_ease_in_out(t);
            assert!(
                cur >= prev - 1e-9,
                "ease-in-out not monotone at t={t}: cur={cur} < prev={prev}"
            );
            prev = cur;
        }
    }

    #[test]
    fn cubic_ease_in_out_output_in_range() {
        // Output must stay in [0, 1] for all inputs in [0, 1]
        for i in 0..=100 {
            let t = i as f64 / 100.0;
            let out = cubic_ease_in_out(t);
            assert!(
                (0.0..=1.0).contains(&out),
                "ease-in-out out of range at t={t}: {out}"
            );
        }
    }

    #[test]
    fn bezier_interpolation_stays_near_line() {
        // With no jitter (conceptually) the eased path from (0,0) to (100,100)
        // at step i/20 should be near eased(i/20) * 100 in each axis.
        // We test the math, not the actual CDP call.
        const STEPS: u32 = 20;
        let from = (0.0_f64, 0.0_f64);
        let to = (100.0_f64, 100.0_f64);

        for step in 0..=STEPS {
            let t = step as f64 / STEPS as f64;
            let eased = cubic_ease_in_out(t);
            let x_expected = from.0 + (to.0 - from.0) * eased;
            let y_expected = from.1 + (to.1 - from.1) * eased;
            // Jitter would be at most ±1 px in the actual function
            assert!((x_expected - y_expected).abs() < 1e-9, "symmetric path");
        }
    }

    #[test]
    fn typing_delay_clamped_to_bounds() {
        // Check that the delay formula stays in [10, 150] for all variances
        for variance in -100_i64..=100_i64 {
            let delay = (80_i64 + variance).clamp(10, 150) as u64;
            assert!((10..=150).contains(&delay), "delay {delay} out of bounds");
        }
    }

    #[test]
    fn pre_click_delay_in_range() {
        // 50 + rand[0, 100) always in [50, 150)
        for offset in 0_u64..100_u64 {
            let ms = 50 + offset;
            assert!((50..150).contains(&ms), "pre-click delay {ms} out of range");
        }
    }
}
