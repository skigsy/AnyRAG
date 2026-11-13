//! Propose unruled tables from recurring whitespace between OCR words.

pub(crate) struct LaneTable {
    pub(crate) cells: Vec<LaneCell>,
    pub(crate) upper: Vec<Vec<usize>>,
    pub(crate) cuts: Vec<f64>,
    pub(crate) ambiguous_rows: Vec<usize>,
}

pub(crate) struct LaneCell {
    pub(crate) row: [usize; 2],
    pub(crate) column: [usize; 2],
    pub(crate) lines: Vec<Vec<usize>>,
}

type WordBox = [f64; 4];
type WordLine = Vec<usize>;
pub(crate) type Rail = [usize; 3];

#[derive(PartialEq)]
struct GridRow {
    columns: Vec<Vec<WordLine>>,
    ambiguous: bool,
}

const MAX_ROWS: usize = 64;
const MAX_COLUMNS: usize = 16;
const RAIL_TOLERANCE: usize = 3;

struct Page<'a> {
    words: &'a [WordBox],
    rows: Vec<WordLine>,
    centers: Vec<f64>,
    height: f64,
    strict_wraps: bool,
}

#[derive(Clone, Copy)]
enum Alignment {
    Left,
    Center,
    Right,
}

impl Alignment {
    fn coordinate(self, [left, right]: [f64; 2]) -> f64 {
        match self {
            Self::Left => left,
            Self::Center => left.midpoint(right),
            Self::Right => right,
        }
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        values[middle - 1].midpoint(values[middle])
    } else {
        values[middle]
    }
}

fn y_center(word: WordBox) -> f64 {
    word[1].midpoint(word[3])
}

fn same_span(a: Rail, b: Rail) -> bool {
    a[1].abs_diff(b[1]).max(a[2].abs_diff(b[2])) <= RAIL_TOLERANCE
}

impl<'a> Page<'a> {
    fn new(words: &'a [WordBox]) -> Self {
        let height = median(words.iter().map(|word| word[3] - word[1]).collect());
        Self::at_height(words, height, false)
    }

    fn at_height(words: &'a [WordBox], height: f64, strict_wraps: bool) -> Self {
        let mut order: Vec<_> = (0..words.len()).collect();
        order.sort_by(|&left, &right| {
            y_center(words[left])
                .total_cmp(&y_center(words[right]))
                .then_with(|| words[left][0].total_cmp(&words[right][0]))
        });
        let mut rows: Vec<WordLine> = Vec::new();
        for id in order {
            if rows
                .last()
                .is_none_or(|row| y_center(words[id]) - y_center(words[row[0]]) > 0.52 * height)
            {
                rows.push(Vec::new());
            }
            rows.last_mut().unwrap().push(id);
        }
        let centers = rows
            .iter_mut()
            .map(|row| {
                row.sort_by(|&left, &right| words[left][0].total_cmp(&words[right][0]));
                median(row.iter().map(|&id| y_center(words[id])).collect())
            })
            .collect();
        Self {
            words,
            rows,
            centers,
            height,
            strict_wraps,
        }
    }

    fn extent(&self, line: &[usize]) -> [f64; 2] {
        line.iter()
            .fold([f64::INFINITY, f64::NEG_INFINITY], |[left, right], &id| {
                [left.min(self.words[id][0]), right.max(self.words[id][2])]
            })
    }

    fn fragments<'b>(&self, row: &'b [usize]) -> Vec<&'b [usize]> {
        let mut fragments = Vec::new();
        let mut start = 0;
        let mut right = self.words[row[0]][2];
        for (i, &id) in row.iter().enumerate().skip(1) {
            let word = self.words[id];
            if word[0] - right > self.height {
                fragments.push(&row[start..i]);
                start = i;
            }
            right = right.max(word[2]);
        }
        fragments.push(&row[start..]);
        fragments
    }

    fn dense_runs(&self) -> Vec<Vec<usize>> {
        let mut runs: Vec<Vec<usize>> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            if self.fragments(row).len() < 3 {
                continue;
            }
            if runs.last().is_none_or(|run| {
                self.centers[i] - self.centers[*run.last().unwrap()] > 5.8 * self.height
            }) {
                runs.push(Vec::new());
            }
            runs.last_mut().unwrap().push(i);
        }
        runs
    }

    fn cuts(&self, start: usize, end: usize, dense: &[usize]) -> Vec<f64> {
        // Sweep simultaneous ink and gap coverage. Each dense row contributes
        // at most one gap vote at any x because its fragments are disjoint.
        let mut events = Vec::new();
        for &id in self.rows[start..end].iter().flatten() {
            let [left, _, right, _] = self.words[id];
            events.push((left - 0.08 * self.height, 1, 0));
            events.push((right + 0.08 * self.height, -1, 0));
        }
        for &index in dense {
            let fragments = self.fragments(&self.rows[index]);
            for pair in fragments.windows(2) {
                events.push((self.extent(pair[0])[1] + 0.12 * self.height, 0, 1));
                events.push((self.extent(pair[1])[0] - 0.12 * self.height, 0, -1));
            }
        }
        events.sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
        let required = (0.66 * dense.len() as f64).ceil() as i32;
        let mut ink = 0;
        let mut gaps = 0;
        let mut intervals: Vec<(f64, f64)> = Vec::new();
        let mut events = events.into_iter().peekable();
        while let Some((left, add_ink, add_gap)) = events.next() {
            ink += add_ink;
            gaps += add_gap;
            let Some(&(right, _, _)) = events.peek() else {
                break;
            };
            if left == right || ink != 0 || gaps < required {
                continue;
            }
            if let Some(previous) = intervals.last_mut().filter(|range| range.1 == left) {
                previous.1 = right;
            } else {
                intervals.push((left, right));
            }
        }
        intervals
            .into_iter()
            .filter(|(left, right)| right - left >= 0.22 * self.height)
            .map(|(left, right)| left.midpoint(right))
            .collect()
    }

    fn slots(&self, row: &[usize], cuts: &[f64]) -> Option<Vec<WordLine>> {
        let mut slots = vec![Vec::new(); cuts.len() + 1];
        for &id in row {
            let [left, _, right, _] = self.words[id];
            let lane = cuts.partition_point(|cut| *cut <= left.midpoint(right));
            if (lane > 0 && left < cuts[lane - 1]) || (lane < cuts.len() && right > cuts[lane]) {
                return None;
            }
            slots[lane].push(id);
        }
        Some(slots)
    }

    fn alignments(&self, body: &[GridRow], lanes: usize) -> Option<Vec<Alignment>> {
        (0..lanes)
            .map(|lane| {
                let observed: Vec<_> = body
                    .iter()
                    .filter_map(|row| row.columns[lane].first().map(|line| self.extent(line)))
                    .collect();
                if observed.len() < 2.max((0.60 * body.len() as f64).ceil() as usize) {
                    return None;
                }
                let mut best: Option<(Alignment, usize, f64)> = None;
                for mode in [Alignment::Left, Alignment::Center, Alignment::Right] {
                    let values: Vec<_> = observed
                        .iter()
                        .map(|&bounds| mode.coordinate(bounds))
                        .collect();
                    let center = median(values.clone());
                    let support = values
                        .iter()
                        .filter(|value| (*value - center).abs() <= 0.9 * self.height)
                        .count();
                    let spread = values.iter().fold(f64::NEG_INFINITY, |a, &b| a.max(b))
                        - values.iter().fold(f64::INFINITY, |a, &b| a.min(b));
                    if best.is_none_or(|(_, count, width)| {
                        support > count || (support == count && spread < width)
                    }) {
                        best = Some((mode, support, spread));
                    }
                }
                let (mode, support, _) = best?;
                (support >= 2.max((0.70 * observed.len() as f64).ceil() as usize)).then_some(mode)
            })
            .collect()
    }

    fn grid(
        &self,
        first: usize,
        last: usize,
        cuts: &[f64],
        step: f64,
    ) -> Option<(Vec<GridRow>, Vec<Alignment>)> {
        let lanes = cuts.len() + 1;
        let continuation = (1.9 * self.height).min(0.66 * step);
        let mut grid: Vec<GridRow> = Vec::new();
        for i in first..last {
            let slots = self.slots(&self.rows[i], cuts)?;
            let occupied = slots.iter().filter(|line| !line.is_empty()).count();
            let short = i > first && self.centers[i] - self.centers[i - 1] <= continuation;
            // A close multi-column line can start another real row. The
            // retry accepts only the header continuation with an empty stub.
            if self.strict_wraps
                && short
                && occupied >= 2
                && (grid.len() != 1 || !slots[0].is_empty())
            {
                return None;
            }
            if !slots[0].is_empty() && occupied >= 2 && !short {
                if grid.len() == MAX_ROWS {
                    return None;
                }
                grid.push(GridRow {
                    columns: vec![Vec::new(); lanes],
                    ambiguous: false,
                });
            } else if !short || grid.is_empty() {
                return None;
            }
            let row = grid.last_mut()?;
            // The same two bands can be distinct records or one wrapped row.
            row.ambiguous |= short && !slots[0].is_empty() && occupied >= 2;
            for (cell, line) in row.columns.iter_mut().zip(slots) {
                if !line.is_empty() {
                    cell.push(line);
                }
            }
        }
        if grid.len() < 3
            || grid[0].columns.iter().any(Vec::is_empty)
            || grid[1..]
                .iter()
                .filter(|row| row.columns.iter().all(|cell| !cell.is_empty()))
                .count()
                < 2
        {
            return None;
        }
        let alignment = self.alignments(&grid[1..], lanes)?;
        Some((grid, alignment))
    }

    fn continuation_end(&self, mut end: usize, limit: usize) -> usize {
        while end < limit && self.centers[end] - self.centers[end - 1] <= 1.8 * self.height {
            end += 1;
        }
        end
    }

    fn sparse_row_fits(
        &self,
        row: usize,
        cuts: &[f64],
        grid: &[GridRow],
        alignment: &[Alignment],
    ) -> bool {
        let Some(slots) = self.slots(&self.rows[row], cuts) else {
            return false;
        };
        let occupied = slots.iter().filter(|line| !line.is_empty()).count();
        if slots[0].is_empty() || !(2..slots.len()).contains(&occupied) {
            return false;
        }
        slots.iter().enumerate().all(|(i, slot)| {
            if slot.is_empty() {
                return true;
            }
            let mode = alignment[i];
            let typical = median(
                grid[1..]
                    .iter()
                    .filter_map(|row| {
                        row.columns[i]
                            .first()
                            .map(|line| mode.coordinate(self.extent(line)))
                    })
                    .collect(),
            );
            (mode.coordinate(self.extent(slot)) - typical).abs() <= 1.2 * self.height
        })
    }

    fn split_folds(&self, grid: Vec<GridRow>, horizontal: &[Rail]) -> Vec<GridRow> {
        if horizontal.len() < 3 || !grid.iter().skip(1).any(|row| row.ambiguous) {
            return grid;
        }
        let words_in = |rows: &[GridRow]| {
            rows.iter()
                .flat_map(|row| row.columns.iter().flatten().flatten())
                .map(|&id| self.words[id])
                .collect::<Vec<_>>()
        };
        let leaves = words_in(&grid[..1]);
        let leaf_bottom = leaves.iter().map(|w| w[3]).fold(0.0, f64::max);
        let height = median(leaves.iter().map(|w| w[3] - w[1]).collect());
        let [left, top, right, bottom] = words_in(&grid[1..]).into_iter().fold(
            [f64::INFINITY, f64::INFINITY, 0.0, 0.0],
            |[l, t, r, b], w| [l.min(w[0]), t.min(w[1]), r.max(w[2]), b.max(w[3])],
        );
        let covers = |rail: Rail| rail[1] as f64 <= left && right <= rail[2] as f64;
        let unique = |min: f64, max: f64, before: bool| {
            let mut matches = horizontal.iter().copied().filter(|rail| {
                let y = rail[0] as f64;
                min <= y && y <= max && (!before || y < top) && covers(*rail)
            });
            let first = matches.next()?;
            matches.next().is_none().then_some(first)
        };
        let (Some(upper), Some(lower)) = (
            unique(leaf_bottom, leaf_bottom + 2.0 * height, true),
            unique(bottom, bottom + 2.0 * height, false),
        ) else {
            return grid;
        };
        if !same_span(upper, lower) {
            return grid;
        }
        let rails = horizontal
            .iter()
            .copied()
            .filter(|rail| {
                upper[0] < rail[0]
                    && rail[0] < lower[0]
                    && same_span(upper, *rail)
                    && same_span(lower, *rail)
            })
            .collect::<Vec<_>>();
        let mut split_budget = MAX_ROWS - grid.len();
        let mut resolved = Vec::with_capacity(grid.len());
        for mut row in grid {
            if split_budget > 0 && row.ambiguous && row.columns.iter().all(|lines| lines.len() == 2)
            {
                let first_end = row
                    .columns
                    .iter()
                    .flat_map(|lines| &lines[0])
                    .map(|&id| self.words[id][3])
                    .fold(0.0, f64::max);
                let second_start = row
                    .columns
                    .iter()
                    .flat_map(|lines| &lines[1])
                    .map(|&id| self.words[id][1])
                    .fold(f64::INFINITY, f64::min);
                let aligned = row.columns.iter().all(|lines| {
                    let [a, b] = self.extent(&lines[0]);
                    let [c, d] = self.extent(&lines[1]);
                    (a - c)
                        .abs()
                        .min((b - d).abs())
                        .min((a.midpoint(b) - c.midpoint(d)).abs())
                        <= 0.9 * height
                });
                if aligned
                    && rails
                        .iter()
                        .filter(|rail| {
                            let y = rail[0] as f64;
                            first_end <= y && y < second_start
                        })
                        .count()
                        == 1
                {
                    let second = row
                        .columns
                        .iter_mut()
                        .map(|lines| lines.split_off(1))
                        .collect();
                    split_budget -= 1;
                    row.ambiguous = false;
                    resolved.push(row);
                    resolved.push(GridRow {
                        columns: second,
                        ambiguous: false,
                    });
                    continue;
                }
            }
            resolved.push(row);
        }
        resolved
    }

    fn table(&self, run: &[usize], limit: usize, horizontal: &[Rail]) -> Option<LaneTable> {
        if run.len() < 3 {
            return None;
        }
        let first = run[0];
        let mut last = self.continuation_end(run[run.len() - 1] + 1, limit);
        let cuts = self.cuts(first, last, run);
        if !(2..MAX_COLUMNS).contains(&cuts.len()) {
            return None;
        }
        let step = median(
            run.windows(2)
                .map(|pair| self.centers[pair[1]] - self.centers[pair[0]])
                .collect(),
        );
        let (mut grid, mut alignment) = self.grid(first, last, &cuts, step)?;
        // A sparse tail may continue the established lanes and cadence. Keep
        // the cuts and reject the extension if it rewrites earlier cells.
        while last < limit
            && self.centers[last] - self.centers[last - 1] <= (5.8 * self.height).min(1.55 * step)
            && self.sparse_row_fits(last, &cuts, &grid, &alignment)
        {
            let end = self.continuation_end(last + 1, limit);
            let Some((extended, next_alignment)) = self.grid(first, end, &cuts, step) else {
                break;
            };
            if extended.len() != grid.len() + 1 || extended[..grid.len()] != grid {
                break;
            }
            last = end;
            grid = extended;
            alignment = next_alignment;
        }

        let mut cells = Vec::new();
        let mut ambiguous_rows = Vec::new();
        for (row, slots) in self.split_folds(grid, horizontal).into_iter().enumerate() {
            if row > 0 && slots.ambiguous {
                ambiguous_rows.push(row);
            }
            for (column, lines) in slots.columns.into_iter().enumerate() {
                if !lines.is_empty() {
                    cells.push(LaneCell {
                        row: [row, row + 1],
                        column: [column, column + 1],
                        lines,
                    });
                }
            }
        }
        let mut upper = Vec::new();
        for i in first.saturating_sub(2)..first {
            let distance = self.centers[first] - self.centers[i];
            if distance > 0.0 && distance <= 4.5 * self.height {
                upper.extend(self.fragments(&self.rows[i]).into_iter().map(<[_]>::to_vec));
            }
        }
        Some(LaneTable {
            cells,
            upper,
            cuts,
            ambiguous_rows,
        })
    }
}

/// Word boxes are [left, top, right, bottom]. Proposals retain the input word
/// IDs. Nearby upper fragments are candidates for a separate header verifier.
pub(crate) fn propose(words: &[[f64; 4]], horizontal: &[Rail]) -> Vec<LaneTable> {
    if words.len() < 9
        || words.iter().any(|word| {
            word.iter().any(|value| !value.is_finite())
                || word[0] >= word[2]
                || word[1] >= word[3]
                || !(word[3] - word[1]).is_finite()
        })
    {
        return Vec::new();
    }
    let collect = |page: &Page| {
        let runs = page.dense_runs();
        runs.iter()
            .enumerate()
            .filter_map(|(i, run)| {
                let limit = runs.get(i + 1).map_or(page.rows.len(), |next| next[0]);
                page.table(run, limit, horizontal)
            })
            .collect::<Vec<_>>()
    };
    let page = Page::new(words);
    let tables = collect(&page);
    if !tables.is_empty() {
        return tables;
    }
    // Individual glyph boxes can understate the physical line height.
    let height = median(
        page.rows
            .iter()
            .map(|line| {
                let (top, bottom) = line
                    .iter()
                    .fold((f64::INFINITY, f64::NEG_INFINITY), |(top, bottom), &id| {
                        (top.min(words[id][1]), bottom.max(words[id][3]))
                    });
                bottom - top
            })
            .collect(),
    )
    .min(1.33 * page.height);
    if height > page.height {
        collect(&Page::at_height(words, height, true))
    } else {
        tables
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn complete_row(y: f64) -> [WordBox; 3] {
        [10.0, 80.0, 170.0].map(|x| [x, y, x + 20.0, y + 20.0])
    }

    #[test]
    fn retry_does_not_fold_a_close_body_row_into_its_predecessor() {
        let words: Vec<_> = [10.0, 60.0, 85.0, 140.0]
            .into_iter()
            .flat_map(complete_row)
            .collect();
        let cuts = [60.0, 140.0];
        let (rows, _) = Page::at_height(&words, 20.0, false)
            .grid(0, 4, &cuts, 50.0)
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.ambiguous).collect::<Vec<_>>(),
            [false, true, false]
        );
        assert!(
            Page::at_height(&words, 20.0, true)
                .grid(0, 4, &cuts, 50.0)
                .is_none()
        );
    }

    #[test]
    fn full_span_rule_resolves_only_the_fold_it_separates() {
        let words: Vec<_> = [10.0, 60.0, 85.0, 140.0, 165.0]
            .into_iter()
            .flat_map(complete_row)
            .collect();
        let page = Page::at_height(&words, 20.0, false);
        let status = |internal| {
            let (rows, _) = page.grid(0, 5, &[60.0, 140.0], 50.0).unwrap();
            page.split_folds(rows, &[[40, 0, 210], internal, [200, 0, 210]])
                .into_iter()
                .map(|row| row.ambiguous)
                .collect::<Vec<_>>()
        };
        assert_eq!(status([82, 0, 210]), [false, false, false, true]);
        assert_eq!(status([82, 5, 210]), [false, true, true]);
        assert_eq!(status([75, 0, 210]), [false, true, true]);
    }

    #[test]
    fn retry_keeps_a_wrapped_header_with_an_empty_stub() {
        let words: Vec<_> = complete_row(10.0)
            .into_iter()
            .chain(complete_row(38.0).into_iter().skip(1))
            .chain(complete_row(88.0))
            .chain(complete_row(148.0))
            .collect();
        let (rows, _) = Page::at_height(&words, 20.0, true)
            .grid(0, 4, &[60.0, 140.0], 60.0)
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0].columns.iter().map(Vec::len).collect::<Vec<_>>(),
            [1, 2, 2]
        );
        assert!(!rows[0].ambiguous);
    }
}
