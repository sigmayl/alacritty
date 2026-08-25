//! KWin shadow support for Wayland client-side decorations.

use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsFd;

use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_registry, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols_plasma::shadow::client::{
    org_kde_kwin_shadow, org_kde_kwin_shadow_manager,
};

use super::decorations::CORNER_RADIUS;

// Breeze's default "Large" shadow parameters. The pixels are generated independently here since
// Breeze itself is GPL licensed, while Alacritty is Apache licensed.
const SHADOW_EXTENT: f64 = 68.;
const SHADOW_OVERLAP: f64 = 3.;
const SHADOW_VERTICAL_OFFSET: f64 = 12.;
const PRIMARY_RADIUS: f32 = 48.;
const PRIMARY_OPACITY: f32 = 0.8;
const SECONDARY_RADIUS: f32 = 24.;
const SECONDARY_RELATIVE_OFFSET_Y: f32 = -6.;
const SECONDARY_OPACITY: f32 = 0.2;

#[derive(Clone, Copy)]
struct ShadowGeometry {
    left: u32,
    top: u32,
    right: u32,
    bottom: u32,
    corner_radius: u32,
}

impl ShadowGeometry {
    fn new() -> Self {
        // KWin interprets shadow buffer pixels and offsets directly in surface coordinates;
        // shadow buffers have no buffer scale. Keep them in logical pixels so their corner
        // follows the window's logical corner radius at fractional DPI.
        let logical = |value: f64| value.round().max(1.) as u32;
        let side = SHADOW_EXTENT - SHADOW_OVERLAP;
        Self {
            left: logical(side),
            top: logical(side - SHADOW_VERTICAL_OFFSET),
            right: logical(side),
            bottom: logical(side + SHADOW_VERTICAL_OFFSET),
            corner_radius: logical(CORNER_RADIUS),
        }
    }
}

struct State;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: org_kde_kwin_shadow_manager::OrgKdeKwinShadowManager);
delegate_noop!(State: org_kde_kwin_shadow::OrgKdeKwinShadow);

/// Shadow attached to a foreign winit Wayland surface.
pub struct KdeShadow {
    connection: Connection,
    event_queue: EventQueue<State>,
    manager: org_kde_kwin_shadow_manager::OrgKdeKwinShadowManager,
    surface: wl_surface::WlSurface,
    shadow: Option<org_kde_kwin_shadow::OrgKdeKwinShadow>,
    buffers: Vec<wl_buffer::WlBuffer>,
    enabled: bool,
}

impl KdeShadow {
    /// Attach a shadow when the compositor advertises KWin's shadow protocol.
    ///
    /// # Safety
    ///
    /// The display and surface pointers must remain valid for the lifetime of this value.
    pub unsafe fn new(
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
        _scale_factor: f64,
    ) -> Option<Self> {
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);
        let surface_id = match unsafe {
            ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.cast())
        } {
            Ok(surface_id) => surface_id,
            Err(err) => {
                log::warn!("Unable to import Wayland surface for KWin shadow: {err}");
                return None;
            },
        };
        let surface = match wl_surface::WlSurface::from_id(&connection, surface_id) {
            Ok(surface) => surface,
            Err(err) => {
                log::warn!("Unable to create Wayland surface proxy for KWin shadow: {err}");
                return None;
            },
        };
        let (globals, event_queue) = match registry_queue_init::<State>(&connection) {
            Ok(registry) => registry,
            Err(err) => {
                log::warn!("Unable to read Wayland globals for KWin shadow: {err}");
                return None;
            },
        };
        let qh = event_queue.handle();
        let manager: org_kde_kwin_shadow_manager::OrgKdeKwinShadowManager =
            match globals.bind(&qh, 1..=2, ()) {
                Ok(manager) => manager,
                Err(err) => {
                    log::debug!("KWin shadow protocol is unavailable: {err}");
                    return None;
                },
            };
        let shm: wl_shm::WlShm = match globals.bind(&qh, 1..=1, ()) {
            Ok(shm) => shm,
            Err(err) => {
                log::warn!("Unable to bind Wayland shared memory for KWin shadow: {err}");
                return None;
            },
        };

        let geometry = ShadowGeometry::new();
        let buffers = match create_buffers(&shm, &qh, geometry) {
            Ok(buffers) => buffers,
            Err(err) => {
                log::warn!("Unable to allocate KWin shadow buffers: {err}");
                return None;
            },
        };
        let shadow = attach_shadow(&manager, &surface, &buffers, &qh, geometry);
        surface.commit();
        let _ = connection.flush();

        Some(Self {
            connection,
            event_queue,
            manager,
            surface,
            shadow: Some(shadow),
            buffers,
            enabled: true,
        })
    }

    pub fn update(&mut self, enabled: bool, _scale_factor: f64) {
        let geometry = ShadowGeometry::new();
        if enabled == self.enabled {
            let _ = self.event_queue.dispatch_pending(&mut State);
            return;
        }

        if enabled {
            self.shadow = Some(attach_shadow(
                &self.manager,
                &self.surface,
                &self.buffers,
                &self.event_queue.handle(),
                geometry,
            ));
        } else {
            self.manager.unset(&self.surface);
            if let Some(shadow) = self.shadow.take() {
                shadow.destroy();
            }
        }
        self.surface.commit();
        let _ = self.connection.flush();
        self.enabled = enabled;
    }
}

impl Drop for KdeShadow {
    fn drop(&mut self) {
        self.manager.unset(&self.surface);
        self.surface.commit();
        if let Some(shadow) = self.shadow.take() {
            shadow.destroy();
        }
        for buffer in &self.buffers {
            buffer.destroy();
        }
        if self.manager.version() >= 2 {
            self.manager.destroy();
        }
        let _ = self.connection.flush();
    }
}

fn attach_shadow(
    manager: &org_kde_kwin_shadow_manager::OrgKdeKwinShadowManager,
    surface: &wl_surface::WlSurface,
    buffers: &[wl_buffer::WlBuffer],
    qh: &QueueHandle<State>,
    geometry: ShadowGeometry,
) -> org_kde_kwin_shadow::OrgKdeKwinShadow {
    let shadow = manager.create(surface, qh, ());
    shadow.attach_left(&buffers[0]);
    shadow.attach_top_left(&buffers[1]);
    shadow.attach_top(&buffers[2]);
    shadow.attach_top_right(&buffers[3]);
    shadow.attach_right(&buffers[4]);
    shadow.attach_bottom_right(&buffers[5]);
    shadow.attach_bottom(&buffers[6]);
    shadow.attach_bottom_left(&buffers[7]);
    shadow.set_left_offset(f64::from(geometry.left));
    shadow.set_top_offset(f64::from(geometry.top));
    shadow.set_right_offset(f64::from(geometry.right));
    shadow.set_bottom_offset(f64::from(geometry.bottom));
    shadow.commit();
    shadow
}

fn create_buffers(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    geometry: ShadowGeometry,
) -> std::io::Result<Vec<wl_buffer::WlBuffer>> {
    let radius = geometry.corner_radius;
    let dimensions = [
        (geometry.left, 1),
        (geometry.left + radius, geometry.top + radius),
        (1, geometry.top),
        (geometry.right + radius, geometry.top + radius),
        (geometry.right, 1),
        (geometry.right + radius, geometry.bottom + radius),
        (1, geometry.bottom),
        (geometry.left + radius, geometry.bottom + radius),
    ];
    let mut buffers = Vec::with_capacity(dimensions.len());

    for (index, (width, height)) in dimensions.into_iter().enumerate() {
        let pixels = shadow_pixels(index, width, height, geometry);
        let mut file = tempfile::tempfile()?;
        file.set_len(pixels.len() as u64)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&pixels)?;

        let pool = shm.create_pool(file.as_fd(), pixels.len() as i32, qh, ());
        buffers.push(pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        ));
        pool.destroy();
    }

    Ok(buffers)
}

fn shadow_pixels(
    index: usize,
    width: u32,
    height: u32,
    geometry: ShadowGeometry,
) -> Vec<u8> {
    let mut pixels = vec![0; (width * height * 4) as usize];
    for y in 0..height {
        for x in 0..width {
            let pixel_offset = ((y * width + x) * 4 + 3) as usize;
            let x = x as f32 + 0.5;
            let y = y as f32 + 0.5;
            let radius = geometry.corner_radius as f32;
            let (x, y) = match index {
                0 => (x - geometry.left as f32, 0.),
                1 => (x - geometry.left as f32, y - geometry.top as f32),
                2 => (0., y - geometry.top as f32),
                3 => (x - radius, y - geometry.top as f32),
                4 => (x, 0.),
                5 => (x - radius, y - radius),
                6 => (0., y),
                _ => (x - geometry.left as f32, y - radius),
            };

            // Never leave shadow pixels underneath the window. Breeze can overlap an opaque
            // decoration to hide fractional-scale seams, but that overlap shows through
            // Alacritty's translucent background as a dark step around rounded corners.
            let mask_distance = signed_distance(index, x, y, radius, 0.);
            let alpha = if mask_distance < 0. {
                0.
            } else {
                let primary_radius = PRIMARY_RADIUS;
                let secondary_radius = SECONDARY_RADIUS;
                let primary_offset = SHADOW_VERTICAL_OFFSET as f32;
                let secondary_offset = primary_offset + SECONDARY_RELATIVE_OFFSET_Y;
                let primary = PRIMARY_OPACITY
                    * gaussian_tail(
                        signed_distance(index, x, y, radius, primary_offset),
                        primary_radius,
                    );
                let secondary = SECONDARY_OPACITY
                    * gaussian_tail(
                        signed_distance(index, x, y, radius, secondary_offset),
                        secondary_radius,
                    );
                (primary + secondary).clamp(0., 1.)
            };
            let alpha = (255. * alpha).round() as u8;
            pixels[pixel_offset] = alpha;
        }
    }
    pixels
}

fn signed_distance(index: usize, x: f32, y: f32, radius: f32, offset_y: f32) -> f32 {
    match index {
        0 => -x,
        1 => (x - radius).hypot(y - radius - offset_y) - radius,
        2 => offset_y - y,
        3 => (x + radius).hypot(y - radius - offset_y) - radius,
        4 => x,
        5 => (x + radius).hypot(y + radius - offset_y) - radius,
        6 => y - offset_y,
        _ => (x - radius).hypot(y + radius - offset_y) - radius,
    }
}

/// Approximate the integral of a Gaussian from `distance` to infinity.
fn gaussian_tail(distance: f32, blur_radius: f32) -> f32 {
    let standard_deviation = blur_radius * 0.5;
    let value = distance / (standard_deviation * std::f32::consts::SQRT_2);
    0.5 * complementary_error_function(value)
}

fn complementary_error_function(value: f32) -> f32 {
    // Abramowitz and Stegun 7.1.26, with a maximum error around 1.5e-7.
    let sign = value.signum();
    let value = value.abs();
    let t = 1. / value.mul_add(0.327_591_1, 1.);
    let polynomial = 1.061_405_4_f32
        .mul_add(t, -1.453_152_1)
        .mul_add(t, 1.421_413_8)
        .mul_add(t, -0.284_496_72)
        .mul_add(t, 0.254_829_6)
        * t;
    let error = 1. - polynomial * (-value * value).exp();
    1. - sign * error
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shadow_fades_away_from_window() {
        let geometry = ShadowGeometry::new();
        let pixels = shadow_pixels(2, 1, geometry.top, geometry);
        let outer_alpha = pixels[3];
        let inner_alpha = pixels[((geometry.top - 1) * 4 + 3) as usize];
        assert!(inner_alpha > outer_alpha);
    }

    #[test]
    fn corner_is_radial() {
        let geometry = ShadowGeometry::new();
        let width = geometry.left + geometry.corner_radius;
        let height = geometry.top + geometry.corner_radius;
        let pixels = shadow_pixels(1, width, height, geometry);
        let outer_alpha = pixels[3];
        let x = geometry.left - 1;
        let y = geometry.top - 1;
        let corner_alpha = pixels[((y * width + x) * 4 + 3) as usize];
        assert_eq!(outer_alpha, 0);
        assert!(corner_alpha > 0);
    }

    #[test]
    fn corner_shadow_follows_window_rounding() {
        let geometry = ShadowGeometry::new();
        let width = geometry.left + geometry.corner_radius;
        let height = geometry.top + geometry.corner_radius;
        let edge = shadow_pixels(2, 1, geometry.top, geometry);
        let corner = shadow_pixels(1, width, height, geometry);
        let edge_inner_alpha = edge[((geometry.top - 1) * 4 + 3) as usize];
        let x = geometry.left - 1;
        let y = geometry.top - 1;
        let corner_at_sharp_point = corner[((y * width + x) * 4 + 3) as usize];
        assert!(corner_at_sharp_point < edge_inner_alpha);
    }

    #[test]
    fn corner_shadow_does_not_overlap_translucent_window() {
        let geometry = ShadowGeometry::new();
        let width = geometry.left + geometry.corner_radius;
        let height = geometry.top + geometry.corner_radius;
        let corner = shadow_pixels(1, width, height, geometry);

        // This pixel is just inside the rounded surface boundary. Any shadow alpha here would
        // show through a translucent window as a dark fringe.
        let x = geometry.left;
        let y = geometry.top + geometry.corner_radius - 1;
        assert_eq!(corner[((y * width + x) * 4 + 3) as usize], 0);
    }

    #[test]
    fn breeze_shadow_extends_further_below_window() {
        let geometry = ShadowGeometry::new();
        assert!(geometry.bottom > geometry.top);
        assert_eq!(geometry.left, geometry.right);
    }

    #[test]
    fn shadow_geometry_uses_logical_surface_coordinates() {
        let geometry = ShadowGeometry::new();
        assert_eq!(geometry.corner_radius, CORNER_RADIUS as u32);
    }
}
