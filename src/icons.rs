//! Material Design Icons (see `icon_data`) as
//! GTK symbolic images, so they take the colour of the widget they sit in.

use std::{fs, path::PathBuf};

use gtk::{gio, prelude::*};

use crate::icon_data::ICONS;

/// Where the SVGs are written so GTK can load them as files.
fn dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("discord-gtk")
        .join("icons")
}

/// Writes the icon files out. Call once at startup.
pub fn install() {
    let dir = dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    for (name, svg) in ICONS {
        let path = dir.join(format!("{name}.svg"));
        // Rewriting every start keeps icons in step with the binary.
        let _ = fs::write(path, svg);
    }
}

/// An image showing one of the icons, e.g. `"hashmark"` or `"microphone-slash"`.
pub fn image(name: &str, size: i32) -> gtk::Image {
    let image = gtk::Image::new();
    image.set_pixel_size(size);
    set(&image, name, size);
    image
}

pub fn set(image: &gtk::Image, name: &str, size: i32) {
    let file = gio::File::for_path(dir().join(format!("icon-{name}-symbolic.svg")));
    image.set_paintable(Some(&gtk::IconPaintable::for_file(&file, size, 1)));
}

/// A flat button holding an icon.
pub fn button(name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_child(Some(&image(name, 20)));
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button
}
