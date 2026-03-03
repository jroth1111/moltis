//! CDP emulation overrides: device metrics, geolocation, timezone, and locale.
//!
//! All functions wrap individual CDP `Emulation.*` commands. They are
//! stateless helpers; the caller is responsible for tracking which overrides
//! are active.

use chromiumoxide::{
    Page,
    cdp::browser_protocol::emulation::{
        ClearDeviceMetricsOverrideParams, SetDeviceMetricsOverrideParams,
        SetGeolocationOverrideParams, SetLocaleOverrideParams, SetTimezoneOverrideParams,
    },
};

use crate::error::Error;

/// Override viewport size, device scale factor, and mobile emulation.
///
/// Persists until [`clear_device_override`] is called or the browser is closed.
pub async fn set_device(
    page: &Page,
    width: u32,
    height: u32,
    device_scale_factor: f64,
    mobile: bool,
) -> Result<(), Error> {
    let params = SetDeviceMetricsOverrideParams::new(
        width as i64,
        height as i64,
        device_scale_factor,
        mobile,
    );

    page.execute(params)
        .await
        .map_err(|e| Error::Cdp(format!("Emulation.setDeviceMetricsOverride failed: {e}")))?;

    Ok(())
}

/// Override the GPS geolocation reported to the page.
pub async fn set_geolocation(
    page: &Page,
    latitude: f64,
    longitude: f64,
    accuracy: f64,
) -> Result<(), Error> {
    let params = SetGeolocationOverrideParams::builder()
        .latitude(latitude)
        .longitude(longitude)
        .accuracy(accuracy)
        .build();

    page.execute(params)
        .await
        .map_err(|e| Error::Cdp(format!("Emulation.setGeolocationOverride failed: {e}")))?;

    Ok(())
}

/// Override the timezone reported to the page.
///
/// `timezone_id` must be an ICU timezone identifier, e.g. `"America/New_York"`.
/// Pass an empty string to clear the override and restore host timezone.
pub async fn set_timezone(page: &Page, timezone_id: &str) -> Result<(), Error> {
    let params = SetTimezoneOverrideParams::new(timezone_id);

    page.execute(params)
        .await
        .map_err(|e| Error::Cdp(format!("Emulation.setTimezoneOverride failed: {e}")))?;

    Ok(())
}

/// Override the locale reported to the page.
///
/// `locale` must be an ICU style C locale, e.g. `"en_US"`.
/// Pass an empty string (or omit) to restore the host locale.
pub async fn set_locale(page: &Page, locale: &str) -> Result<(), Error> {
    let params = SetLocaleOverrideParams::builder().locale(locale).build();

    page.execute(params)
        .await
        .map_err(|e| Error::Cdp(format!("Emulation.setLocaleOverride failed: {e}")))?;

    Ok(())
}

/// Clear any active device metrics override and restore original viewport.
pub async fn clear_device_override(page: &Page) -> Result<(), Error> {
    page.execute(ClearDeviceMetricsOverrideParams::default())
        .await
        .map_err(|e| Error::Cdp(format!("Emulation.clearDeviceMetricsOverride failed: {e}")))?;

    Ok(())
}
