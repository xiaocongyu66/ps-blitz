//! `<tr>`, `<thead>` and `<tbody>` must report a box.
//!
//! Table layout flattens the rows into a CSS grid of cells: the row and
//! row-group nodes have their box construction damage cleared and never reach
//! Taffy, so every one of them reported 0x0 and "not displayed". Anything that
//! walks a table by row, a QA check included, had nothing to walk.

use blitz_dom::DocumentConfig;
use blitz_html::{HtmlDocument, HtmlProvider};
use blitz_traits::shell::{ColorScheme, Viewport};
use std::sync::Arc;

const HTML: &str = r#"<!DOCTYPE html>
<html><head><style>
    body { margin: 0 }
    table { border-collapse: collapse; width: 400px }
    td, th { padding: 0; height: 20px; width: 200px }
</style></head>
<body>
    <table>
        <thead><tr id="head"><th>Name</th><th>Size</th></tr></thead>
        <tbody id="body">
            <tr id="first"><td>alpha</td><td>1</td></tr>
            <tr id="second"><td>beta</td><td>2</td></tr>
        </tbody>
    </table>
</body></html>
"#;

fn rect(doc: &HtmlDocument, selector: &str) -> (f32, f32, f32, f32) {
    let node_id = doc
        .query_selector(selector)
        .unwrap()
        .unwrap_or_else(|| panic!("no node matching {selector}"));
    let node = doc.get_node(node_id).unwrap();
    let position = node.absolute_position(0.0, 0.0);
    let layout = node.final_layout();
    (
        position.x,
        position.y,
        layout.size.width,
        layout.size.height,
    )
}

#[test]
fn every_row_reports_the_box_its_cells_occupy() {
    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let head = rect(&doc, "#head");
    let first = rect(&doc, "#first");
    let second = rect(&doc, "#second");

    for (name, row) in [("head", head), ("first", first), ("second", second)] {
        assert!(
            row.2 > 0.0 && row.3 > 0.0,
            "row #{name} has no box: {row:?}"
        );
    }

    assert_eq!(
        (head.0, head.2),
        (first.0, first.2),
        "every row spans the same horizontal extent, the table's"
    );
    assert_eq!((first.0, first.2), (second.0, second.2));
    assert_eq!(head.3, 20.0, "a row is as tall as its cells");

    assert!(
        head.1 < first.1 && first.1 < second.1,
        "rows stack in tree order: {head:?} {first:?} {second:?}"
    );
    assert!(
        head.1 + head.3 <= first.1 && first.1 + first.3 <= second.1,
        "rows do not overlap: {head:?} {first:?} {second:?}"
    );
}

#[test]
fn a_row_group_spans_the_rows_it_holds() {
    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let body = rect(&doc, "#body");
    let first = rect(&doc, "#first");
    let second = rect(&doc, "#second");

    assert_eq!(
        (body.0, body.2),
        (first.0, first.2),
        "a row group spans the table horizontally"
    );
    assert_eq!(body.1, first.1, "tbody starts at its first row");
    assert_eq!(
        body.1 + body.3,
        second.1 + second.3,
        "tbody ends at its last row"
    );
    assert!(body.3 > first.3, "tbody covers both of its rows");
}

#[test]
fn colspan_headers_preserve_thirteen_date_columns() {
    const HTML: &str = r#"<!DOCTYPE html><style>
        body { margin: 0 } table { border-collapse: collapse; width: 520px }
        td { width: 40px; height: 20px; padding: 0 }
    </style><table>
        <tr><td id="sep" colspan="4">9月</td><td id="oct" colspan="9">10月</td></tr>
        <tr><td id="d0">29</td><td id="d1">30</td><td id="d2">1</td><td id="d3">2</td>
            <td id="d4">3</td><td id="d5">4</td><td id="d6">5</td><td id="d7">6</td>
            <td id="d8">7</td><td id="d9">8</td><td id="d10">9</td><td id="d11">10</td><td id="d12">11</td></tr>
    </table>"#;
    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let sep = rect(&doc, "#sep");
    let oct = rect(&doc, "#oct");
    assert!(sep.2 > 0.0 && oct.2 > 0.0);
    assert!(oct.0 > sep.0 && sep.0 + sep.2 <= oct.0);

    let dates: Vec<_> = (0..13)
        .map(|index| rect(&doc, &format!("#d{index}")))
        .collect();
    for pair in dates.windows(2) {
        assert!(pair[1].0 > pair[0].0, "date columns overlap: {pair:?}");
        assert_eq!(pair[0].2, pair[1].2, "date columns differ in width");
    }
    assert!(
        dates[0].0 < oct.0,
        "September dates must precede October header"
    );
}

#[test]
fn definite_colspan_width_is_distributed_across_covered_columns() {
    const HTML: &str = r#"<!DOCTYPE html><style>
        body { margin: 0 } table { border-collapse: separate; border-spacing: 2px }
        td { height: 20px; padding: 0 } .day { width: 40px }
    </style><table><tr><td id="header" colspan="3" style="width: 240px">month</td></tr>
        <tr><td class="day" id="c0">1</td><td class="day" id="c1">2</td><td class="day" id="c2">3</td></tr>
    </table>"#;
    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let header = rect(&doc, "#header");
    let columns: Vec<_> = (0..3)
        .map(|index| rect(&doc, &format!("#c{index}")))
        .collect();
    assert!((header.2 - 240.0).abs() < 0.1, "header width: {header:?}");
    for pair in columns.windows(2) {
        assert!(
            (pair[0].2 - pair[1].2).abs() < 0.1,
            "tracks differ: {pair:?}"
        );
    }
    assert!(
        (columns[0].2 - 78.666_67).abs() < 0.1,
        "tracks: {columns:?}"
    );
}

#[test]
fn long_colspan_text_does_not_copy_into_each_date_column() {
    const HTML: &str = r#"<!DOCTYPE html><style>
        body { margin: 0 } table { border-collapse: collapse }
        td { height: 20px; padding: 0 } .day { width: 40px }
    </style><table><tr><td colspan="3">September extraordinarily long month heading</td></tr>
        <tr><td class="day" id="c0">1</td><td class="day" id="c1">2</td><td class="day" id="c2">3</td></tr>
    </table>"#;
    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let columns: Vec<_> = (0..3)
        .map(|index| rect(&doc, &format!("#c{index}")))
        .collect();
    for column in &columns {
        assert!(
            (column.2 - 40.0).abs() < 0.1,
            "long heading widened a date track: {columns:?}"
        );
    }
}

#[test]
fn relative_cell_is_the_containing_block_for_an_inset_overlay() {
    const HTML: &str = r#"<!DOCTYPE html><style>
        body { margin: 0 } table { border-collapse: collapse }
        td { position: relative; width: 120px; height: 30px; padding: 0 }
        .overlay { position: absolute; inset: 0; height: 100%; background: red }
    </style><table><tr><td id="cell"><span id="overlay" class="overlay"></span></td></tr></table>"#;
    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);

    let cell = rect(&doc, "#cell");
    let overlay = rect(&doc, "#overlay");
    assert_eq!(overlay.0, cell.0);
    assert_eq!(overlay.1, cell.1);
    assert_eq!(overlay.2, cell.2);
    assert_eq!(overlay.3, cell.3);
}

#[test]
fn rowspan_skips_the_occupied_slot() {
    const HTML: &str = r#"<!DOCTYPE html><style>
        body { margin: 0 } table { border-collapse: collapse }
        td { width: 40px; height: 20px; padding: 0 }
    </style><table><tr><td id="span" rowspan="2">a</td><td id="top">b</td></tr><tr><td id="bottom">c</td></tr></table>"#;
    let mut doc = HtmlDocument::from_html(
        HTML,
        DocumentConfig {
            viewport: Some(Viewport::new(800, 600, 1.0, ColorScheme::Light)),
            html_parser_provider: Some(Arc::new(HtmlProvider) as _),
            ..Default::default()
        },
    );
    doc.resolve(0.0);
    let span = rect(&doc, "#span");
    let bottom = rect(&doc, "#bottom");
    assert!(
        bottom.0 > span.0,
        "rowspan cell must reserve its column: {span:?} {bottom:?}"
    );
    assert!(
        bottom.1 > span.1,
        "second-row cell must be below the first row: {span:?} {bottom:?}"
    );
}
