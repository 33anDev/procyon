//! Per-executable icon cache.
//!
//! Icons are extracted **once per exe path**, on a background worker thread,
//! and uploaded to the GPU as egui textures on the UI thread. The table asks
//! for an icon each frame via [`IconCache::get`]; that's a hash lookup. On a
//! miss it queues the path for the worker and returns `None` (the UI draws a
//! placeholder) until the icon arrives a moment later.
//!
//! On non-Windows platforms no worker is started and every lookup returns
//! `None`, so the placeholder is shown everywhere.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use egui::{ColorImage, TextureHandle, TextureId, TextureOptions};

/// Decoded icon pixels sent from the worker. `None` = extraction failed;
/// we remember that too so we never retry the same path every frame.
type IconResult = (PathBuf, Option<ColorImage>);

enum Entry {
    /// Requested, worker hasn't answered yet.
    Pending,
    Ready(TextureHandle),
    Failed,
}

pub struct IconCache {
    entries: HashMap<PathBuf, Entry>,
    /// `None` when there's no worker (non-Windows), so lookups short-circuit.
    requests: Option<Sender<PathBuf>>,
    results: Option<Receiver<IconResult>>,
}

impl IconCache {
    pub fn new(ctx: &egui::Context) -> Self {
        #[cfg(target_os = "windows")]
        {
            let (req_tx, req_rx) = std::sync::mpsc::channel::<PathBuf>();
            let (res_tx, res_rx) = std::sync::mpsc::channel::<IconResult>();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("procyon-icons".into())
                .spawn(move || win::worker(req_rx, res_tx, ctx));
            if spawned.is_ok() {
                return Self {
                    entries: HashMap::new(),
                    requests: Some(req_tx),
                    results: Some(res_rx),
                };
            }
        }

        let _ = ctx;
        Self { entries: HashMap::new(), requests: None, results: None }
    }

    /// Upload any icons the worker has finished. Call once per frame, before
    /// drawing the table.
    pub fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.results else { return };
        for (path, image) in rx.try_iter() {
            let entry = match image {
                Some(img) => {
                    let name = format!("icon:{}", path.display());
                    Entry::Ready(ctx.load_texture(name, img, TextureOptions::LINEAR))
                }
                None => Entry::Failed,
            };
            self.entries.insert(path, entry);
        }
    }

    /// Texture for `exe`'s icon, or `None` if unknown / not loaded yet /
    /// extraction failed (caller draws a placeholder).
    pub fn get(&mut self, exe: Option<&Path>) -> Option<TextureId> {
        let exe = exe?;
        let requests = self.requests.as_ref()?;
        match self.entries.get(exe) {
            Some(Entry::Ready(tex)) => Some(tex.id()),
            Some(Entry::Pending | Entry::Failed) => None,
            None => {
                // First sighting: queue it exactly once.
                let queued = requests.send(exe.to_path_buf()).is_ok();
                let state = if queued { Entry::Pending } else { Entry::Failed };
                self.entries.insert(exe.to_path_buf(), state);
                None
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod win {
    use super::IconResult;

    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{Receiver, Sender};

    use egui::ColorImage;
    use windows::core::HSTRING;
    use windows::Win32::Graphics::Gdi::{
        DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO,
        BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP,
    };
    use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_ICON, SHGFI_SMALLICON};
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};

    /// Worker loop: receive exe paths, send back decoded pixels, and nudge
    /// the UI to repaint so the new icon shows up without user input.
    pub fn worker(requests: Receiver<PathBuf>, results: Sender<IconResult>, ctx: egui::Context) {
        // SHGetFileInfo requires COM on the calling thread. The result is
        // ignored deliberately: "already initialized" is fine too.
        // SAFETY: called once at thread start, never uninitialized while
        // the thread runs (process exit cleans up).
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        }

        // Ends when the IconCache (and thus the Sender) is dropped.
        for path in requests {
            let image = extract(&path);
            if results.send((path, image)).is_err() {
                break;
            }
            ctx.request_repaint();
        }
    }

    /// Get the shell's small icon for a file, as straight-alpha RGBA.
    ///
    /// The shell returns the same icon Explorer shows, including the generic
    /// "application" icon for exes that embed none.
    fn extract(path: &Path) -> Option<ColorImage> {
        let mut info = SHFILEINFOW::default();
        // SAFETY: `info` is a valid, correctly sized out-parameter.
        let ok = unsafe {
            SHGetFileInfoW(
                &HSTRING::from(path.as_os_str()),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_SMALLICON,
            )
        };
        if ok == 0 || info.hIcon.is_invalid() {
            return None;
        }

        let image = icon_to_rgba(info.hIcon);
        // SAFETY: we own the icon returned by SHGetFileInfoW.
        unsafe {
            let _ = DestroyIcon(info.hIcon);
        }
        image
    }

    /// Convert an HICON to RGBA by reading its color (and, if needed, mask)
    /// bitmaps with GetDIBits.
    fn icon_to_rgba(icon: HICON) -> Option<ColorImage> {
        let mut ii = ICONINFO::default();
        // SAFETY: valid icon; GetIconInfo creates two bitmaps that we must free.
        unsafe { GetIconInfo(icon, &mut ii) }.ok()?;

        let result = (|| {
            // Monochrome icons have no color bitmap; treat as "no icon".
            if ii.hbmColor.is_invalid() {
                return None;
            }
            let (w, h) = bitmap_size(ii.hbmColor)?;
            let mut bgra = read_bits(ii.hbmColor, w, h)?;

            // Modern icons carry real alpha. Old ones have alpha all zero and
            // use the separate AND mask for transparency instead.
            if bgra.chunks_exact(4).all(|px| px[3] == 0) {
                let mask = read_bits(ii.hbmMask, w, h)?;
                for (px, m) in bgra.chunks_exact_mut(4).zip(mask.chunks_exact(4)) {
                    // Mask pixel set (white) means transparent.
                    px[3] = if m[0] == 0 { 255 } else { 0 };
                }
            }

            // BGRA -> RGBA.
            for px in bgra.chunks_exact_mut(4) {
                px.swap(0, 2);
            }
            Some(ColorImage::from_rgba_unmultiplied([w, h], &bgra))
        })();

        // SAFETY: both bitmaps were created by GetIconInfo and are ours.
        unsafe {
            if !ii.hbmColor.is_invalid() {
                let _ = DeleteObject(ii.hbmColor);
            }
            if !ii.hbmMask.is_invalid() {
                let _ = DeleteObject(ii.hbmMask);
            }
        }
        result
    }

    fn bitmap_size(bmp: HBITMAP) -> Option<(usize, usize)> {
        let mut bm = BITMAP::default();
        // SAFETY: `bm` is a correctly sized BITMAP out-parameter.
        let n = unsafe {
            GetObjectW(
                bmp,
                std::mem::size_of::<BITMAP>() as i32,
                Some(&mut bm as *mut BITMAP as *mut _),
            )
        };
        (n != 0 && bm.bmWidth > 0 && bm.bmHeight > 0)
            .then_some((bm.bmWidth as usize, bm.bmHeight as usize))
    }

    /// Read a bitmap as top-down 32-bit BGRA rows.
    fn read_bits(bmp: HBITMAP, w: usize, h: usize) -> Option<Vec<u8>> {
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32), // negative = top-down row order
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; w * h * 4];

        // SAFETY: the screen DC is released below; `buf` holds exactly
        // w*h 32-bit pixels as described by `bmi`.
        unsafe {
            let dc = GetDC(None);
            let lines = GetDIBits(
                dc,
                bmp,
                0,
                h as u32,
                Some(buf.as_mut_ptr().cast()),
                &mut bmi,
                DIB_RGB_COLORS,
            );
            ReleaseDC(None, dc);
            (lines == h as i32).then_some(buf)
        }
    }
}
