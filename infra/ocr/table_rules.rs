//! Propose ruled tables from a bounded, row-major grayscale raster.

const MAX_PIXELS: usize = 6_000_000;
const MAX_RULES: usize = 128;
const MIN_RUN: usize = 25;
const RULE_MARGIN: usize = 5;
const INK_THRESHOLD: u8 = 155;
const FAINT_RULE_THRESHOLD: u8 = 170;

#[derive(Clone, Copy)]
struct Rule {
    at: usize,
    first: usize,
    last: usize,
    scan: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RuleTable {
    pub(crate) x: Vec<usize>,
    pub(crate) y: Vec<usize>,
    // [row_start, row_end, column_start, column_end]
    pub(crate) cells: Vec<[usize; 4]>,
    // [top_px, bottom_px, start_col, end_col]
    pub(crate) frames: Vec<[usize; 4]>,
}

#[derive(Default)]
pub(crate) struct RuleLayout {
    pub(crate) tables: Vec<RuleTable>,
    // [x_px, first_y, end_y] for strokes that meet a horizontal rail.
    pub(crate) vertical: Vec<[usize; 3]>,
    // [y_px, first_x, end_x] for observed horizontal rails.
    pub(crate) horizontal: Vec<[usize; 3]>,
    pub(crate) faint_closures: Vec<[usize; 3]>,
}

struct Raster<'a> {
    pixels: &'a [u8],
    width: usize,
    height: usize,
}

impl Raster<'_> {
    fn axes(&self, vertical: bool) -> (usize, usize) {
        if vertical {
            (self.width, self.height)
        } else {
            (self.height, self.width)
        }
    }

    fn ink(&self, vertical: bool, scan: usize, at: usize, threshold: u8) -> bool {
        let offset = if vertical {
            at * self.width + scan
        } else {
            scan * self.width + at
        };
        self.pixels[offset] < threshold
    }
}

fn strokes(
    raster: &Raster<'_>,
    vertical: bool,
    minimum: usize,
    threshold: u8,
) -> Option<Vec<Rule>> {
    let (count, run) = raster.axes(vertical);
    let mut rules: Vec<Rule> = Vec::new();
    for scan in 0..count {
        let mut first = 0;
        for at in 0..=run {
            if at < run && raster.ink(vertical, scan, at, threshold) {
                continue;
            }
            if at - first >= minimum {
                if let Some(rule) = rules
                    .iter_mut()
                    .rev()
                    .take_while(|rule| scan.saturating_sub(rule.at) <= 10)
                    .find(|rule| {
                        scan - rule.scan <= 2
                            && first.abs_diff(rule.first) <= RULE_MARGIN
                            && at.abs_diff(rule.last) <= RULE_MARGIN
                    })
                {
                    rule.at = (rule.at + scan) / 2;
                    rule.scan = scan;
                    rule.first = rule.first.min(first);
                    rule.last = rule.last.max(at);
                } else {
                    if rules.len() == MAX_RULES {
                        return None;
                    }
                    rules.push(Rule {
                        at: scan,
                        first,
                        last: at,
                        scan,
                    });
                }
            }
            first = at + 1;
        }
    }
    Some(rules)
}

fn faint_closures(raster: &Raster<'_>, strong: &[Rule], minimum: usize) -> Vec<[usize; 3]> {
    if strong.len() < 2 {
        return Vec::new();
    }
    let Some(faint) = strokes(raster, false, minimum, FAINT_RULE_THRESHOLD) else {
        return Vec::new();
    };
    faint
        .into_iter()
        .filter(|rule| {
            let matches = |rail: &Rule| {
                rule.first.abs_diff(rail.first) <= 3 && rule.last.abs_diff(rail.last) <= 3
            };
            let thin = rule.scan.checked_sub(4).is_none_or(|y| {
                (rule.first + 3..rule.last - 3)
                    .any(|x| !raster.ink(false, y, x, FAINT_RULE_THRESHOLD))
            });
            thin && strong
                .iter()
                .any(|rail| matches(rail) && rail.at + 3 < rule.at)
                && !strong
                    .iter()
                    .any(|rail| matches(rail) && rail.at.abs_diff(rule.at) <= 3)
        })
        .map(|rule| [rule.at, rule.first, rule.last])
        .collect()
}

fn intersects(h: Rule, v: Rule) -> bool {
    h.first.saturating_sub(RULE_MARGIN) <= v.at
        && v.at <= h.last + RULE_MARGIN
        && v.first.saturating_sub(RULE_MARGIN) <= h.at
        && h.at <= v.last + RULE_MARGIN
}

fn axes(rules: &[Rule]) -> Vec<usize> {
    let mut positions = rules.iter().map(|rule| rule.at).collect::<Vec<_>>();
    positions.sort_unstable();
    positions.dedup_by(|later, earlier| later.abs_diff(*earlier) <= RULE_MARGIN);
    positions
}

fn covers(rules: &[Rule], at: usize, first: usize, last: usize) -> bool {
    rules.iter().any(|rule| {
        rule.at.abs_diff(at) <= RULE_MARGIN
            && rule.first <= first + RULE_MARGIN
            && rule.last + RULE_MARGIN >= last
    })
}

fn separator(
    raster: &Raster<'_>,
    vertical: bool,
    rules: &[Rule],
    at: usize,
    first: usize,
    last: usize,
) -> Option<bool> {
    if covers(rules, at, first, last) {
        return Some(true);
    }
    let start = first + RULE_MARGIN;
    let end = last.saturating_sub(RULE_MARGIN);
    if start >= end
        || rules.iter().any(|rule| {
            rule.at.abs_diff(at) <= RULE_MARGIN && rule.first < end && start < rule.last
        })
    {
        return None;
    }

    // A short continuation of an edge can fall below the page-wide stroke
    // threshold. Treat it as uncertain instead of merging across it.
    const EDGE_RUN: usize = 8;
    let (count, _) = raster.axes(vertical);
    if end - start >= EDGE_RUN {
        for scan in at.saturating_sub(2)..=(at + 2).min(count - 1) {
            if (start..start + EDGE_RUN)
                .all(|point| raster.ink(vertical, scan, point, INK_THRESHOLD))
                || (end - EDGE_RUN..end)
                    .all(|point| raster.ink(vertical, scan, point, INK_THRESHOLD))
            {
                return None;
            }
        }
    }
    Some(false)
}

fn faint_panel(raster: &Raster<'_>, horizontal: &[Rule], vertical: &[Rule]) -> Option<RuleTable> {
    let x = axes(vertical);
    let y = axes(horizontal);
    let [left, right] = *x.as_slice() else {
        return None;
    };
    let [top, band, bottom] = *y.as_slice() else {
        return None;
    };
    if right - left <= 2 * RULE_MARGIN
        || band - top <= 2 * RULE_MARGIN
        || bottom - band < 2 * MIN_RUN
        || !y.iter().all(|at| covers(horizontal, *at, left, right))
        || !x.iter().all(|at| covers(vertical, *at, top, bottom))
    {
        return None;
    }
    let mut candidates = (left + RULE_MARGIN..right - RULE_MARGIN).filter(|&at| {
        let ink = |row| raster.pixels[row * raster.width + at] < FAINT_RULE_THRESHOLD;
        (band + RULE_MARGIN..bottom - RULE_MARGIN).all(ink)
            && !(top + RULE_MARGIN..band - RULE_MARGIN).all(ink)
    });
    let first = candidates.next()?;
    let mut last = first;
    for at in candidates {
        if at != last + 1 || at - first >= 4 {
            return None;
        }
        last = at;
    }
    Some(RuleTable {
        x: vec![left, first.midpoint(last), right],
        y,
        cells: vec![[0, 1, 0, 2], [1, 2, 0, 1], [1, 2, 1, 2]],
        frames: vec![],
    })
}

fn grid(
    raster: &Raster<'_>,
    horizontal: &[Rule],
    vertical: &[Rule],
    page_horizontal: &[Rule],
    page_vertical: &[Rule],
) -> Option<RuleTable> {
    let mut y = axes(horizontal);
    let mut x = axes(vertical);
    if x.len() < 3 || y.len() < 3 {
        return None;
    }
    let (left, right) = (x[0], *x.last()?);
    // Upper labels can use inset frames connected to the data grid.
    // Locate the tallest fully bordered base within the connected strokes.
    let rails = y
        .iter()
        .copied()
        .filter(|at| covers(horizontal, *at, left, right))
        .collect::<Vec<_>>();
    let mut base = None;
    for (i, &top) in rails.iter().enumerate() {
        if let Some(&bottom) = rails[i + 1..].iter().rev().find(|&&bottom| {
            covers(vertical, left, top, bottom) && covers(vertical, right, top, bottom)
        }) && base.is_none_or(|(first, last)| bottom - top > last - first)
        {
            base = Some((top, bottom));
        }
    }
    let (top, bottom) = base?;
    y.retain(|at| (top..=bottom).contains(at));
    x.retain(|at| {
        y.iter()
            .filter(|y| {
                vertical.iter().any(|rule| {
                    rule.at.abs_diff(*at) <= RULE_MARGIN
                        && rule.first.saturating_sub(RULE_MARGIN) <= **y
                        && **y <= rule.last + RULE_MARGIN
                })
            })
            .take(2)
            .count()
            == 2
    });
    if !(3..=65).contains(&y.len()) || !(3..=17).contains(&x.len()) {
        return None;
    }

    let columns = x.len() - 1;
    let rows = y.len() - 1;
    let mut taken = vec![false; columns * rows];
    let mut cells = Vec::new();
    for index in 0..taken.len() {
        if taken[index] {
            continue;
        }
        let mut pending = vec![index];
        taken[index] = true;
        let mut slots = 0;
        let (mut lo_x, mut lo_y, mut hi_x, mut hi_y) = (columns, rows, 0, 0);
        while let Some(next) = pending.pop() {
            let col = next % columns;
            let row = next / columns;
            lo_x = lo_x.min(col);
            lo_y = lo_y.min(row);
            hi_x = hi_x.max(col + 1);
            hi_y = hi_y.max(row + 1);
            slots += 1;
            let mut add = |other: usize| {
                if !taken[other] {
                    taken[other] = true;
                    pending.push(other);
                }
            };
            if col > 0 && !separator(raster, true, page_vertical, x[col], y[row], y[row + 1])? {
                add(next - 1);
            }
            if col + 1 < columns
                && !separator(raster, true, page_vertical, x[col + 1], y[row], y[row + 1])?
            {
                add(next + 1);
            }
            if row > 0 && !separator(raster, false, page_horizontal, y[row], x[col], x[col + 1])? {
                add(next - columns);
            }
            if row + 1 < rows
                && !separator(
                    raster,
                    false,
                    page_horizontal,
                    y[row + 1],
                    x[col],
                    x[col + 1],
                )?
            {
                add(next + columns);
            }
        }
        if slots != (hi_x - lo_x) * (hi_y - lo_y) {
            return None;
        }
        cells.push([lo_y, hi_y, lo_x, hi_x]);
    }
    let framed_body =
        x.len() == 3 && y.len() == 3 && cells == [[0, 1, 0, 2], [1, 2, 0, 1], [1, 2, 1, 2]];
    if cells.len() < 4 && !framed_body {
        return None;
    }

    let mut frames = Vec::new();
    let band = y[1] - top;
    for upper in axes(horizontal)
        .into_iter()
        .filter(|at| *at < top && top - *at <= 2 * band)
    {
        let cuts = x
            .iter()
            .enumerate()
            .filter(|(_, at)| covers(vertical, **at, upper, top))
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        for pair in cuts.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if covers(horizontal, upper, x[a], x[b]) && covers(horizontal, top, x[a], x[b]) {
                frames.push([upper, top, a, b]);
            }
        }
    }
    Some(RuleTable {
        x,
        y,
        cells,
        frames,
    })
}

pub(crate) fn analyze(pixels: &[u8], width: usize, height: usize) -> RuleLayout {
    let shortest = width.min(height);
    if shortest < MIN_RUN
        || pixels.len() > MAX_PIXELS
        || width.checked_mul(height) != Some(pixels.len())
    {
        return RuleLayout::default();
    }
    let raster = Raster {
        pixels,
        width,
        height,
    };
    let minimum = (shortest / 35).max(MIN_RUN);
    let Some(h) = strokes(&raster, false, minimum, INK_THRESHOLD) else {
        return RuleLayout::default();
    };
    let Some(v) = strokes(&raster, true, minimum, INK_THRESHOLD) else {
        return RuleLayout::default();
    };
    let mut visited = vec![false; h.len() + v.len()];
    let mut tables = Vec::new();
    let mut recovered_dividers = Vec::new();
    for first in 0..h.len() {
        if visited[first] {
            continue;
        }
        let mut pending = vec![first];
        let (mut rows, mut columns) = (Vec::new(), Vec::new());
        visited[first] = true;
        while let Some(line) = pending.pop() {
            if line < h.len() {
                rows.push(h[line]);
                for (i, column) in v.iter().enumerate() {
                    if !visited[h.len() + i] && intersects(h[line], *column) {
                        visited[h.len() + i] = true;
                        pending.push(h.len() + i);
                    }
                }
            } else {
                let column = v[line - h.len()];
                columns.push(column);
                for (i, row) in h.iter().enumerate() {
                    if !visited[i] && intersects(*row, column) {
                        visited[i] = true;
                        pending.push(i);
                    }
                }
            }
        }
        if let Some(table) = grid(&raster, &rows, &columns, &h, &v) {
            tables.push(table);
        } else if let Some(table) = faint_panel(&raster, &rows, &columns) {
            recovered_dividers.push([
                table.x[1],
                table.y[1] + RULE_MARGIN,
                table.y[2] - RULE_MARGIN,
            ]);
            tables.push(table);
        }
    }
    let vertical: Vec<_> = v
        .into_iter()
        .filter(|stroke| h.iter().any(|&rail| intersects(rail, *stroke)))
        .map(|stroke| [stroke.at, stroke.first, stroke.last])
        .chain(recovered_dividers)
        .collect();
    let faint_closures = if vertical.is_empty() {
        faint_closures(&raster, &h, minimum)
    } else {
        Vec::new()
    };
    let horizontal = h
        .into_iter()
        .map(|stroke| [stroke.at, stroke.first, stroke.last])
        .collect();
    RuleLayout {
        tables,
        vertical,
        horizontal,
        faint_closures,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIDE: usize = 300;

    #[test]
    fn reports_thin_faint_closure_separately_from_strong_rules() {
        let mut pixels = vec![255; SIDE * SIDE];
        let stroke = |y, pixels: &mut [u8], ink| pixels[y * SIDE + 20..y * SIDE + 280].fill(ink);
        for y in [70, 140] {
            stroke(y, &mut pixels, 0);
        }
        stroke(220, &mut pixels, 165);
        let layout = analyze(&pixels, SIDE, SIDE);
        assert_eq!(layout.horizontal, [[70, 20, 280], [140, 20, 280]]);
        assert_eq!(layout.faint_closures, [[220, 20, 280]]);
        assert!(layout.tables.is_empty());

        pixels[220 * SIDE + 100] = 255;
        assert!(analyze(&pixels, SIDE, SIDE).faint_closures.is_empty());

        for y in 220..225 {
            stroke(y, &mut pixels, 165);
        }
        assert!(analyze(&pixels, SIDE, SIDE).faint_closures.is_empty());
    }

    fn propose(pixels: &[u8], width: usize, height: usize) -> Vec<RuleTable> {
        analyze(pixels, width, height).tables
    }

    fn horizontal_rails() -> Vec<u8> {
        let mut pixels = vec![255; SIDE * SIDE];
        for y in [40, 80, 130, 200] {
            pixels[y * SIDE + 30..y * SIDE + 220].fill(0);
        }
        pixels
    }

    fn merged_top() -> Vec<u8> {
        let mut pixels = horizontal_rails();
        for (x, first) in [(30, 40), (100, 80), (220, 40)] {
            for y in first..=200 {
                pixels[y * SIDE + x] = 0;
            }
        }
        pixels
    }

    #[test]
    fn fully_ruled_grid_with_merged_top() {
        assert_eq!(
            propose(&merged_top(), SIDE, SIDE),
            [RuleTable {
                x: vec![30, 100, 220],
                y: vec![40, 80, 130, 200],
                frames: vec![],
                cells: vec![
                    [0, 1, 0, 2],
                    [1, 2, 0, 1],
                    [1, 2, 1, 2],
                    [2, 3, 0, 1],
                    [2, 3, 1, 2]
                ],
            }],
        );
    }

    #[test]
    fn caption_rules_cannot_propose_cells() {
        assert!(propose(&horizontal_rails(), SIDE, SIDE).is_empty());
    }

    #[test]
    fn retains_corroborated_stroke_without_an_enclosed_grid() {
        let mut pixels = horizontal_rails();
        for y in 40..=80 {
            pixels[y * SIDE + 100] = 0;
        }
        for y in 88..=123 {
            pixels[y * SIDE + 250] = 0;
        }
        let layout = analyze(&pixels, SIDE, SIDE);
        assert!(layout.tables.is_empty());
        assert_eq!(layout.vertical, [[100, 40, 81]]);
    }

    #[test]
    fn proposes_enclosed_two_column_body_without_row_rules() {
        let mut pixels = merged_top();
        pixels[130 * SIDE + 31..130 * SIDE + 100].fill(255);
        pixels[130 * SIDE + 101..130 * SIDE + 220].fill(255);
        let tables = propose(&pixels, SIDE, SIDE);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].cells, [[0, 1, 0, 2], [1, 2, 0, 1], [1, 2, 1, 2]]);
    }

    #[test]
    fn recovers_only_one_continuous_faint_divider_inside_a_strong_frame() {
        let mut pixels = merged_top();
        pixels[130 * SIDE + 31..130 * SIDE + 220].fill(255);
        for y in 81..200 {
            pixels[y * SIDE + 100] = 162;
        }
        let layout = analyze(&pixels, SIDE, SIDE);
        assert_eq!(layout.tables[0].x, [30, 100, 220]);
        assert!(layout.vertical.contains(&[100, 85, 195]));

        let mut broken = pixels.clone();
        broken[120 * SIDE + 100] = 255;
        assert!(propose(&broken, SIDE, SIDE).is_empty());
        for y in 81..200 {
            pixels[y * SIDE + 165] = 162;
        }
        assert!(propose(&pixels, SIDE, SIDE).is_empty());
    }

    #[test]
    fn rejects_incomplete_separators_instead_of_merging() {
        for range in [41..=54, 46..=71] {
            let mut pixels = merged_top();
            for y in range {
                pixels[y * SIDE + 100] = 0;
            }
            assert!(propose(&pixels, SIDE, SIDE).is_empty());
        }
    }

    #[test]
    fn rejects_invalid_or_oversized_rasters() {
        assert!(propose(&[], 0, SIDE).is_empty());
        assert!(propose(&[0; 17], SIDE, SIDE).is_empty());
        assert!(propose(&[0; 1], usize::MAX, SIDE).is_empty());
        assert!(propose(&vec![0; MAX_PIXELS + 25], 25, MAX_PIXELS / 25 + 1).is_empty());
    }

    #[test]
    fn stops_after_the_rule_budget_on_stripes() {
        let width = 128;
        let height = (MAX_RULES + 1) * 12;
        let mut pixels = vec![255; width * height];
        for y in (0..height).step_by(12) {
            pixels[y * width..(y + 1) * width].fill(0);
        }
        let raster = Raster {
            pixels: &pixels,
            width,
            height,
        };
        assert!(strokes(&raster, false, MIN_RUN, INK_THRESHOLD).is_none());
        assert!(propose(&pixels, width, height).is_empty());
    }
}
