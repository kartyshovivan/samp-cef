use cef_sys::cef_rect_t;
use client_api::gta::matrix::CRect;
use client_api::gta::rw;
use client_api::gta::rw::rwcore::{RwRaster, RwTexture};
use client_api::gta::rw::rwplcore::{self, RwRGBA};
use client_api::gta::sprite::Sprite;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

use winapi::shared::d3d9::IDirect3DTexture9;
use winapi::shared::d3d9types::{
    D3DLOCKED_RECT, D3DPOOL_DEFAULT, D3DPOOL_MANAGED, D3DSURFACE_DESC, D3DUSAGE_DYNAMIC,
};
use winapi::shared::windef::RECT;

/// `RwD3D9RasterExtOffset` in gta_sa.exe 1.0 US: offset of the D3D9 plugin data inside RwRaster.
const D3D9_RASTER_EXT_OFFSET: usize = 0xB4E9E0;

pub struct RwLockGuard<'a> {
    bytes: &'a mut [u8],
    pub pitch: usize,
    raster: NonNull<RwRaster>,
}

impl RwLockGuard<'_> {
    #[inline(always)]
    pub fn bytes_as_mut_ptr(&mut self) -> *mut u8 {
        self.bytes.as_mut_ptr()
    }
}

impl Deref for RwLockGuard<'_> {
    type Target = [u8];

    #[inline(always)]
    fn deref(&self) -> &[u8] {
        self.bytes
    }
}

impl DerefMut for RwLockGuard<'_> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.bytes
    }
}

impl Drop for RwLockGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            self.raster.as_mut().unlock();
        }
    }
}

pub struct RwContainer {
    texture: Option<NonNull<RwTexture>>,
    raster: Option<NonNull<RwRaster>>,
    /// D3D9 texture behind `raster`. `None` = not resolved yet, `Some(None)` = unavailable.
    d3d_texture: Option<Option<NonNull<IDirect3DTexture9>>>,
}

impl RwContainer {
    pub fn new(width: usize, height: usize) -> RwContainer {
        let raster = RwRaster::new(width as i32, height as i32);
        let texture = RwTexture::new(raster);

        RwContainer {
            texture: NonNull::new(texture),
            raster: NonNull::new(raster),
            d3d_texture: None,
        }
    }

    fn d3d_texture(&mut self) -> Option<NonNull<IDirect3DTexture9>> {
        if self.d3d_texture.is_none() {
            let resolved = self.raster.and_then(|raster| unsafe { resolve_d3d_texture(raster) });

            log::trace!("RwContainer: direct D3D9 upload available: {}", resolved.is_some());
            self.d3d_texture = Some(resolved);
        }

        self.d3d_texture.flatten()
    }

    /// Uploads `rects` (already clipped to the texture) straight into the D3D9 texture,
    /// locking only those regions so the driver re-uploads just the changed pixels.
    /// Returns `false` if the fast path isn't available; nothing is guaranteed to be
    /// written in that case and the caller must fall back to a full RW lock.
    fn upload_rects(&mut self, bytes: &[u8], src_pitch: usize, rects: &[RECT]) -> bool {
        let Some(texture) = self.d3d_texture() else {
            return false;
        };

        unsafe {
            let texture = &*texture.as_ptr();

            for rect in rects {
                let mut locked = std::mem::zeroed::<D3DLOCKED_RECT>();

                if texture.LockRect(0, &mut locked, rect, 0) < 0 || locked.pBits.is_null() {
                    return false;
                }

                let dst = locked.pBits as *mut u8;
                let len = (rect.right - rect.left) as usize * 4;
                let x = rect.left as usize * 4;

                for row in 0..(rect.bottom - rect.top) as usize {
                    let src = bytes.as_ptr().add((rect.top as usize + row) * src_pitch + x);
                    let dst = dst.add(row * locked.Pitch as usize);
                    std::ptr::copy_nonoverlapping(src, dst, len);
                }

                texture.UnlockRect(0);
            }
        }

        true
    }

    #[inline]
    pub fn bytes(&mut self) -> Option<RwLockGuard<'_>> {
        unsafe {
            self.raster.as_mut().map(|raster| {
                let bytes = raster.as_mut().lock(0);
                let size = {
                    let raster = raster.as_mut();
                    raster.height * raster.width * 4
                };

                RwLockGuard {
                    bytes: std::slice::from_raw_parts_mut(bytes, size as usize),
                    pitch: raster.as_mut().stride as usize,
                    raster: *raster,
                }
            })
        }
    }
}

impl Drop for RwContainer {
    fn drop(&mut self) {
        unsafe {
            // RwTextureDestroy also destroys the texture's raster, destroying it again
            // afterwards is a double free (crash in d3d9 inside RwRasterDestroy).
            if let Some(mut texture) = self.texture.take() {
                texture.as_mut().destroy();
                self.raster = None;
            }

            if let Some(mut raster) = self.raster.take() {
                raster.as_mut().destroy();
            }
        }
    }
}

/// Finds the `IDirect3DTexture9` of an RW raster. The plugin-data offset is verified by
/// locking the raster through RW and checking that the D3D9 extension recorded the same
/// locked pointer and pitch, then the texture's description is checked as well.
unsafe fn resolve_d3d_texture(raster: NonNull<RwRaster>) -> Option<NonNull<IDirect3DTexture9>> {
    unsafe {
        let ext_offset = *(D3D9_RASTER_EXT_OFFSET as *const i32);

        if !(0x20..=0x400).contains(&ext_offset) {
            return None;
        }

        let raster = &mut *raster.as_ptr();
        let ext = (raster as *mut RwRaster as *mut u8).add(ext_offset as usize);

        let bits = raster.lock(0);
        if bits.is_null() {
            return None;
        }

        let stride = raster.stride;
        let verified = (4..48).step_by(4).any(|i| {
            let pitch = *(ext.add(i) as *const i32);
            let pbits = *(ext.add(i + 4) as *const *mut u8);
            pitch == stride && pbits == bits
        });

        raster.unlock();

        if !verified {
            return None;
        }

        let texture = NonNull::new(*(ext as *const *mut IDirect3DTexture9))?;

        let mut desc = std::mem::zeroed::<D3DSURFACE_DESC>();
        if (*texture.as_ptr()).GetLevelDesc(0, &mut desc) < 0
            || desc.Width as i32 != raster.width
            || desc.Height as i32 != raster.height
        {
            return None;
        }

        let lockable = desc.Pool == D3DPOOL_MANAGED
            || (desc.Pool == D3DPOOL_DEFAULT && desc.Usage & D3DUSAGE_DYNAMIC != 0);

        lockable.then_some(texture)
    }
}

pub struct SpriteContainer {
    sprite: Sprite,
    rw: RwContainer,
}

impl SpriteContainer {
    pub fn new(width: usize, height: usize) -> SpriteContainer {
        let rw = RwContainer::new(width, height);
        let mut sprite = Sprite::new();

        if let Some(texture) = rw.texture {
            sprite.set_texture(texture.as_ptr());
        }

        SpriteContainer { sprite, rw }
    }
}

impl Drop for SpriteContainer {
    fn drop(&mut self) {
        // The texture belongs to `rw`. CSprite2d's destructor would destroy it too,
        // so detach it before `sprite` is dropped.
        self.sprite.set_texture(std::ptr::null_mut());
    }
}

impl SpriteContainer {
    #[inline]
    pub fn draw(&mut self) {
        let client = crate::utils::client_rect();
        let rect = CRect {
            top: 0.0,
            left: 0.0,
            right: client[0] as f32,
            bottom: client[1] as f32,
        };

        let color = RwRGBA {
            red: 0xFF,
            green: 0xFF,
            blue: 0xFF,
            alpha: 0xFF,
        };

        let prev = rw::render_state(rwplcore::RENDERSTATETEXTUREFILTER);

        rw::set_render_state(rwplcore::RENDERSTATETEXTUREFILTER, rwplcore::FILTERNEAREST);
        self.sprite.draw(rect, color);

        rw::set_render_state(rwplcore::RENDERSTATETEXTUREFILTER, prev);
    }

    #[inline]
    pub fn bytes(&mut self) -> Option<RwLockGuard<'_>> {
        self.rw.bytes()
    }
}

enum ViewContainer {
    Material(RwContainer),
    Display(SpriteContainer),
}

impl ViewContainer {
    fn draw(&mut self) {
        if let ViewContainer::Display(sprite) = self {
            sprite.draw();
        }
    }

    fn bytes(&mut self) -> Option<RwLockGuard<'_>> {
        match self {
            ViewContainer::Display(sprite) => sprite.bytes(),
            ViewContainer::Material(sprite) => sprite.bytes(),
        }
    }

    fn rw_mut(&mut self) -> &mut RwContainer {
        match self {
            ViewContainer::Display(sprite) => &mut sprite.rw,
            ViewContainer::Material(rw) => rw,
        }
    }

    fn texture(&self) -> Option<NonNull<RwTexture>> {
        match self {
            ViewContainer::Display(sprite) => sprite.rw.texture,
            ViewContainer::Material(rw) => rw.texture,
        }
    }
}

pub struct View {
    container: Option<ViewContainer>,
    width: usize,
    height: usize,
    active: bool,
}

impl View {
    pub fn new() -> View {
        View {
            container: None,
            width: 0,
            height: 0,
            active: true,
        }
    }

    pub fn make_display(&mut self, width: usize, height: usize) {
        let width = std::cmp::max(1, width);
        let height = std::cmp::max(1, height);

        self.destroy_previous();

        self.container = Some(ViewContainer::Display(SpriteContainer::new(width, height)));

        self.set_size(width, height);
    }

    #[inline(never)]
    pub fn make_inactive(&mut self) {
        self.destroy_previous();
        self.set_size(1, 1);
        self.active = false;
    }

    pub fn make_active(&mut self) {
        self.active = true;
    }

    #[inline]
    pub fn draw(&mut self) {
        if let Some(rw) = self.container.as_mut() {
            rw.draw()
        }
    }

    #[inline(always)]
    pub fn update_texture(&mut self, bytes: &[u8], src_width: usize, rects: &[cef_rect_t]) {
        let (view_width, view_height) = (self.width, self.height);

        let Some(container) = self.container.as_mut() else {
            return;
        };

        let src_pitch = src_width * 4;
        let max_w = std::cmp::min(view_width, src_width) as i32;
        let max_h = std::cmp::min(view_height, bytes.len() / src_pitch.max(1)) as i32;

        let clipped: Vec<RECT> = rects
            .iter()
            .filter_map(|r| {
                let left = r.x.max(0);
                let top = r.y.max(0);
                let right = (r.x + r.width).min(max_w);
                let bottom = (r.y + r.height).min(max_h);

                (right > left && bottom > top).then_some(RECT {
                    left,
                    top,
                    right,
                    bottom,
                })
            })
            .collect();

        if clipped.is_empty() {
            return;
        }

        if container.rw_mut().upload_rects(bytes, src_pitch, &clipped) {
            return;
        }

        // Fallback: lock the whole raster through RenderWare.
        if let Some(mut dest) = container.bytes() {
            let dst_pitch = dest.pitch;
            let dest = dest.bytes_as_mut_ptr();
            let pixels_origin = bytes.as_ptr();

            for rect in &clipped {
                let len = (rect.right - rect.left) as usize * 4;
                let x = rect.left as usize * 4;

                for y in rect.top as usize..rect.bottom as usize {
                    unsafe {
                        let ptr = dest.add(dst_pitch * y + x);
                        let pixels = pixels_origin.add(src_pitch * y + x);
                        std::ptr::copy_nonoverlapping(pixels, ptr, len);
                    }
                }
            }
        }
    }

    pub fn update_popup(&mut self, bytes: &[u8], popup_rect: &cef_rect_t) {
        let set_pixels = |dest: &mut [u8], pitch: usize| {
            let dest = dest.as_mut_ptr();
            let popup_pitch = popup_rect.width * 4;

            for y in 0..popup_rect.height {
                let source_index = y * popup_pitch;
                let dest_index = (y + popup_rect.y) * pitch as i32 + popup_rect.x * 4;

                unsafe {
                    let surface_data = dest.add(dest_index as usize);
                    let new_data = bytes.as_ptr().add(source_index as usize);

                    std::ptr::copy(new_data, surface_data, popup_pitch as usize);
                }
            }
        };

        self.set_texture_bytes(set_pixels);
    }

    pub fn clear(&mut self) {
        let clear = |dest: &mut [u8], _: usize| {
            let size = dest.len();
            let dest = dest.as_mut_ptr();

            unsafe {
                std::ptr::write_bytes(dest, 0x00, size);
            }
        };

        self.set_texture_bytes(clear);
    }

    pub fn on_lost_device(&mut self) {
        self.destroy_previous();
    }

    pub fn resize(&mut self, is_object: bool, width: usize, height: usize) {
        if !self.active {
            return;
        }

        let should_replace = self.active && self.container.is_none();

        if self.width == width && self.height == height && !should_replace {
            return;
        }

        let width = std::cmp::max(1, width);
        let height = std::cmp::max(1, height);

        self.destroy_previous();
        self.set_size(width, height);

        if is_object {
            self.container = Some(ViewContainer::Material(RwContainer::new(width, height)));
        } else {
            self.container = Some(ViewContainer::Display(SpriteContainer::new(width, height)));
        }
    }

    pub fn rect(&self) -> cef_rect_t {
        let width = if self.width == 0 {
            1
        } else {
            self.width as i32
        };

        let height = if self.height == 0 {
            1
        } else {
            self.height as i32
        };

        cef_rect_t {
            width,
            height,
            x: 0,
            y: 0,
        }
    }

    pub fn rwtexture(&mut self) -> Option<NonNull<RwTexture>> {
        self.container.as_mut().and_then(|rw| rw.texture())
    }

    pub fn is_empty(&self) -> bool {
        // self.render_mode == RenderMode::Empty
        false
    }

    fn destroy_previous(&mut self) {
        self.container.take();
    }

    fn set_size(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
    }

    #[inline(always)]
    fn set_texture_bytes<F>(&mut self, mut func: F)
    where
        F: FnMut(&mut [u8], usize),
    {
        if let Some(mut bytes) = self.container.as_mut().and_then(|rw| rw.bytes()) {
            let pitch = bytes.pitch;
            func(&mut bytes, pitch);
        }
    }
}

impl Default for View {
    fn default() -> Self {
        Self::new()
    }
}
