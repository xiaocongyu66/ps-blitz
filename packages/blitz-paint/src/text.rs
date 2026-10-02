use anyrender::{Filter, PaintScene};
use blitz_dom::{BaseDocument, NodeId, node::TextBrush, util::ToColorColor};
use kurbo::{Affine, Rect, RoundedRect, Stroke, Vec2};
use parley::{Affinity, Cursor, Layout, Line, PositionedLayoutItem, Selection};
use peniko::{Fill, Mix};
use std::sync::Arc;
use style::values::computed::{CSSPixelLength, TextDecorationLine};

use crate::color::{Color, ToColorColor as _};
use crate::{FONT_EMBOLDEN_ENABLED, SELECTION_COLOR};

/// Draw the backgrounds of inline elements (e.g. `<span style="background: ...">`).
///
/// Each glyph run carries the node id of the innermost inline element it belongs to
/// (via its brush). We look up that node's `background-color` and, if non-transparent,
/// fill a rectangle covering the run's advance and its font's ascent/descent so that the
/// background sits behind the text.
///
/// The inline root's own background is painted separately (as a normal block box), so
/// runs belonging to the root are skipped to avoid drawing it twice.
pub(crate) fn draw_inline_backgrounds<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    inline_root_id: NodeId,
) {
    for line in lines {
        for item in line.items() {
            let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                continue;
            };

            let node_id = glyph_run.style().brush.id;
            if node_id == inline_root_id {
                continue;
            }

            let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles()) else {
                continue;
            };

            let current_color = styles.clone_color();
            let bg_color = styles
                .get_background()
                .background_color
                .resolve_to_absolute(&current_color)
                .as_srgb_color();
            if bg_color == Color::TRANSPARENT {
                continue;
            }

            let metrics = glyph_run.run().metrics();
            let x = glyph_run.offset() as f64;
            let w = glyph_run.advance() as f64;
            let baseline = glyph_run.baseline() as f64;
            let y0 = baseline - metrics.ascent as f64;
            let y1 = baseline + metrics.descent as f64;
            let rect = Rect::new(x, y0, x + w, y1);

            // `border-radius` applies to an inline box's background too. Filling
            // a plain rect made the application's inline code chips — styled
            // `rounded-[5px] border bg-base-300` — render as hard-edged squares
            // sitting behind their text, which is what they were reported as.
            //
            // Resolved the same way `create_css_rect` does it for a block box:
            // percentages against the box being painted, each corner clamped so
            // opposite radii cannot overlap.
            let s_border = styles.get_border();
            let resolve_w = CSSPixelLength::new(rect.width() as f32);
            let resolve_h = CSSPixelLength::new(rect.height() as f32);
            let resolve = |radius: &style::values::computed::BorderCornerRadius| -> (f64, f64) {
                (
                    radius.0.width.0.resolve(resolve_w).px() as f64,
                    radius.0.height.0.resolve(resolve_h).px() as f64,
                )
            };
            let corners = [
                resolve(&s_border.border_top_left_radius),
                resolve(&s_border.border_top_right_radius),
                resolve(&s_border.border_bottom_right_radius),
                resolve(&s_border.border_bottom_left_radius),
            ];

            // Kurbo's uniform radii are enough here: a chip's corners are the
            // same in practice, and taking the smallest keeps an over-large
            // radius from swallowing a short run.
            let limit = (rect.width().min(rect.height()) / 2.0).max(0.0);
            let radius = corners
                .iter()
                .flat_map(|(x, y)| [*x, *y])
                .fold(f64::INFINITY, f64::min)
                .min(limit);

            if radius > 0.0 {
                scene.fill(
                    Fill::NonZero,
                    transform,
                    bg_color,
                    None,
                    &RoundedRect::from_rect(rect, radius),
                );
            } else {
                scene.fill(Fill::NonZero, transform, bg_color, None, &rect);
            }
        }
    }
}

pub(crate) fn stroke_text<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    scale: f64,
) {
    stroke_text_with_alpha(scene, lines, doc, transform, scale, 1.0);
}

pub(crate) fn stroke_text_with_alpha<'a>(
    scene: &mut impl PaintScene,
    lines: impl Iterator<Item = Line<'a, TextBrush>>,
    doc: &BaseDocument,
    transform: Affine,
    scale: f64,
    alpha: f32,
) {
    for line in lines {
        for item in line.items() {
            if let PositionedLayoutItem::GlyphRun(glyph_run) = item {
                let run = glyph_run.run();
                let font = run.font();
                let font_size = run.font_size();
                let metrics = run.metrics();
                let style = glyph_run.style();
                let synthesis = run.synthesis();
                let glyph_xform = synthesis
                    .skew()
                    .map(|angle| Affine::skew(angle.to_radians().tan() as f64, 0.0));

                // Styles
                let styles = doc
                    .get_node(style.brush.id)
                    .unwrap()
                    .primary_styles()
                    .unwrap();
                let itext_styles = styles.get_inherited_text();
                let text_styles = styles.get_text();
                let text_color = itext_styles.color.as_color_color();
                let text_decoration_color = text_styles
                    .text_decoration_color
                    .as_absolute()
                    .map(ToColorColor::as_color_color)
                    .unwrap_or(text_color);
                let text_decoration_brush = anyrender::Paint::from(text_decoration_color);
                let text_decoration_line = text_styles.text_decoration_line;
                let has_underline = text_decoration_line.contains(TextDecorationLine::UNDERLINE);
                let has_strikethrough =
                    text_decoration_line.contains(TextDecorationLine::LINE_THROUGH);

                let embolden = if FONT_EMBOLDEN_ENABLED {
                    let fs = font_size as f64 / scale;
                    kurbo::Vec2::new((0.015125 * fs).min(0.3), (0.0121 * fs).min(0.3))
                } else {
                    kurbo::Vec2::default()
                };

                // text-shadow: CSS paints the list in reverse order (front to
                // back), so the first entry ends up on top. Each shadow redraws
                // the glyphs offset by (x, y) in the shadow colour; a non-zero
                // blur radius runs through a gaussian layer with sigma = blur/2,
                // the same convention our box-shadow path applies to CSS blur.
                let shadow_list = &itext_styles.text_shadow.0;
                if !shadow_list.is_empty() {
                    let paint_glyphs =
                        |scene: &mut impl PaintScene, paint: anyrender::Paint, xform: Affine| {
                            scene.draw_glyphs(
                                font,
                                font_size,
                                !FONT_EMBOLDEN_ENABLED, // hint
                                run.normalized_coords(),
                                embolden,
                                Fill::NonZero,
                                &paint,
                                alpha,
                                xform,
                                glyph_xform,
                                glyph_run.positioned_glyphs().map(|glyph| anyrender::Glyph {
                                    id: glyph.id as _,
                                    x: glyph.x,
                                    y: glyph.y,
                                }),
                            );
                        };

                    for shadow in shadow_list.iter().rev() {
                        let sc = shadow
                            .color
                            .resolve_to_absolute(&text_color)
                            .as_srgb_color();
                        let shadow_paint = anyrender::Paint::from(sc);
                        let dx = shadow.horizontal.px() as f64;
                        let dy = shadow.vertical.px() as f64;
                        let sigma = shadow.blur.px() as f32 * 0.5;
                        let xform = transform.then_translate(Vec2 { x: dx, y: dy });

                        if sigma > 0.0 {
                            // Generous clip so blurred shadows are not cut off;
                            // vello_cpu allocates scratch buffers by layer size.
                            let clip = Rect::new(-4.0e4, -4.0e4, 8.0e4, 8.0e4);
                            scene.push_layer(
                                Mix::Normal,
                                1.0,
                                transform,
                                &clip,
                                Some(Arc::new(Filter::blur(sigma))),
                                None,
                            );
                            paint_glyphs(scene, shadow_paint, xform);
                            scene.pop_layer();
                        } else {
                            paint_glyphs(scene, shadow_paint, xform);
                        }
                    }
                }

                scene.draw_glyphs(
                    font,
                    font_size,
                    !FONT_EMBOLDEN_ENABLED, // hint
                    run.normalized_coords(),
                    embolden,
                    Fill::NonZero,
                    &anyrender::Paint::from(text_color),
                    alpha,
                    transform,
                    glyph_xform,
                    glyph_run.positioned_glyphs().map(|glyph| anyrender::Glyph {
                        id: glyph.id as _,
                        x: glyph.x,
                        y: glyph.y,
                    }),
                );

                let mut draw_decoration_line =
                    |offset: f32, size: f32, brush: &anyrender::Paint| {
                        let x = glyph_run.offset() as f64;
                        let w = glyph_run.advance() as f64;
                        let y = (glyph_run.baseline() - offset + size / 2.0) as f64;
                        let line = kurbo::Line::new((x, y), (x + w, y));
                        scene.stroke(&Stroke::new(size as f64), transform, brush, None, &line)
                    };

                if has_underline {
                    let offset = metrics.underline_offset;
                    let size = metrics.underline_size;

                    // TODO: intercept line when crossing an descending character like "gqy"
                    draw_decoration_line(offset, size, &text_decoration_brush);
                }
                if has_strikethrough {
                    let offset = metrics.strikethrough_offset;
                    let size = metrics.strikethrough_size;

                    draw_decoration_line(offset, size, &text_decoration_brush);
                }
            }
        }
    }
}

/// Draw selection highlight rectangles for the given byte range in a layout.
/// Uses Parley's Selection type for accurate geometry calculation.
pub(crate) fn draw_text_selection(
    scene: &mut impl PaintScene,
    layout: &Layout<TextBrush>,
    transform: Affine,
    selection_start: usize,
    selection_end: usize,
) {
    let anchor = Cursor::from_byte_index(layout, selection_start, Affinity::Downstream);
    let focus = Cursor::from_byte_index(layout, selection_end, Affinity::Downstream);
    let selection = Selection::new(anchor, focus);

    selection.geometry_with(layout, |rect, _line_idx| {
        let rect = kurbo::Rect::new(rect.x0, rect.y0, rect.x1, rect.y1);
        scene.fill(Fill::NonZero, transform, SELECTION_COLOR, None, &rect);
    });
}
