//! A one-pixel td box shadow must stay clipped to the table cell's edge.
//!
//! In particular, putting the table in an opacity effect layer must not make
//! the shadow spread beyond its one-pixel hairline.

use anyrender::render_to_buffer;
use anyrender_vello_cpu::VelloCpuImageRenderer;
use blitz_dom::DocumentConfig;
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_paint::paint_scene;
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

const WIDTH: u32 = 100;
const HEIGHT: u32 = 100;
const TD_LEFT: u32 = 20;
const TD_TOP: u32 = 20;
const TD_SIZE: u32 = 40;

fn render(html: &str) -> Vec<u8> {
    let mut doc = HtmlDocument::from_html(
        html,
        DocumentConfig {
            viewport: Some(Viewport::new(WIDTH, HEIGHT, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider)),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    render_to_buffer::<VelloCpuImageRenderer, _>(
        |scene| paint_scene(scene, &mut doc, 1.0, WIDTH, HEIGHT, 0, 0),
        WIDTH,
        HEIGHT,
    )
}

/// `(r, g, b)` at a pixel in the rendered RGBA buffer.
fn pixel(buffer: &[u8], x: u32, y: u32) -> (u8, u8, u8) {
    let i = ((y * WIDTH + x) * 4) as usize;
    (buffer[i], buffer[i + 1], buffer[i + 2])
}

fn html(opacity: Option<&str>) -> String {
    let opacity = opacity.map_or(String::new(), |value| format!("opacity:{value};"));
    format!(
        r#"
        <!doctype html>
        <html><body style="margin:0;background:#ffffff">
          <style>
            table { border-collapse: collapse; }
            td { width:40px; height:40px; padding:0;
                 box-shadow:0 0 0 1px rgb(0,0,255); }
          </style>
          <table style="position:absolute;left:{TD_LEFT}px;top:{TD_TOP}px;
                        background:#ffffff;{opacity}">
            <tbody><tr><td></td></tr></tbody>
          </table>
        </body></html>
        "#,
    )
}

fn assert_hairline_is_local(buffer: &[u8], label: &str) {
    let x = TD_LEFT + TD_SIZE / 2;
    let outside = pixel(buffer, x, TD_TOP - 1);
    let farther = pixel(buffer, x, TD_TOP - 2);

    assert!(
        outside.2 > outside.0 + 40,
        "{label}: the pixel immediately outside the td should contain the blue hairline, got {outside:?}"
    );
    assert_eq!(
        farther,
        (255, 255, 255),
        "{label}: the shadow must not spread to the second outside pixel, got {farther:?}"
    );
}

#[test]
fn td_box_shadow_is_one_pixel_outside_the_cell() {
    let buffer = render(&html(None));
    assert_hairline_is_local(&buffer, "plain table");
}

#[test]
fn td_box_shadow_stays_local_inside_an_opacity_effect_layer() {
    let buffer = render(&html(Some("0.5")));
    assert_hairline_is_local(&buffer, "opacity table");
}
