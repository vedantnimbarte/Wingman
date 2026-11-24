//! Headless-browser visual verification.
//!
//! Two pieces:
//! - [`diff_ratio`] — a pure, dependency-light screenshot comparison (fraction
//!   of differing pixels between two PNGs). Always available and unit-tested.
//! - [`capture`] (feature `chrome`) — drive a headless Chrome/Chromium to load
//!   a URL and take a screenshot. Behind a feature because it needs a browser
//!   binary; the diff logic doesn't.
//!
//! The verification gate loads a URL, screenshots it, and fails if it differs
//! from a committed baseline by more than a threshold — proving a UI change
//! renders, a receipt no terminal agent offers.
//!
//! [`Session`] (feature `chrome`) is the long-lived counterpart the agent's
//! `browser` tool drives across calls.

mod session;
#[cfg(feature = "chrome")]
pub use session::Session;
pub use session::{find_chrome, ConsoleBuffer, MAX_CONSOLE_LINES};

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("image decode/encode error: {0}")]
    Image(#[from] image::ImageError),
    #[error("screenshot dimensions differ: {0}x{1} vs {2}x{3}")]
    DimensionMismatch(u32, u32, u32, u32),
    #[error("browser error: {0}")]
    Browser(String),
}

pub type Result<T> = std::result::Result<T, BrowserError>;

/// Fraction of pixels that differ between two PNG-encoded screenshots, in
/// `[0.0, 1.0]`. `tolerance` (0..=255) is the per-channel delta below which two
/// pixels count as equal (absorbs anti-aliasing jitter). Errors if the two
/// images have different dimensions — a size change is itself a visual change
/// the caller should treat as a diff.
pub fn diff_ratio(a_png: &[u8], b_png: &[u8], tolerance: u8) -> Result<f64> {
    let a = image::load_from_memory(a_png)?.to_rgba8();
    let b = image::load_from_memory(b_png)?.to_rgba8();
    let (aw, ah) = a.dimensions();
    let (bw, bh) = b.dimensions();
    if aw != bw || ah != bh {
        return Err(BrowserError::DimensionMismatch(aw, ah, bw, bh));
    }
    let total = (aw as u64) * (ah as u64);
    if total == 0 {
        return Ok(0.0);
    }
    let mut differing = 0u64;
    for (pa, pb) in a.pixels().zip(b.pixels()) {
        let differs =
            pa.0.iter()
                .zip(pb.0.iter())
                .any(|(&x, &y)| x.abs_diff(y) > tolerance);
        if differs {
            differing += 1;
        }
    }
    Ok(differing as f64 / total as f64)
}

/// The result of loading a URL in a headless browser.
#[derive(Debug, Clone)]
pub struct Capture {
    /// PNG-encoded screenshot of the rendered page.
    pub screenshot_png: Vec<u8>,
    /// Console error messages captured during load (best-effort).
    pub console_errors: Vec<String>,
}

/// How [`capture_with`] renders the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureOpts {
    /// CSS pixels. Below 600 the page is emulated as a mobile device.
    pub width: u32,
    pub height: u32,
    /// Emulate `prefers-color-scheme: dark` (else `light`).
    pub dark: bool,
    /// Grow the viewport to the page's scroll height, capped at
    /// [`MAX_FULL_PAGE_HEIGHT`], so the shot covers more than the first screen.
    pub full_page: bool,
}

/// Tallest full-page shot. Vision models downscale or refuse very tall
/// images, and the top of a long page is where most problems show anyway.
pub const MAX_FULL_PAGE_HEIGHT: u32 = 3000;

/// Load `url` in a headless Chrome/Chromium and screenshot it. Requires a
/// Chrome binary on the system (headless_chrome locates or fetches one).
#[cfg(feature = "chrome")]
pub fn capture(url: &str, timeout: std::time::Duration) -> Result<Capture> {
    capture_impl(url, timeout, None)
}

/// [`capture`] at a set viewport and color scheme. `capture` itself is left
/// as it was, so screenshot baselines taken with it stay comparable.
#[cfg(feature = "chrome")]
pub fn capture_with(url: &str, timeout: std::time::Duration, opts: CaptureOpts) -> Result<Capture> {
    capture_impl(url, timeout, Some(opts))
}

#[cfg(feature = "chrome")]
fn device_metrics(
    width: u32,
    height: u32,
) -> headless_chrome::protocol::cdp::Emulation::SetDeviceMetricsOverride {
    headless_chrome::protocol::cdp::Emulation::SetDeviceMetricsOverride {
        width,
        height,
        device_scale_factor: 1.0,
        mobile: width < 600,
        scale: None,
        screen_width: None,
        screen_height: None,
        position_x: None,
        position_y: None,
        dont_set_visible_size: None,
        screen_orientation: None,
        viewport: None,
        display_feature: None,
        device_posture: None,
    }
}

#[cfg(feature = "chrome")]
fn capture_impl(
    url: &str,
    timeout: std::time::Duration,
    render: Option<CaptureOpts>,
) -> Result<Capture> {
    use headless_chrome::protocol::cdp::types::Event;
    use headless_chrome::protocol::cdp::Emulation::{MediaFeature, SetEmulatedMedia};
    use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
    use headless_chrome::{Browser, LaunchOptions};
    use std::sync::{Arc, Mutex};

    let opts = LaunchOptions::default_builder()
        .headless(true)
        .idle_browser_timeout(timeout)
        .build()
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    let browser = Browser::new(opts).map_err(|e| BrowserError::Browser(e.to_string()))?;
    let tab = browser
        .new_tab()
        .map_err(|e| BrowserError::Browser(e.to_string()))?;

    // Capture console errors + uncaught exceptions. Register the listener
    // BEFORE navigation so nothing is missed. The generated CDP event structs
    // are opaque here, so we filter on their Debug representation — robust and
    // compile-stable: keep error-level log entries and all thrown exceptions.
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let errors = errors.clone();
        tab.enable_log()
            .map_err(|e| BrowserError::Browser(e.to_string()))?;
        tab.enable_runtime()
            .map_err(|e| BrowserError::Browser(e.to_string()))?;
        let _ = tab.add_event_listener(Arc::new(move |event: &Event| match event {
            Event::LogEntryAdded(e) => {
                let s = format!("{e:?}");
                // The CDP LogEntry level enum renders as `Error` in Debug.
                if s.contains("Error") {
                    errors.lock().unwrap().push(format!("console: {s}"));
                }
            }
            Event::RuntimeExceptionThrown(e) => {
                errors.lock().unwrap().push(format!("exception: {e:?}"));
            }
            _ => {}
        }));
    }

    if let Some(o) = render {
        tab.call_method(device_metrics(o.width, o.height))
            .map_err(|e| BrowserError::Browser(e.to_string()))?;
        tab.call_method(SetEmulatedMedia {
            media: None,
            features: Some(vec![MediaFeature {
                name: "prefers-color-scheme".into(),
                value: if o.dark { "dark" } else { "light" }.into(),
            }]),
        })
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    }

    tab.navigate_to(url)
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    tab.wait_until_navigated()
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    // Give async console/exception events a moment to arrive.
    std::thread::sleep(std::time::Duration::from_millis(300));
    if let Some(o) = render.filter(|o| o.full_page) {
        // In device pixels: a page wider than a phone is zoomed out
        // (`visualViewport.scale` < 1), and its CSS height would leave the
        // bottom of the shot empty.
        let tall = tab
            .evaluate(
                "Math.ceil(document.documentElement.scrollHeight * \
                 (window.visualViewport ? window.visualViewport.scale : 1))",
                false,
            )
            .ok()
            .and_then(|r| r.value)
            .and_then(|v| v.as_f64())
            .map_or(o.height, |h| {
                (h as u32).clamp(o.height, MAX_FULL_PAGE_HEIGHT)
            });
        if tall > o.height {
            tab.call_method(device_metrics(o.width, tall))
                .map_err(|e| BrowserError::Browser(e.to_string()))?;
            // Let layout settle at the new size.
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    let png = tab
        .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    let console_errors = errors.lock().unwrap().clone();
    Ok(Capture {
        screenshot_png: png,
        console_errors,
    })
}

#[cfg(not(feature = "chrome"))]
pub fn capture(_url: &str, _timeout: std::time::Duration) -> Result<Capture> {
    Err(BrowserError::Browser(
        "browser capture requires the `chrome` feature (and a Chrome/Chromium binary)".into(),
    ))
}

#[cfg(not(feature = "chrome"))]
pub fn capture_with(
    url: &str,
    timeout: std::time::Duration,
    _opts: CaptureOpts,
) -> Result<Capture> {
    capture(url, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgba, RgbaImage};

    fn png(w: u32, h: u32, color: [u8; 4]) -> Vec<u8> {
        let mut img = RgbaImage::new(w, h);
        for p in img.pixels_mut() {
            *p = Rgba(color);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn identical_images_have_zero_diff() {
        let a = png(4, 4, [10, 20, 30, 255]);
        assert_eq!(diff_ratio(&a, &a, 0).unwrap(), 0.0);
    }

    #[test]
    fn fully_different_images_have_full_diff() {
        let a = png(4, 4, [0, 0, 0, 255]);
        let b = png(4, 4, [255, 255, 255, 255]);
        assert_eq!(diff_ratio(&a, &b, 0).unwrap(), 1.0);
    }

    #[test]
    fn tolerance_absorbs_small_jitter() {
        let a = png(4, 4, [100, 100, 100, 255]);
        let b = png(4, 4, [103, 100, 100, 255]); // 3 off on one channel
        assert_eq!(diff_ratio(&a, &b, 5).unwrap(), 0.0); // within tolerance
        assert!(diff_ratio(&a, &b, 1).unwrap() > 0.0); // outside tolerance
    }

    #[test]
    fn dimension_mismatch_errors() {
        let a = png(4, 4, [0, 0, 0, 255]);
        let b = png(8, 8, [0, 0, 0, 255]);
        assert!(matches!(
            diff_ratio(&a, &b, 0),
            Err(BrowserError::DimensionMismatch(4, 4, 8, 8))
        ));
    }
}
