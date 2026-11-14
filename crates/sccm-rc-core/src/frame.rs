//! Keeping a local copy of the session's desktop image up to date, shared by
//! the viewer and the headless MCP backend.
//!
//! Each graphics update hands over the full, always-correct composite plus
//! the region that changed. Copying only that region is cheap but drifts if a
//! region is ever under-reported, so a full copy is made every
//! [`FULL_RESYNC`] as well (cheap at ~3/s).

use std::time::{Duration, Instant};

use crate::rdp::UpdateRegion;

/// How often an update is copied in full instead of just its region.
pub const FULL_RESYNC: Duration = Duration::from_millis(300);

/// What [`FrameCopier::copy`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Copied {
    /// The whole image (first frame, size change or periodic resync); the
    /// destination now has the source's size.
    Full,
    /// Only these rows and columns (clamped to the image).
    Region(UpdateRegion),
}

/// Copies graphics updates into an RGBA destination buffer.
#[derive(Debug, Default, Clone)]
pub struct FrameCopier {
    last_full: Option<Instant>,
}

impl FrameCopier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy update `r` of the `w`x`h` RGBA image `src` into `dst`, which is
    /// `dst_w`x`dst_h`. Copies everything when the sizes differ or the last
    /// full copy is [`FULL_RESYNC`] old; the caller then takes over `w`/`h`.
    pub fn copy(
        &mut self,
        dst: &mut Vec<u8>,
        (dst_w, dst_h): (u32, u32),
        src: &[u8],
        (w, h): (u32, u32),
        r: UpdateRegion,
        now: Instant,
    ) -> Copied {
        let due = self
            .last_full
            .is_none_or(|t| now.saturating_duration_since(t) >= FULL_RESYNC);
        if dst_w != w || dst_h != h || dst.len() != src.len() || due || w == 0 || h == 0 {
            dst.clear();
            dst.extend_from_slice(src);
            self.last_full = Some(now);
            return Copied::Full;
        }
        let stride = w as usize * 4;
        let right = (r.right as usize).min(w as usize - 1);
        let bottom = (r.bottom as usize).min(h as usize - 1);
        let left = (r.left as usize).min(right);
        let top = (r.top as usize).min(bottom);
        for row in top..=bottom {
            let a = row * stride + left * 4;
            let b = row * stride + (right + 1) * 4;
            if b <= src.len() {
                dst[a..b].copy_from_slice(&src[a..b]);
            }
        }
        Copied::Region(UpdateRegion {
            left: left as u16,
            top: top as u16,
            right: right as u16,
            bottom: bottom as u16,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(left: u16, top: u16, right: u16, bottom: u16) -> UpdateRegion {
        UpdateRegion {
            left,
            top,
            right,
            bottom,
        }
    }

    fn px(buf: &[u8], x: usize, y: usize) -> u8 {
        buf[(y * 4 + x) * 4]
    }

    #[test]
    fn first_update_is_full_then_regions() {
        let t0 = Instant::now();
        let mut c = FrameCopier::new();
        let mut dst = Vec::new();
        let all = region(0, 0, 3, 1);
        assert_eq!(
            c.copy(&mut dst, (0, 0), &[1; 32], (4, 2), all, t0),
            Copied::Full
        );
        assert_eq!(dst, vec![1; 32]);
        let r = c.copy(
            &mut dst,
            (4, 2),
            &[9; 32],
            (4, 2),
            region(1, 1, 1, 1),
            t0 + Duration::from_millis(10),
        );
        assert_eq!(r, Copied::Region(region(1, 1, 1, 1)));
        assert_eq!((px(&dst, 1, 1), px(&dst, 0, 1), px(&dst, 1, 0)), (9, 1, 1));
    }

    #[test]
    fn periodic_full_copy_heals_an_under_reported_region() {
        let t0 = Instant::now();
        let mut c = FrameCopier::new();
        let mut dst = Vec::new();
        c.copy(&mut dst, (0, 0), &[1; 32], (4, 2), region(0, 0, 3, 1), t0);
        // The composite changed everywhere, but only one pixel was reported.
        let one = region(0, 0, 0, 0);
        let early = t0 + FULL_RESYNC - Duration::from_millis(1);
        assert!(matches!(
            c.copy(&mut dst, (4, 2), &[7; 32], (4, 2), one, early),
            Copied::Region(_)
        ));
        assert_eq!(px(&dst, 3, 1), 1, "drift until the resync");
        assert_eq!(
            c.copy(&mut dst, (4, 2), &[7; 32], (4, 2), one, t0 + FULL_RESYNC),
            Copied::Full
        );
        assert_eq!(dst, vec![7; 32]);
    }

    #[test]
    fn size_change_copies_everything() {
        let t0 = Instant::now();
        let mut c = FrameCopier::new();
        let mut dst = Vec::new();
        c.copy(&mut dst, (0, 0), &[1; 32], (4, 2), region(0, 0, 3, 1), t0);
        let r = c.copy(&mut dst, (4, 2), &[2; 16], (2, 2), region(0, 0, 0, 0), t0);
        assert_eq!(r, Copied::Full);
        assert_eq!(dst.len(), 16);
    }

    #[test]
    fn out_of_range_region_is_clamped() {
        let t0 = Instant::now();
        let mut c = FrameCopier::new();
        let mut dst = Vec::new();
        c.copy(&mut dst, (0, 0), &[1; 32], (4, 2), region(0, 0, 3, 1), t0);
        let r = c.copy(&mut dst, (4, 2), &[5; 32], (4, 2), region(9, 9, 99, 99), t0);
        assert_eq!(r, Copied::Region(region(3, 1, 3, 1)));
        assert_eq!(px(&dst, 3, 1), 5);
        assert_eq!(px(&dst, 2, 1), 1);
    }
}
