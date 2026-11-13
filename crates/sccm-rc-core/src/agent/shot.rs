//! Screenshot space: what the script client sees, and how points in the
//! delivered image map back to desktop pixels.

use image::{imageops::FilterType, ImageBuffer, ImageEncoder, Rgb};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShotError {
    #[error("point lies outside the screenshot")]
    OutOfBounds,
    #[error("desktop size changed since the screenshot was taken")]
    StaleShot,
    #[error("region is empty or outside the desktop")]
    BadRegion,
    #[error("frame buffer has the wrong size")]
    BadFrame,
    #[error("png encoding failed: {0}")]
    Encode(String),
}

/// Geometry of a delivered screenshot relative to the desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShotSpace {
    pub desk_w: u16,
    pub desk_h: u16,
    pub crop: Rect,
    pub img_w: u32,
    pub img_h: u32,
}

impl ShotSpace {
    /// Desktop pixels per image pixel (>= 1.0; never upscaled).
    pub fn scale(&self) -> f64 {
        self.crop.w as f64 / self.img_w as f64
    }

    /// Map a point in the delivered image back to desktop pixels.
    pub fn to_desktop_checked(
        &self,
        x: f64,
        y: f64,
        cur_w: u16,
        cur_h: u16,
    ) -> Result<(u16, u16), ShotError> {
        if (cur_w, cur_h) != (self.desk_w, self.desk_h) {
            return Err(ShotError::StaleShot);
        }
        let (iw, ih) = (self.img_w as f64, self.img_h as f64);
        if !(x.is_finite() && y.is_finite()) || x < 0.0 || y < 0.0 || x > iw || y > ih {
            return Err(ShotError::OutOfBounds);
        }
        let dx = self.crop.x as f64 + x * (self.crop.w as f64 / iw);
        let dy = self.crop.y as f64 + y * (self.crop.h as f64 / ih);
        let max_x = (self.crop.x as u32 + (self.crop.w as u32).saturating_sub(1)) as f64;
        let max_y = (self.crop.y as u32 + (self.crop.h as u32).saturating_sub(1)) as f64;
        Ok((dx.round().min(max_x) as u16, dy.round().min(max_y) as u16))
    }
}

/// Map fractional coordinates (0.0..=1.0) of the whole desktop to pixels.
pub fn relative_to_desktop(
    rx: f64,
    ry: f64,
    desk_w: u16,
    desk_h: u16,
) -> Result<(u16, u16), ShotError> {
    if !(rx.is_finite() && ry.is_finite())
        || !(0.0..=1.0).contains(&rx)
        || !(0.0..=1.0).contains(&ry)
    {
        return Err(ShotError::OutOfBounds);
    }
    if desk_w == 0 || desk_h == 0 {
        return Err(ShotError::BadRegion);
    }
    let x = (rx * (desk_w - 1) as f64).round() as u16;
    let y = (ry * (desk_h - 1) as f64).round() as u16;
    Ok((x, y))
}

/// Crop (optional region), downscale to `max_width` (never upscale) and
/// PNG-encode an RGBA8 frame.
pub fn take(
    rgba: &[u8],
    desk_w: u16,
    desk_h: u16,
    region: Option<Rect>,
    max_width: u32,
) -> Result<(Vec<u8>, ShotSpace), ShotError> {
    if desk_w == 0 || desk_h == 0 || rgba.len() != desk_w as usize * desk_h as usize * 4 {
        return Err(ShotError::BadFrame);
    }
    let crop = region.unwrap_or(Rect {
        x: 0,
        y: 0,
        w: desk_w,
        h: desk_h,
    });
    if crop.w == 0
        || crop.h == 0
        || crop.x as u32 + crop.w as u32 > desk_w as u32
        || crop.y as u32 + crop.h as u32 > desk_h as u32
    {
        return Err(ShotError::BadRegion);
    }

    let stride = desk_w as usize * 4;
    // Alpha is dropped: live frames may carry alpha 0, and the PNG must be opaque.
    let mut buf = Vec::with_capacity(crop.w as usize * crop.h as usize * 3);
    for row in crop.y as usize..crop.y as usize + crop.h as usize {
        let start = row * stride + crop.x as usize * 4;
        for px in rgba[start..start + crop.w as usize * 4].as_chunks::<4>().0 {
            buf.extend_from_slice(&px[..3]);
        }
    }
    let cropped: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_raw(crop.w as u32, crop.h as u32, buf).ok_or(ShotError::BadFrame)?;

    let max_w = max_width.max(1);
    let (img_w, img_h, pixels) = if (crop.w as u32) > max_w {
        let h = ((crop.h as f64 * max_w as f64 / crop.w as f64).round() as u32).max(1);
        let r = image::imageops::resize(&cropped, max_w, h, FilterType::Triangle);
        (max_w, h, r.into_raw())
    } else {
        (crop.w as u32, crop.h as u32, cropped.into_raw())
    };

    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&pixels, img_w, img_h, image::ExtendedColorType::Rgb8)
        .map_err(|e| ShotError::Encode(e.to_string()))?;

    Ok((
        png,
        ShotSpace {
            desk_w,
            desk_h,
            crop,
            img_w,
            img_h,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: u16, h: u16) -> Vec<u8> {
        vec![0x80; w as usize * h as usize * 4]
    }

    #[test]
    fn downscales_to_max_width() {
        let (png, s) = take(&frame(1920, 1080), 1920, 1080, None, 1280).unwrap();
        assert_eq!((s.img_w, s.img_h), (1280, 720));
        assert!((s.scale() - 1.5).abs() < 1e-9);
        assert_eq!(&png[1..4], b"PNG");
    }

    #[test]
    fn never_upscales() {
        let (_, s) = take(&frame(1024, 768), 1024, 768, None, 1280).unwrap();
        assert_eq!((s.img_w, s.img_h), (1024, 768));
        assert_eq!(s.scale(), 1.0);
    }

    #[test]
    fn click_maps_back_through_scale() {
        let (_, s) = take(&frame(1920, 1080), 1920, 1080, None, 1280).unwrap();
        assert_eq!(
            s.to_desktop_checked(640.0, 360.0, 1920, 1080),
            Ok((960, 540))
        );
    }

    #[test]
    fn click_maps_back_through_crop() {
        let r = Rect {
            x: 100,
            y: 50,
            w: 200,
            h: 100,
        };
        let (_, s) = take(&frame(1920, 1080), 1920, 1080, Some(r), 1280).unwrap();
        assert_eq!((s.img_w, s.img_h), (200, 100));
        assert_eq!(s.to_desktop_checked(10.0, 10.0, 1920, 1080), Ok((110, 60)));
        assert_eq!(
            s.to_desktop_checked(201.0, 10.0, 1920, 1080),
            Err(ShotError::OutOfBounds)
        );
        assert_eq!(
            s.to_desktop_checked(200.0, 100.0, 1920, 1080),
            Ok((299, 149))
        );
    }

    #[test]
    fn stale_shot_rejected() {
        let (_, s) = take(&frame(100, 100), 100, 100, None, 1280).unwrap();
        assert_eq!(
            s.to_desktop_checked(1.0, 1.0, 120, 100),
            Err(ShotError::StaleShot)
        );
    }

    #[test]
    fn relative_corners() {
        assert_eq!(relative_to_desktop(0.0, 0.0, 800, 600), Ok((0, 0)));
        assert_eq!(relative_to_desktop(1.0, 1.0, 800, 600), Ok((799, 599)));
        assert_eq!(
            relative_to_desktop(1.2, 0.5, 800, 600),
            Err(ShotError::OutOfBounds)
        );
    }

    #[test]
    fn bad_region_rejected() {
        let r = Rect {
            x: 90,
            y: 0,
            w: 20,
            h: 10,
        };
        assert_eq!(
            take(&frame(100, 100), 100, 100, Some(r), 1280).unwrap_err(),
            ShotError::BadRegion
        );
    }

    #[test]
    fn png_is_opaque() {
        let rgba = vec![0u8; 8 * 8 * 4]; // alpha 0 everywhere
        let (png, _) = take(&rgba, 8, 8, None, 1280).unwrap();
        let img = image::load_from_memory(&png).unwrap();
        assert!(!img.color().has_alpha());
        assert!(img.to_rgba8().pixels().all(|p| p.0[3] == 255));
    }

    #[test]
    fn wrong_buffer_length_is_an_error() {
        assert_eq!(
            take(&[0u8; 10], 100, 100, None, 1280).unwrap_err(),
            ShotError::BadFrame
        );
    }
}
