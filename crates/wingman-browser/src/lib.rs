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

/// Load `url` in a headless Chrome/Chromium and screenshot it. Requires a
/// Chrome binary on the system (headless_chrome locates or fetches one).
#[cfg(feature = "chrome")]
pub fn capture(url: &str, timeout: std::time::Duration) -> Result<Capture> {
    use headless_chrome::protocol::cdp::Page::CaptureScreenshotFormatOption;
    use headless_chrome::{Browser, LaunchOptions};

    let opts = LaunchOptions::default_builder()
        .headless(true)
        .idle_browser_timeout(timeout)
        .build()
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    let browser = Browser::new(opts).map_err(|e| BrowserError::Browser(e.to_string()))?;
    let tab = browser
        .new_tab()
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    tab.navigate_to(url)
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    tab.wait_until_navigated()
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    let png = tab
        .capture_screenshot(CaptureScreenshotFormatOption::Png, None, None, true)
        .map_err(|e| BrowserError::Browser(e.to_string()))?;
    Ok(Capture {
        screenshot_png: png,
        // Console-error capture needs a pre-navigation event listener; left
        // best-effort empty for now (the screenshot diff is the primary signal).
        console_errors: Vec::new(),
    })
}

#[cfg(not(feature = "chrome"))]
pub fn capture(_url: &str, _timeout: std::time::Duration) -> Result<Capture> {
    Err(BrowserError::Browser(
        "browser capture requires the `chrome` feature (and a Chrome/Chromium binary)".into(),
    ))
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
