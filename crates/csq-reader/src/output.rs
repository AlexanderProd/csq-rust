//! Turning values into bytes on stdout.
//!
//! Every writer here is line-oriented and streams: nothing is buffered up into
//! a document first, so `csq-reader temps … | head -3` stops early instead of
//! decoding and formatting a whole recording. The JSON is written by hand for
//! the same reason, and because field order carries meaning to a reader in a
//! way that alphabetical order does not.

use std::io::{self, Write};

/// One value on its way out, in the unit it was measured in.
///
/// The variant decides how the value is rendered in *both* the text and the
/// JSON output, which is what keeps `-F tsv` and `-F json` agreeing digit for
/// digit rather than drifting apart.
#[derive(Clone)]
pub enum Cell {
    Text(String),
    Int(u64),
    /// A measurement printed at its full `f32` precision, for calibration
    /// constants where every digit is meaningful.
    Float(f32),
    /// A temperature, printed with the decimals the caller asked for.
    Fixed(f32, usize),
    /// A position in the recording, in seconds.
    Seconds(f64),
    /// An already-rendered number, for the few values whose formatting is
    /// decided at the call site. The text must be a valid JSON number.
    Number(String),
}

/// Decimals for a playback position: enough to keep frames apart at any frame
/// rate a camera can record, and no more.
const TIME_DECIMALS: usize = 6;

impl Cell {
    /// The value as it appears in a text column.
    pub fn text(&self) -> String {
        match self {
            Cell::Text(text) => text.clone(),
            Cell::Int(value) => value.to_string(),
            Cell::Float(value) if value.is_finite() => value.to_string(),
            Cell::Fixed(value, decimals) if value.is_finite() => format!("{value:.decimals$}"),
            Cell::Seconds(value) => format!("{value:.TIME_DECIMALS$}"),
            Cell::Number(text) => text.clone(),
            // A count outside the calibration curve's domain has no
            // temperature; say so rather than printing a plausible number.
            Cell::Float(_) | Cell::Fixed(..) => "nan".to_owned(),
        }
    }

    pub fn write_json_to(&self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Cell::Text(text) => write_json_string(out, text),
            Cell::Int(value) => write!(out, "{value}"),
            Cell::Float(value) if value.is_finite() => write!(out, "{value}"),
            Cell::Fixed(value, decimals) if value.is_finite() => write!(out, "{value:.decimals$}"),
            Cell::Seconds(value) => write!(out, "{value:.TIME_DECIMALS$}"),
            Cell::Number(text) => out.write_all(text.as_bytes()),
            // JSON has no NaN, and `null` is the honest stand-in.
            Cell::Float(_) | Cell::Fixed(..) => out.write_all(b"null"),
        }
    }
}

/// JSON has no NaN and no infinity; `\u` escaping covers whatever a camera
/// wrote into a string field.
fn write_json_string(out: &mut impl Write, value: &str) -> io::Result<()> {
    out.write_all(b"\"")?;
    for character in value.chars() {
        match character {
            '"' => out.write_all(b"\\\"")?,
            '\\' => out.write_all(b"\\\\")?,
            '\n' => out.write_all(b"\\n")?,
            '\r' => out.write_all(b"\\r")?,
            '\t' => out.write_all(b"\\t")?,
            control if (control as u32) < 0x20 => write!(out, "\\u{:04x}", control as u32)?,
            other => write!(out, "{other}")?,
        }
    }
    out.write_all(b"\"")
}

// ------------------------------------------------------------------- pairs --

/// A named value. Keys are dotted paths (`camera.model`) so that the flat and
/// the nested renderings carry exactly the same information.
pub type Pair = (String, Cell);

/// Collects named values in the order they should be printed.
#[derive(Default)]
pub struct Pairs(Vec<Pair>);

impl Pairs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn into_vec(self) -> Vec<Pair> {
        self.0
    }

    pub fn text(&mut self, key: &str, value: &str) {
        self.0.push((key.to_owned(), Cell::Text(value.to_owned())));
    }

    /// Records a string unless it is empty, which is how the container spells
    /// "the camera did not fill this field in".
    pub fn text_if_set(&mut self, key: &str, value: &str) {
        if !value.trim().is_empty() {
            self.text(key, value);
        }
    }

    pub fn int(&mut self, key: &str, value: u64) {
        self.0.push((key.to_owned(), Cell::Int(value)));
    }

    pub fn float(&mut self, key: &str, value: f32) {
        self.0.push((key.to_owned(), Cell::Float(value)));
    }

    pub fn seconds(&mut self, key: &str, value: f64) {
        self.0.push((key.to_owned(), Cell::Seconds(value)));
    }

    /// Records a number the caller has already rendered.
    pub fn number(&mut self, key: &str, value: String) {
        self.0.push((key.to_owned(), Cell::Number(value)));
    }
}

/// Writes `key<SEP>value`, one field per line.
pub fn write_pairs(out: &mut impl Write, pairs: &[Pair], separator: char) -> io::Result<()> {
    for (key, cell) in pairs {
        write_row(out, [key.as_str(), &cell.text()].iter(), separator)?;
    }
    Ok(())
}

/// Writes the same fields as one object, nested on the dots in the keys.
pub fn write_pairs_json(out: &mut impl Write, pairs: &[Pair]) -> io::Result<()> {
    let mut root = Branch::default();
    for (key, cell) in pairs {
        root.insert(&key.split('.').collect::<Vec<_>>(), cell.clone());
    }
    root.write(out, 0)?;
    out.write_all(b"\n")
}

/// An ordered tree, because insertion order is the order a person wants to
/// read these fields in.
#[derive(Default)]
struct Branch(Vec<(String, Node)>);

enum Node {
    Leaf(Cell),
    Branch(Branch),
}

impl Branch {
    fn insert(&mut self, path: &[&str], cell: Cell) {
        let (head, rest) = path.split_first().expect("a key is never empty");
        if rest.is_empty() {
            self.0.push(((*head).to_owned(), Node::Leaf(cell)));
            return;
        }
        let existing = self
            .0
            .iter()
            .position(|(name, node)| name == head && matches!(node, Node::Branch(_)));
        match existing {
            Some(at) => match &mut self.0[at].1 {
                Node::Branch(branch) => branch.insert(rest, cell),
                Node::Leaf(_) => unreachable!("filtered out just above"),
            },
            None => {
                let mut branch = Branch::default();
                branch.insert(rest, cell);
                self.0.push(((*head).to_owned(), Node::Branch(branch)));
            }
        }
    }

    fn write(&self, out: &mut impl Write, depth: usize) -> io::Result<()> {
        let indent = "  ".repeat(depth + 1);
        out.write_all(b"{\n")?;
        for (position, (name, node)) in self.0.iter().enumerate() {
            if position > 0 {
                out.write_all(b",\n")?;
            }
            write!(out, "{indent}")?;
            write_json_string(out, name)?;
            out.write_all(b": ")?;
            match node {
                Node::Leaf(cell) => cell.write_json_to(out)?,
                Node::Branch(branch) => branch.write(out, depth + 1)?,
            }
        }
        write!(out, "\n{}}}", "  ".repeat(depth))
    }
}

// -------------------------------------------------------------------- rows --

/// The row shapes [`Rows`] can write.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum RowShape {
    Separated(char),
    JsonArray,
    JsonLines,
    /// The last column only, unadorned, for `$(csq-reader pixel …)`.
    Bare,
}

/// Streams a table of rows in whichever shape was asked for.
///
/// JSON is the only shape that needs bracketing, so the writer remembers
/// whether it has opened the array yet and [`Rows::finish`] closes it.
pub struct Rows<'a> {
    shape: RowShape,
    columns: &'a [&'a str],
    header: bool,
    started: bool,
}

impl<'a> Rows<'a> {
    pub fn new(shape: RowShape, columns: &'a [&'a str], header: bool) -> Self {
        Self {
            shape,
            columns,
            header,
            started: false,
        }
    }

    pub fn write(&mut self, out: &mut impl Write, cells: &[Cell]) -> io::Result<()> {
        debug_assert_eq!(cells.len(), self.columns.len());
        match self.shape {
            RowShape::Separated(separator) => {
                if !self.started && self.header {
                    write_row(out, self.columns.iter(), separator)?;
                }
                write_row(out, cells.iter().map(Cell::text), separator)?;
            }
            RowShape::JsonArray | RowShape::JsonLines => {
                if self.shape == RowShape::JsonArray {
                    out.write_all(if self.started { b",\n  " } else { b"[\n  " })?;
                }
                out.write_all(b"{")?;
                for (position, (name, cell)) in self.columns.iter().zip(cells).enumerate() {
                    if position > 0 {
                        out.write_all(b",")?;
                    }
                    write_json_string(out, name)?;
                    out.write_all(b":")?;
                    cell.write_json_to(out)?;
                }
                out.write_all(b"}")?;
                if self.shape == RowShape::JsonLines {
                    out.write_all(b"\n")?;
                }
            }
            RowShape::Bare => {
                let last = cells.last().map(Cell::text).unwrap_or_default();
                writeln!(out, "{last}")?;
            }
        }
        self.started = true;
        Ok(())
    }

    pub fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.shape == RowShape::JsonArray {
            out.write_all(if self.started { b"\n]\n" } else { b"[]\n" })?;
        }
        Ok(())
    }
}

fn write_row<T: AsRef<str>>(
    out: &mut impl Write,
    cells: impl Iterator<Item = T>,
    separator: char,
) -> io::Result<()> {
    let mut buffer = [0u8; 4];
    let separator = separator.encode_utf8(&mut buffer).as_bytes().to_vec();
    for (position, value) in cells.enumerate() {
        if position > 0 {
            out.write_all(&separator)?;
        }
        let value = value.as_ref();
        if separator == b"," {
            out.write_all(quote_csv(value).as_bytes())?;
        } else {
            // A tab or a newline inside a value would invent a column or a
            // row, so flatten it to a space.
            out.write_all(value.replace(['\t', '\n', '\r'], " ").as_bytes())?;
        }
    }
    out.write_all(b"\n")
}

fn quote_csv(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(write: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> String {
        let mut buffer = Vec::new();
        write(&mut buffer).expect("writing to a Vec cannot fail");
        String::from_utf8(buffer).expect("output is UTF-8")
    }

    #[test]
    fn text_and_json_render_a_temperature_identically() {
        let cell = Cell::Fixed(33.295, 2);
        assert_eq!(cell.text(), "33.29");
        assert_eq!(rendered(|out| cell.write_json_to(out)), "33.29");
    }

    #[test]
    fn a_temperature_off_the_calibration_curve_is_nan_then_null() {
        let cell = Cell::Fixed(f32::NAN, 2);
        assert_eq!(cell.text(), "nan");
        assert_eq!(rendered(|out| cell.write_json_to(out)), "null");
    }

    #[test]
    fn json_keeps_the_order_the_fields_were_added_in() {
        let mut pairs = Pairs::new();
        pairs.text("camera.model", "FLIR T1020");
        pairs.int("file.frames", 22);
        pairs.text("camera.serial_number", "72502433");

        let json = rendered(|out| write_pairs_json(out, &pairs.into_vec()));
        assert_eq!(
            json,
            "{\n  \"camera\": {\n    \"model\": \"FLIR T1020\",\n    \
             \"serial_number\": \"72502433\"\n  },\n  \"file\": {\n    \"frames\": 22\n  }\n}\n"
        );
    }

    #[test]
    fn separators_inside_values_cannot_invent_a_column() {
        let mut pairs = Pairs::new();
        pairs.text("camera.model", "a,b");
        pairs.text("camera.software", "a\tb");

        assert_eq!(
            rendered(|out| write_pairs(out, &pairs.0.clone(), ',')),
            "camera.model,\"a,b\"\ncamera.software,a\tb\n"
        );
        assert_eq!(
            rendered(|out| write_pairs(out, &pairs.into_vec(), '\t')),
            "camera.model\ta,b\ncamera.software\ta b\n"
        );
    }

    #[test]
    fn a_json_table_is_one_array_even_when_empty() {
        let columns = ["frame", "celsius"];
        let mut rows = Rows::new(RowShape::JsonArray, &columns, false);
        assert_eq!(rendered(|out| rows.finish(out)), "[]\n");

        let mut rows = Rows::new(RowShape::JsonArray, &columns, false);
        let json = rendered(|out| {
            rows.write(out, &[Cell::Int(0), Cell::Fixed(18.0, 2)])?;
            rows.write(out, &[Cell::Int(1), Cell::Fixed(18.5, 2)])?;
            rows.finish(out)
        });
        assert_eq!(
            json,
            "[\n  {\"frame\":0,\"celsius\":18.00},\n  {\"frame\":1,\"celsius\":18.50}\n]\n"
        );
    }

    #[test]
    fn bare_rows_print_the_last_column_only() {
        let columns = ["frame", "time_s", "celsius"];
        let mut rows = Rows::new(RowShape::Bare, &columns, true);
        let text = rendered(|out| {
            rows.write(
                out,
                &[Cell::Int(3), Cell::Seconds(0.1), Cell::Fixed(18.0, 1)],
            )
        });
        assert_eq!(text, "18.0\n");
    }
}
