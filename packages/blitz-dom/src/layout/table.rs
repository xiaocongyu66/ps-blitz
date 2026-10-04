use blitz_traits::node_id::NodeId;
use std::{ops::Range, sync::Arc};

use atomic_refcell::AtomicRefCell;
use markup5ever::local_name;
use style::properties::style_structs::Border;
use style::servo_arc::Arc as ServoArc;
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use style::{
    Atom, computed_values::border_collapse::T as BorderCollapse,
    computed_values::table_layout::T as TableLayout,
};
use taffy::{
    DetailedGridInfo, LayoutPartialTree as _, ResolveOrZero, TrackSizingFunction, style_helpers,
};

use crate::BaseDocument;

use super::damage::{CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC};
use super::resolve_calc_value;

pub struct TableTreeWrapper<'doc> {
    pub(crate) doc: &'doc mut BaseDocument,
    pub(crate) ctx: Arc<TableContext>,
}

#[derive(Debug, Clone)]
pub struct TableContext {
    pub style: taffy::Style<Atom>,
    pub cells: Vec<TableCell>,
    pub rows: Vec<TableRow>,
    pub row_groups: Vec<TableRowGroup>,
    pub computed_grid_info: AtomicRefCell<Option<DetailedGridInfo>>,
    pub border_style: Option<ServoArc<Border>>,
    pub border_collapse: BorderCollapse,
}

// #[derive(Debug, Clone, Eq, PartialEq)]
// pub enum TableItemKind {
//     Row,
//     Cell,
// }

#[derive(Debug, Clone)]
pub struct TableCell {
    // kind: TableItemKind,
    node_id: NodeId,
    style: taffy::Style<Atom>,
}

#[derive(Debug, Clone)]
pub struct TableRow {
    // kind: TableItemKind,
    pub node_id: NodeId,
    pub height: f32,
    /// Where this row's cells sit in [`TableContext::cells`].
    ///
    /// Rows are flattened away into the grid, so nothing else records which
    /// cells belong to which row, and without that a row cannot be given the
    /// box its cells occupy. It reported 0x0 and "not displayed", and anything
    /// walking a table by row had nothing to walk.
    pub cells: Range<usize>,
}

/// A `<thead>`, `<tbody>` or `<tfoot>`, and the rows it holds.
#[derive(Debug, Clone)]
pub struct TableRowGroup {
    pub node_id: NodeId,
    /// Where this group's rows sit in [`TableContext::rows`].
    pub rows: Range<usize>,
}

pub(crate) fn build_table_context(
    doc: &mut BaseDocument,
    table_root_node_id: NodeId,
) -> (TableContext, Vec<NodeId>) {
    let mut cells: Vec<TableCell> = Vec::new();
    let mut rows: Vec<TableRow> = Vec::new();
    let mut row_groups: Vec<TableRowGroup> = Vec::new();
    let mut row = 0u16;
    let mut col = 0u16;

    let root_node = &mut doc.nodes[table_root_node_id];

    let children = std::mem::take(&mut root_node.children);

    let Some(stylo_styles) = root_node.primary_styles() else {
        panic!("Ignoring table because it has no styles");
    };

    let mut style = stylo_taffy::to_taffy_style(&stylo_styles);
    style.item_is_table = true;
    // Use `dense` row-flow so that each cell scans the row from its
    // leftmost column for the first free track. Without `dense`,
    // `place_definite_secondary_axis_item` keeps a per-item secondary
    // cursor across rows, which means cells in later rows do not
    // backfill columns freed up by rowspan cells from earlier rows.
    style.grid_auto_flow = taffy::GridAutoFlow::RowDense;
    style.grid_auto_columns = Vec::new();
    style.grid_auto_rows = Vec::new();

    let is_fixed = match stylo_styles.clone_table_layout() {
        TableLayout::Fixed => true,
        TableLayout::Auto => false,
    };

    let border_collapse = stylo_styles.clone_border_collapse();
    let border_spacing = stylo_styles.clone_border_spacing().0;

    drop(stylo_styles);

    let mut column_sizes: Vec<taffy::TrackSizingFunction> = Vec::new();
    let mut first_cell_border: Option<ServoArc<Border>> = None;
    for child_id in children.iter().copied() {
        collect_table_cells(
            doc,
            child_id,
            is_fixed,
            border_collapse,
            &mut row,
            &mut col,
            &mut cells,
            &mut rows,
            &mut row_groups,
            &mut column_sizes,
            &mut first_cell_border,
        );
    }
    column_sizes.resize(col as usize, style_helpers::auto());

    style.grid_template_columns = column_sizes.into_iter().map(|dim| dim.into()).collect();
    style.grid_template_rows = rows
        .iter()
        .map(|table_row| {
            if table_row.height > 0.0 {
                style_helpers::length(table_row.height)
            } else {
                style_helpers::auto()
            }
        })
        .collect();

    style.gap = match border_collapse {
        BorderCollapse::Separate => {
            // In the separated borders model, `border-spacing` also applies between
            // the table border and the outermost cells, in addition to between cells.
            let spacing_x = border_spacing.width.px();
            let spacing_y = border_spacing.height.px();
            let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
            style.padding = taffy::Rect {
                left: style_helpers::length(padding.left + spacing_x),
                right: style_helpers::length(padding.right + spacing_x),
                top: style_helpers::length(padding.top + spacing_y),
                bottom: style_helpers::length(padding.bottom + spacing_y),
            };
            taffy::Size {
                width: style_helpers::length(spacing_x),
                height: style_helpers::length(spacing_y),
            }
        }
        BorderCollapse::Collapse => first_cell_border
            .as_ref()
            .map(|border| {
                let x = border
                    .border_left_width
                    .0
                    .max(border.border_right_width.0)
                    .to_f32_px();
                let y = border
                    .border_top_width
                    .0
                    .max(border.border_bottom_width.0)
                    .to_f32_px();
                taffy::Size {
                    width: style_helpers::length(x),
                    height: style_helpers::length(y),
                }
            })
            .unwrap_or(taffy::Size::ZERO.map(style_helpers::length)),
    };

    if border_collapse == BorderCollapse::Collapse {
        style.border = taffy::Rect {
            left: style.gap.width,
            right: style.gap.width,
            top: style.gap.height,
            bottom: style.gap.height,
        };
    }

    let layout_children = cells.iter().map(|cell| cell.node_id).collect();
    let root_node = &mut doc.nodes[table_root_node_id];
    root_node.children = children;

    (
        TableContext {
            style,
            cells,
            rows,
            row_groups,
            computed_grid_info: AtomicRefCell::new(None),
            border_collapse,
            border_style: first_cell_border,
        },
        layout_children,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_table_cells(
    doc: &mut BaseDocument,
    node_id: NodeId,
    is_fixed: bool,
    border_collapse: BorderCollapse,
    row: &mut u16,
    col: &mut u16,
    cells: &mut Vec<TableCell>,
    rows: &mut Vec<TableRow>,
    row_groups: &mut Vec<TableRowGroup>,
    columns: &mut Vec<TrackSizingFunction>,
    first_cell_border: &mut Option<ServoArc<Border>>,
) {
    let node = &mut doc.nodes[node_id];

    if !node.is_element() {
        return;
    }

    let Some(display) = node.primary_styles().map(|s| s.clone_display()) else {
        #[cfg(feature = "tracing")]
        tracing::info!("Ignoring table descendent because it has no styles");
        return;
    };

    if display.outside() == DisplayOutside::None {
        node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
        return;
    }

    match display.inside() {
        DisplayInside::TableRowGroup
        | DisplayInside::TableHeaderGroup
        | DisplayInside::TableFooterGroup
        | DisplayInside::Contents => {
            let is_row_group = !matches!(display.inside(), DisplayInside::Contents);
            let first_row = rows.len();
            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                doc.nodes[child_id]
                    .remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                collect_table_cells(
                    doc,
                    child_id,
                    is_fixed,
                    border_collapse,
                    row,
                    col,
                    cells,
                    rows,
                    row_groups,
                    columns,
                    first_cell_border,
                );
            }
            doc.nodes[node_id].children = children;
            if is_row_group {
                row_groups.push(TableRowGroup {
                    node_id,
                    rows: first_row..rows.len(),
                });
            }
        }
        DisplayInside::TableRow => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            *row += 1;
            *col = 0;

            let row_index = rows.len();
            let first_cell = cells.len();
            let row_style = node
                .primary_styles()
                .map(stylo_taffy::to_taffy_style)
                .unwrap_or_default();
            let row_height = match row_style.size.height {
                taffy::Dimension::Length(value) => value,
                _ => 0.0,
            };
            rows.push(TableRow {
                node_id,
                height: row_height,
                cells: first_cell..first_cell,
            });

            let children = std::mem::take(&mut doc.nodes[node_id].children);
            for child_id in children.iter().copied() {
                collect_table_cells(
                    doc,
                    child_id,
                    is_fixed,
                    border_collapse,
                    row,
                    col,
                    cells,
                    rows,
                    row_groups,
                    columns,
                    first_cell_border,
                );
            }
            doc.nodes[node_id].children = children;
            rows[row_index].cells = first_cell..cells.len();
        }
        DisplayInside::TableCell => {
            // node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            let stylo_style = &node.primary_styles().unwrap();
            let colspan: u16 = node
                .attr(local_name!("colspan"))
                .and_then(|val| val.parse().ok())
                .unwrap_or(1);
            let rowspan: u16 = node
                .attr(local_name!("rowspan"))
                .and_then(|val| val.parse::<u16>().ok())
                .map(|v| v.clamp(1, 65534))
                .unwrap_or(1);
            let mut style = stylo_taffy::to_taffy_style(stylo_style);

            if first_cell_border.is_none() {
                // Only record a border when some side actually has a style:
                // stylo's computed width stays `medium` even for `none`, and
                // feeding that into the collapse gap wedges a phantom gutter
                // between cells that Chrome keeps flush.
                let b = stylo_style.clone_border();
                let has_border = !(b.border_left_style.none_or_hidden()
                    && b.border_right_style.none_or_hidden()
                    && b.border_top_style.none_or_hidden()
                    && b.border_bottom_style.none_or_hidden());
                if has_border {
                    *first_cell_border = Some(b);
                }
            }

            if *row == 1 {
                let column = match style.size.width.tag() {
                    taffy::CompactLength::LENGTH_TAG => {
                        let len = style.size.width.value();
                        let padding = style.padding.resolve_or_zero(None, resolve_calc_value);
                        let border = style.border.resolve_or_zero(None, resolve_calc_value);
                        match style.box_sizing {
                            taffy::BoxSizing::ContentBox => style_helpers::length(
                                len + padding.left + padding.right + border.left + border.right,
                            ),
                            taffy::BoxSizing::BorderBox => style_helpers::length(len),
                        }
                    }
                    taffy::CompactLength::PERCENT_TAG => {
                        if is_fixed {
                            style_helpers::percent(style.size.width.value())
                        } else {
                            style_helpers::auto()
                        }
                    }
                    taffy::CompactLength::AUTO_TAG => style_helpers::auto(),
                    // Dimension values are always length, percentage, auto or calc(),
                    // so any other tag is a calc() value. Pass it through so that
                    // Taffy resolves it against the table's inner width.
                    _ => style.size.width.into(),
                };
                // A spanning header contributes columns, but its own width
                // must not become the width of every covered date track.
                if colspan == 1 {
                    columns.push(column);
                } else {
                    let auto_track: TrackSizingFunction = style_helpers::auto::<TrackSizingFunction>();
                    columns.extend(std::iter::repeat_n(auto_track, colspan as usize));
                }
            }

            // Zero-out cell borders is BorderCollapse is Collapse
            // Borders are handled at the table level in this mode
            if border_collapse == BorderCollapse::Collapse {
                style.border = taffy::Rect::ZERO.map(style_helpers::length);
            }

            // The margin properties do not apply to table-internal elements
            style.margin = taffy::Rect::ZERO.map(style_helpers::length);

            // Let Taffy auto-place the column. Combined with
            // `grid_auto_flow: RowDense` set on the table root, each cell
            // scans from the first track in its row for a free position,
            // which makes cells automatically skip columns occupied by
            // rowspan cells from earlier rows.
            style.grid_column = taffy::Line {
                start: style_helpers::auto(),
                end: style_helpers::span(colspan),
            };
            style.grid_row = taffy::Line {
                start: style_helpers::line(*row as i16),
                end: style_helpers::span(rowspan),
            };
            style.size.width = style_helpers::auto();
            cells.push(TableCell { node_id, style });

            *col += colspan;
        }
        DisplayInside::Flow
        | DisplayInside::FlowRoot
        | DisplayInside::Flex
        | DisplayInside::Grid => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Probably a table caption: ignore
            // println!(
            //     "Warning: ignoring non-table typed descendent of table ({:?})",
            //     display.inside()
            // );
        }
        DisplayInside::TableColumnGroup | DisplayInside::TableColumn | DisplayInside::Table => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            //Ignore
        }
        DisplayInside::None => {
            node.remove_damage(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
            // Ignore
        }
    }
}

/// Give every `<tr>` and every row group the box its cells occupy.
///
/// Table layout flattens the rows into a grid of cells, so the row and
/// row-group nodes never reach Taffy and nothing ever wrote a layout for them:
/// each one reported 0x0 and, to anything asking whether an element is
/// displayed, "no". A row is not laid out, it is described, so this runs after
/// the grid is computed and derives each box from the cells that are.
///
/// Rounded and unrounded are both written. The rounding pass walks the box
/// tree, and these nodes are not in it, so a value left only in the unrounded
/// slot would never reach `final_layout`, which is what every geometry query
/// reads.
impl BaseDocument {
    pub(crate) fn assign_table_row_layouts(&mut self) {
        let contexts: Vec<Arc<TableContext>> = self
            .nodes
            .iter()
            .filter(|(_, node)| node.flags.is_table_root())
            .filter_map(
                |(_, node)| match &node.data.downcast_element()?.special_data {
                    crate::node::SpecialElementData::TableRoot(context) => {
                        Some(Arc::clone(context))
                    }
                    _ => None,
                },
            )
            .collect();
        for context in contexts {
            assign_row_layouts(self, &context);
        }
    }
}

pub(crate) fn assign_row_layouts(doc: &mut BaseDocument, ctx: &TableContext) {
    if ctx.rows.is_empty() {
        return;
    }

    // The grid's own extent, so that a row spans the table rather than only the
    // cells that happen to be in it. A row with a colspan short of the full
    // width is still as wide as the table.
    let mut left = f32::MAX;
    let mut right = f32::MIN;
    for cell in &ctx.cells {
        let Some(node) = doc.nodes.get(cell.node_id) else {
            continue;
        };
        let layout = node.final_layout();
        left = left.min(layout.location.x);
        right = right.max(layout.location.x + layout.size.width);
    }
    if left > right {
        return;
    }

    let mut row_extents: Vec<Option<(f32, f32)>> = Vec::with_capacity(ctx.rows.len());
    for row in &ctx.rows {
        let mut top = f32::MAX;
        let mut bottom = f32::MIN;
        for cell in &ctx.cells[row.cells.clone()] {
            let Some(node) = doc.nodes.get(cell.node_id) else {
                continue;
            };
            let layout = node.final_layout();
            top = top.min(layout.location.y);
            bottom = bottom.max(layout.location.y + layout.size.height);
        }
        if top > bottom {
            row_extents.push(None);
            continue;
        }
        row_extents.push(Some((top, bottom)));
        write_box(doc, row.node_id, left, top, right - left, bottom - top);
    }

    for group in &ctx.row_groups {
        let mut top = f32::MAX;
        let mut bottom = f32::MIN;
        for extent in row_extents[group.rows.clone()].iter().flatten() {
            top = top.min(extent.0);
            bottom = bottom.max(extent.1);
        }
        if top > bottom {
            continue;
        }
        write_box(doc, group.node_id, left, top, right - left, bottom - top);
    }
}

fn write_box(doc: &mut BaseDocument, node_id: NodeId, x: f32, y: f32, width: f32, height: f32) {
    let Some(node) = doc.nodes.get_mut(node_id) else {
        return;
    };
    let mut layout = taffy::Layout::with_order(node.final_layout().order);
    layout.location = taffy::Point { x, y };
    layout.size = taffy::Size { width, height };
    layout.content_size = layout.size;
    *node.unrounded_layout_mut() = layout;
    *node.final_layout_mut() = layout;
}

pub struct RangeIter(Range<usize>);

impl Iterator for RangeIter {
    type Item = taffy::NodeId;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(taffy::NodeId::from)
    }
}

impl taffy::TraversePartialTree for TableTreeWrapper<'_> {
    type ChildIter<'a>
        = RangeIter
    where
        Self: 'a;

    #[inline(always)]
    fn child_ids(&self, _node_id: taffy::NodeId) -> Self::ChildIter<'_> {
        RangeIter(0..self.ctx.cells.len())
    }

    #[inline(always)]
    fn child_count(&self, _node_id: taffy::NodeId) -> usize {
        self.ctx.cells.len()
    }

    #[inline(always)]
    fn get_child_id(&self, _node_id: taffy::NodeId, index: usize) -> taffy::NodeId {
        index.into()
    }
}
impl taffy::TraverseTree for TableTreeWrapper<'_> {}

impl taffy::LayoutPartialTree for TableTreeWrapper<'_> {
    type CoreContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type CustomIdent = Atom;

    fn get_core_container_style(&self, _node_id: taffy::NodeId) -> &taffy::Style<Atom> {
        &self.ctx.style
    }

    fn resolve_calc_value(&self, calc_ptr: *const (), parent_size: f32) -> f32 {
        resolve_calc_value(calc_ptr, parent_size)
    }

    fn set_unrounded_layout(&mut self, node_id: taffy::NodeId, layout: &taffy::Layout) {
        let node_id = crate::taffy_node_id(self.ctx.cells[usize::from(node_id)].node_id);
        self.doc.set_unrounded_layout(node_id, layout)
    }

    fn compute_child_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: taffy::tree::LayoutInput,
    ) -> taffy::LayoutOutput {
        let cell = &self.ctx.cells[usize::from(node_id)];
        let node_id = crate::taffy_node_id(cell.node_id);
        self.doc.compute_child_layout(node_id, inputs)
    }
}

impl taffy::LayoutGridContainer for TableTreeWrapper<'_> {
    type GridContainerStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    type GridItemStyle<'a>
        = &'a taffy::Style<Atom>
    where
        Self: 'a;

    fn get_grid_container_style(&self, node_id: taffy::NodeId) -> Self::GridContainerStyle<'_> {
        self.get_core_container_style(node_id)
    }

    fn get_grid_child_style(&self, child_node_id: taffy::NodeId) -> Self::GridItemStyle<'_> {
        &self.ctx.cells[usize::from(child_node_id)].style
    }

    fn set_detailed_grid_info(
        &mut self,
        _node_id: taffy::NodeId,
        detailed_grid_info: DetailedGridInfo,
    ) {
        *self.ctx.computed_grid_info.borrow_mut() = Some(detailed_grid_info);
    }
}
