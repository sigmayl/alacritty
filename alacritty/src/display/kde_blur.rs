//! Rounded KWin blur regions for Wayland client-side decorations.

use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_region, wl_registry, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1, ext_background_effect_surface_v1,
};
use wayland_protocols_plasma::blur::client::{org_kde_kwin_blur, org_kde_kwin_blur_manager};

use super::decorations::CORNER_RADIUS;

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

delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: wl_region::WlRegion);
delegate_noop!(State: org_kde_kwin_blur_manager::OrgKdeKwinBlurManager);
delegate_noop!(State: org_kde_kwin_blur::OrgKdeKwinBlur);
impl Dispatch<ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1,
        _: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(State: ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1);

/// Blur state attached to a foreign winit Wayland surface.
pub struct KdeBlur {
    connection: Connection,
    event_queue: EventQueue<State>,
    compositor: wl_compositor::WlCompositor,
    ext_manager: Option<ext_background_effect_manager_v1::ExtBackgroundEffectManagerV1>,
    legacy_manager: Option<org_kde_kwin_blur_manager::OrgKdeKwinBlurManager>,
    surface: wl_surface::WlSurface,
    ext_blur: Option<ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1>,
    legacy_blur: Option<org_kde_kwin_blur::OrgKdeKwinBlur>,
    region: Option<wl_region::WlRegion>,
    geometry: Option<(u32, u32, u32)>,
}

impl KdeBlur {
    /// Connect to KWin's blur protocol for an existing surface.
    ///
    /// # Safety
    ///
    /// The display and surface pointers must remain valid for the lifetime of this value.
    pub unsafe fn new(
        display: *mut std::ffi::c_void,
        surface: *mut std::ffi::c_void,
    ) -> Option<Self> {
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);
        let surface_id = unsafe {
            ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.cast()).ok()?
        };
        let surface = wl_surface::WlSurface::from_id(&connection, surface_id).ok()?;
        let (globals, event_queue) = registry_queue_init::<State>(&connection).ok()?;
        let qh = event_queue.handle();
        let compositor = globals.bind(&qh, 1..=1, ()).ok()?;
        let ext_manager = globals.bind(&qh, 1..=1, ()).ok();
        let legacy_manager = globals.bind(&qh, 1..=1, ()).ok();
        if ext_manager.is_none() && legacy_manager.is_none() {
            log::debug!("Wayland background blur protocols are unavailable");
            return None;
        }

        Some(Self {
            connection,
            event_queue,
            compositor,
            ext_manager,
            legacy_manager,
            surface,
            ext_blur: None,
            legacy_blur: None,
            region: None,
            geometry: None,
        })
    }

    pub fn update(&mut self, enabled: bool, width: u32, height: u32, scale: f64, rounded: bool) {
        let logical_width = (f64::from(width) / scale).round().max(1.) as u32;
        let logical_height = (f64::from(height) / scale).round().max(1.) as u32;
        let radius = if rounded { CORNER_RADIUS.round() as u32 } else { 0 };
        let geometry = (logical_width, logical_height, radius);

        if !enabled {
            self.disable();
            return;
        }

        if (self.ext_blur.is_some() || self.legacy_blur.is_some())
            && self.geometry == Some(geometry)
        {
            let _ = self.event_queue.dispatch_pending(&mut State);
            return;
        }

        let qh = self.event_queue.handle();
        let region = self.compositor.create_region(&qh, ());
        add_rounded_region(&region, logical_width, logical_height, radius);

        if let Some(manager) = &self.ext_manager {
            let blur = self
                .ext_blur
                .take()
                .unwrap_or_else(|| manager.get_background_effect(&self.surface, &qh, ()));
            blur.set_blur_region(Some(&region));
            self.ext_blur = Some(blur);
        } else if let Some(manager) = &self.legacy_manager {
            let blur = self
                .legacy_blur
                .take()
                .unwrap_or_else(|| manager.create(&self.surface, &qh, ()));
            blur.set_region(Some(&region));
            blur.commit();
            self.legacy_blur = Some(blur);
        }
        self.surface.commit();

        if let Some(old_region) = self.region.replace(region) {
            old_region.destroy();
        }
        self.geometry = Some(geometry);
        let _ = self.connection.flush();
    }

    fn disable(&mut self) {
        if let Some(blur) = self.ext_blur.take() {
            blur.set_blur_region(None);
            blur.destroy();
            self.surface.commit();
        }
        if let Some(blur) = self.legacy_blur.take() {
            self.legacy_manager.as_ref().unwrap().unset(&self.surface);
            blur.release();
            self.surface.commit();
        }
        if let Some(region) = self.region.take() {
            region.destroy();
        }
        self.geometry = None;
        let _ = self.connection.flush();
    }
}

impl Drop for KdeBlur {
    fn drop(&mut self) {
        self.disable();
        if let Some(manager) = self.ext_manager.take() {
            manager.destroy();
        }
    }
}

fn add_rounded_region(region: &wl_region::WlRegion, width: u32, height: u32, radius: u32) {
    let radius = radius.min(width / 2).min(height / 2);
    if radius == 0 {
        region.add(0, 0, width as i32, height as i32);
        return;
    }

    if height > 2 * radius {
        region.add(0, radius as i32, width as i32, (height - 2 * radius) as i32);
    }

    let radius_squared = f64::from(radius * radius);
    for row in 0..radius {
        let center_offset = f64::from(radius) - f64::from(row) - 0.5;
        let inside = (radius_squared - center_offset * center_offset).sqrt();
        let inset = (f64::from(radius) - inside).ceil() as u32;
        let row_width = (width - 2 * inset) as i32;
        region.add(inset as i32, row as i32, row_width, 1);
        region.add(inset as i32, (height - row - 1) as i32, row_width, 1);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn logical_geometry_respects_fractional_scale() {
        let width = (500_f64 / 1.25).round() as u32;
        let height = (375_f64 / 1.25).round() as u32;
        assert_eq!((width, height), (400, 300));
    }
}
