use std::time::{Duration, Instant};

use unicode_width::UnicodeWidthChar;
use winit::dpi::PhysicalPosition;
use winit::window::{CursorIcon, ResizeDirection};

pub const TITLEBAR_HEIGHT: f32 = 30.;
pub const BUTTON_WIDTH: f32 = 42.;
pub const CORNER_RADIUS: f64 = 8.;
const RESIZE_EDGE: f64 = 6.;
const RESIZE_CORNER: f64 = 12.;
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Button {
    Minimize,
    Maximize,
    Close,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Hit {
    None,
    Titlebar,
    Button(Button),
    Resize(ResizeDirection),
}

impl Hit {
    pub fn cursor_icon(self) -> CursorIcon {
        match self {
            Self::Button(_) => CursorIcon::Pointer,
            Self::Resize(direction) => direction.into(),
            Self::None | Self::Titlebar => CursorIcon::Default,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Action {
    Minimize,
    ToggleMaximized,
    ExitFullscreen,
    Close,
    Drag,
    Resize(ResizeDirection),
}

#[derive(Debug, Copy, Clone)]
pub struct Layout {
    width: f64,
    height: f64,
    scale_factor: f64,
    resizable: bool,
}

impl Layout {
    pub fn new(width: f64, height: f64, scale_factor: f64, resizable: bool) -> Self {
        Self { width, height, scale_factor, resizable }
    }

    pub fn titlebar_height(self) -> f64 {
        f64::from(TITLEBAR_HEIGHT) * self.scale_factor
    }

    pub fn button_width(self) -> f64 {
        f64::from(BUTTON_WIDTH) * self.scale_factor
    }

    pub fn button_left(self, button: Button) -> f64 {
        let index = match button {
            Button::Minimize => 3.,
            Button::Maximize => 2.,
            Button::Close => 1.,
        };
        (self.width - index * self.button_width()).max(0.)
    }

    pub fn hit_test(self, position: PhysicalPosition<f64>) -> Hit {
        let x = position.x;
        let y = position.y;
        if x < 0. || y < 0. || x >= self.width || y >= self.height {
            return Hit::None;
        }

        if self.resizable {
            let edge = RESIZE_EDGE * self.scale_factor;
            let corner = RESIZE_CORNER * self.scale_factor;
            let left_corner = x < corner;
            let right_corner = x >= self.width - corner;
            let top_corner = y < corner;
            let bottom_corner = y >= self.height - corner;

            let direction = match (left_corner, right_corner, top_corner, bottom_corner) {
                (true, _, true, _) => Some(ResizeDirection::NorthWest),
                (_, true, true, _) => Some(ResizeDirection::NorthEast),
                (true, _, _, true) => Some(ResizeDirection::SouthWest),
                (_, true, _, true) => Some(ResizeDirection::SouthEast),
                _ if x < edge => Some(ResizeDirection::West),
                _ if x >= self.width - edge => Some(ResizeDirection::East),
                _ if y < edge => Some(ResizeDirection::North),
                _ if y >= self.height - edge => Some(ResizeDirection::South),
                _ => None,
            };

            if let Some(direction) = direction {
                return Hit::Resize(direction);
            }
        }

        if y >= self.titlebar_height() {
            return Hit::None;
        }

        for button in [Button::Close, Button::Maximize, Button::Minimize] {
            if x >= self.button_left(button) {
                return Hit::Button(button);
            }
        }

        Hit::Titlebar
    }
}

#[derive(Debug)]
pub struct Decorations {
    pub hovered: Hit,
    pub pressed: Option<Button>,
    last_titlebar_click: Option<Instant>,
}

impl Default for Decorations {
    fn default() -> Self {
        Self { hovered: Hit::None, pressed: None, last_titlebar_click: None }
    }
}

impl Decorations {
    pub fn clear_pointer_state(&mut self) -> bool {
        let changed = self.hovered != Hit::None || self.pressed.is_some();
        self.hovered = Hit::None;
        self.pressed = None;
        changed
    }

    pub fn update_hover(&mut self, hit: Hit) -> bool {
        if self.hovered == hit {
            return false;
        }

        self.hovered = hit;
        true
    }

    pub fn press(&mut self, hit: Hit, fullscreen: bool, now: Instant) -> Option<Action> {
        match hit {
            Hit::Button(button) => {
                self.pressed = Some(button);
                None
            },
            Hit::Resize(direction) => Some(Action::Resize(direction)),
            Hit::Titlebar => {
                let double_click = self
                    .last_titlebar_click
                    .is_some_and(|last| now.duration_since(last) < DOUBLE_CLICK_INTERVAL);
                self.last_titlebar_click = (!double_click).then_some(now);

                if double_click {
                    Some(if fullscreen { Action::ExitFullscreen } else { Action::ToggleMaximized })
                } else {
                    Some(Action::Drag)
                }
            },
            Hit::None => None,
        }
    }

    pub fn release(&mut self, hit: Hit, fullscreen: bool) -> Option<Action> {
        let pressed = self.pressed.take()?;
        if hit != Hit::Button(pressed) {
            return None;
        }

        Some(match pressed {
            Button::Minimize => Action::Minimize,
            Button::Maximize if fullscreen => Action::ExitFullscreen,
            Button::Maximize => Action::ToggleMaximized,
            Button::Close => Action::Close,
        })
    }
}

pub fn truncate_title(title: &str, max_columns: usize) -> String {
    let width = title.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
    if width <= max_columns {
        return title.to_owned();
    }
    if max_columns == 0 {
        return String::new();
    }

    let mut result = String::new();
    let mut used = 0;
    for character in title.chars() {
        let width = character.width().unwrap_or(0);
        if used + width >= max_columns {
            break;
        }
        result.push(character);
        used += width;
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_test_prioritizes_resize_edges() {
        let layout = Layout::new(500., 300., 1., true);
        assert_eq!(layout.hit_test((499., 1.).into()), Hit::Resize(ResizeDirection::NorthEast));
        assert_eq!(layout.hit_test((480., 15.).into()), Hit::Button(Button::Close));
        assert_eq!(layout.hit_test((200., 15.).into()), Hit::Titlebar);
        assert_eq!(layout.hit_test((200., 100.).into()), Hit::None);
    }

    #[test]
    fn maximized_layout_has_no_resize_edges() {
        let layout = Layout::new(500., 300., 1., false);
        assert_eq!(layout.hit_test((1., 1.).into()), Hit::Titlebar);
    }

    #[test]
    fn button_requires_release_over_same_button() {
        let mut decorations = Decorations::default();
        let close = Hit::Button(Button::Close);
        assert_eq!(decorations.press(close, false, Instant::now()), None);
        assert_eq!(decorations.release(Hit::Titlebar, false), None);
    }

    #[test]
    fn fullscreen_double_click_exits_fullscreen() {
        let mut decorations = Decorations::default();
        let now = Instant::now();
        assert_eq!(decorations.press(Hit::Titlebar, true, now), Some(Action::Drag));
        assert_eq!(
            decorations.press(Hit::Titlebar, true, now + Duration::from_millis(100)),
            Some(Action::ExitFullscreen)
        );
    }

    #[test]
    fn unicode_title_is_truncated_to_display_width() {
        assert_eq!(truncate_title("Alacritty 终端", 11), "Alacritty …");
        assert_eq!(truncate_title("abc", 3), "abc");
        assert_eq!(truncate_title("abc", 0), "");
    }
}
