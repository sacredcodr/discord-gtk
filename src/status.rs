//! Discord's presence badge: a ring in the surface colour around a coloured
//! disc, with a crescent for idle, a bar for do-not-disturb and a hole for
//! offline.

use gtk::{cairo, prelude::*};

const SIZE: i32 = 16;

fn colour(status: &str) -> (f64, f64, f64) {
    let rgb = |hex: u32| {
        (f64::from(hex >> 16 & 0xff) / 255.0, f64::from(hex >> 8 & 0xff) / 255.0, f64::from(hex & 0xff) / 255.0)
    };
    match status {
        "online" => rgb(0x23a55a),
        "idle" => rgb(0xf0b232),
        "dnd" => rgb(0xf23f43),
        _ => rgb(0x80848e),
    }
}

pub fn normalize(status: &str) -> &'static str {
    match status {
        "online" => "online",
        "idle" => "idle",
        "dnd" => "dnd",
        _ => "offline",
    }
}

/// A status badge to overlay on the bottom-right of an avatar. The ring takes
/// the widget's CSS `color`, so it blends into whatever the avatar sits on.
pub fn dot(status: &str) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.add_css_class("status-dot");
    area.set_content_width(SIZE);
    area.set_content_height(SIZE);
    area.set_halign(gtk::Align::End);
    area.set_valign(gtk::Align::End);
    area.set_can_target(false);
    set(&area, status);
    area.set_draw_func(|area, cr, w, h| {
        let status = ["online", "idle", "dnd"]
            .into_iter()
            .find(|s| area.has_css_class(s))
            .unwrap_or("offline");
        let ring = area.color();
        let (cx, cy) = (f64::from(w) / 2.0, f64::from(h) / 2.0);
        let full = cx;
        let inner = cx - 3.0;
        let paint = |r: (f64, f64, f64)| cr.set_source_rgb(r.0, r.1, r.2);
        let circle = |x: f64, y: f64, r: f64| cr.arc(x, y, r, 0.0, std::f64::consts::TAU);
        let surface = (f64::from(ring.red()), f64::from(ring.green()), f64::from(ring.blue()));

        paint(surface);
        circle(cx, cy, full);
        let _ = cr.fill();
        paint(colour(status));
        circle(cx, cy, inner);
        let _ = cr.fill();
        paint(surface);
        match status {
            "idle" => {
                circle(cx - inner * 0.45, cy - inner * 0.45, inner * 0.6);
                let _ = cr.fill();
            }
            "dnd" => {
                let (bw, bh) = (inner * 1.2, inner * 0.55);
                rounded(cr, cx - bw / 2.0, cy - bh / 2.0, bw, bh, bh / 2.0);
                let _ = cr.fill();
            }
            "offline" => {
                circle(cx, cy, inner * 0.5);
                let _ = cr.fill();
            }
            _ => {}
        }
    });
    area
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, 3.0 * FRAC_PI_2);
    cr.close_path();
}

/// Switches a badge to another status.
pub fn set(dot: &gtk::DrawingArea, status: &str) {
    for class in ["online", "idle", "dnd", "offline"] {
        dot.remove_css_class(class);
    }
    dot.add_css_class(normalize(status));
    dot.queue_draw();
}
