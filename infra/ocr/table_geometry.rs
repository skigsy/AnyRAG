//! Bounded local geometry proposals. Only numeric observations cross this boundary.

mod table_lanes;
mod table_rules;

use std::io::{self, Read, Write};

const MAX_PIXELS: usize = 6_000_000;
const MAX_WORDS: usize = 8192;
const MAX_LANE_RAILS: usize = 128;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid OCR geometry input")
}

fn number(token: &[u8]) -> io::Result<usize> {
    std::str::from_utf8(token)
        .map_err(|_| invalid())?
        .parse()
        .map_err(|_| invalid())
}

fn pgm(mut input: &[u8]) -> io::Result<(&[u8], usize, usize)> {
    let mut token = || {
        let skip = input
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        input = &input[skip..];
        let end = input
            .iter()
            .position(|byte| byte.is_ascii_whitespace())
            .ok_or_else(invalid)?;
        let (value, remaining) = input.split_at(end);
        input = &remaining[1..];
        Ok::<_, io::Error>(value)
    };
    if token()? != b"P5" {
        return Err(invalid());
    }
    let width = number(token()?)?;
    let height = number(token()?)?;
    if token()? != b"255"
        || width == 0
        || height == 0
        || input.len() > MAX_PIXELS
        || width.checked_mul(height) != Some(input.len())
    {
        return Err(invalid());
    }
    Ok((input, width, height))
}

fn boxes(input: &[u8]) -> io::Result<Vec<[f64; 4]>> {
    let input = std::str::from_utf8(input).map_err(|_| invalid())?;
    let mut words = Vec::new();
    for line in input.lines() {
        let mut values = line.split_ascii_whitespace();
        let mut rect = [0.0; 4];
        for value in &mut rect {
            *value = values
                .next()
                .ok_or_else(invalid)?
                .parse()
                .map_err(|_| invalid())?;
            if !(0.0..=MAX_PIXELS as f64).contains(value) {
                return Err(invalid());
            }
        }
        if values.next().is_some()
            || rect[0] >= rect[2]
            || rect[1] >= rect[3]
            || words.len() == MAX_WORDS
        {
            return Err(invalid());
        }
        words.push(rect);
    }
    Ok(words)
}

fn lane_input(input: &[u8]) -> io::Result<(Vec<table_lanes::Rail>, Vec<[f64; 4]>)> {
    let input = std::str::from_utf8(input).map_err(|_| invalid())?;
    let (rails, words) = input.split_once("\n\n").unwrap_or(("", input));
    let mut horizontal = Vec::new();
    for line in rails.lines() {
        let mut parts = line.split_ascii_whitespace();
        let mut rail = [0; 3];
        for value in &mut rail {
            *value = number(parts.next().ok_or_else(invalid)?.as_bytes())?;
        }
        if parts.next().is_some()
            || rail[1] >= rail[2]
            || rail.iter().any(|&value| value > MAX_PIXELS)
            || horizontal.len() == MAX_LANE_RAILS
        {
            return Err(invalid());
        }
        horizontal.push(rail);
    }
    Ok((horizontal, boxes(words.as_bytes())?))
}

fn run() -> io::Result<()> {
    let mode = std::env::args().nth(1).ok_or_else(invalid)?;
    let limit = match mode.as_str() {
        "rules" => MAX_PIXELS + 128,
        "lanes" => MAX_WORDS * 96 + MAX_LANE_RAILS * 32 + 2,
        _ => return Err(invalid()),
    };
    let mut data = Vec::new();
    io::stdin()
        .take((limit + 1) as u64)
        .read_to_end(&mut data)?;
    if data.len() > limit {
        return Err(invalid());
    }
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    if mode == "rules" {
        let (pixels, width, height) = pgm(&data)?;
        let layout = table_rules::analyze(pixels, width, height);
        write!(
            stdout,
            r#"{{"vertical":{:?},"horizontal":{:?},"faint_closures":{:?},"tables":["#,
            layout.vertical, layout.horizontal, layout.faint_closures
        )?;
        for (index, table) in layout.tables.into_iter().enumerate() {
            if index > 0 {
                write!(stdout, ",")?;
            }
            write!(
                stdout,
                r#"{{"x":{:?},"y":{:?},"cells":{:?},"frames":{:?}}}"#,
                table.x, table.y, table.cells, table.frames
            )?;
        }
        writeln!(stdout, "]}}")?;
    } else {
        let (horizontal, words) = lane_input(&data)?;
        for table in table_lanes::propose(&words, &horizontal) {
            write!(
                stdout,
                r#"{{"cuts":{:?},"upper":{:?},"ambiguous_rows":{:?},"cells":["#,
                table.cuts, table.upper, table.ambiguous_rows
            )?;
            for (index, cell) in table.cells.iter().enumerate() {
                if index > 0 {
                    write!(stdout, ",")?;
                }
                write!(
                    stdout,
                    r#"{{"row":{:?},"column":{:?},"lines":{:?}}}"#,
                    cell.row, cell.column, cell.lines
                )?;
            }
            writeln!(stdout, "]}}")?;
        }
    }
    stdout.flush()
}

fn main() -> io::Result<()> {
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_boundary_preserves_white_pixels_and_checks_length() {
        let (pixels, width, height) = pgm(b"P5\n2 2\n255\n\n \x00\xff").unwrap();
        assert_eq!((width, height, pixels), (2, 2, &b"\n \x00\xff"[..]));
        assert!(pgm(b"P5\n2 2\n255\n\x00\xff").is_err());
    }

    #[test]
    fn geometry_uses_only_bounded_numeric_word_boxes() {
        assert_eq!(boxes(b"1\t2\t3\t4\n").unwrap(), vec![[1.0, 2.0, 3.0, 4.0]]);
        assert!(boxes(b"NaN 2 3 4\n").is_err());
        assert!(boxes(b"3 2 1 4\n").is_err());
        assert!(boxes(b"1 2 3 4 text\n").is_err());
    }

    #[test]
    fn lane_rails_are_bounded_and_separate_from_word_boxes() {
        assert_eq!(
            lane_input(b"82\t0\t210\n\n1\t2\t3\t4\n").unwrap(),
            (vec![[82, 0, 210]], vec![[1.0, 2.0, 3.0, 4.0]])
        );
        assert!(lane_input(b"82\t220\t210\n\n1\t2\t3\t4\n").is_err());
        assert!(lane_input(b"82\t0\t6000001\n\n1\t2\t3\t4\n").is_err());
        assert!(lane_input(b"82\t0\t210\t9\n\n1\t2\t3\t4\n").is_err());
    }
}
