//! CSV as JSON and back: RFC 4180 through the `csv` crate, JSON through jaq's own value type
//! (so key order is kept and strings are escaped by the same code that serializes query
//! results).

use jaq_core::ValT as _;
use jaq_json::Val;
use jaq_std::ValT as _;

/// The largest CSV this crate converts: 16 MiB.
pub const MAX_CSV_BYTES: usize = 16 * 1024 * 1024;
/// The most records (rows, not counting the header): 1,000,000.
pub const MAX_CSV_ROWS: usize = 1_000_000;
/// The most fields in one record: 1,024.
pub const MAX_CSV_COLUMNS: usize = 1024;
/// The largest single field: 1 MiB.
pub const MAX_CSV_FIELD_BYTES: usize = 1024 * 1024;

/// Why a conversion was refused: which argument, and what about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CsvRefusal {
    /// What was wrong, for a person; names the bound when a bound was reached.
    pub detail: String,
}

fn refuse<T>(detail: impl Into<String>) -> Result<T, CsvRefusal> {
    Err(CsvRefusal {
        detail: detail.into(),
    })
}

/// Read RFC 4180 CSV as JSON.
///
/// With `header`, the first record names the columns and the answer is an array of objects
/// keyed by them, in column order. Without it, the answer is an array of arrays. **Every value
/// is a string**: CSV has no types, and guessing (is `007` a number? is `true`?) would change
/// data silently, so a query that wants a number says `tonumber`.
///
/// Refused rather than guessed: a record with a different number of fields from the first, a
/// header that names a column twice (an object cannot hold both), bytes that are not UTF-8, and
/// anything past the bounds above, each naming its bound. A leading UTF-8 byte-order mark is
/// skipped. An empty input is `[]`.
///
/// ```
/// let csv = b"name,age\r\nAda,36\r\n\"Hopper, Grace\",85\r\n";
/// assert_eq!(
///     ikigai_json::csv_to_json(csv, true).unwrap(),
///     r#"[{"name":"Ada","age":"36"},{"name":"Hopper, Grace","age":"85"}]"#
/// );
/// assert_eq!(
///     ikigai_json::csv_to_json(csv, false).unwrap(),
///     r#"[["name","age"],["Ada","36"],["Hopper, Grace","85"]]"#
/// );
/// ```
pub fn csv_to_json(bytes: &[u8], header: bool) -> Result<String, CsvRefusal> {
    if bytes.len() > MAX_CSV_BYTES {
        return refuse(format!(
            "the CSV is {} bytes, more than MAX_CSV_BYTES ({MAX_CSV_BYTES})",
            bytes.len()
        ));
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(bytes);
    let mut names: Option<Vec<Val>> = None;
    let mut width: Option<usize> = None;
    let mut rows: Vec<Val> = Vec::new();
    let mut record = csv::StringRecord::new();
    let mut line = 0usize;
    loop {
        match reader.read_record(&mut record) {
            Ok(false) => break,
            Ok(true) => {}
            Err(e) => return refuse(describe(e)),
        }
        line += 1;
        if record.len() > MAX_CSV_COLUMNS {
            return refuse(format!(
                "record {line} has {} fields, more than MAX_CSV_COLUMNS ({MAX_CSV_COLUMNS})",
                record.len()
            ));
        }
        if let Some(field) = record.iter().find(|f| f.len() > MAX_CSV_FIELD_BYTES) {
            return refuse(format!(
                "record {line} has a field of {} bytes, more than MAX_CSV_FIELD_BYTES \
                 ({MAX_CSV_FIELD_BYTES})",
                field.len()
            ));
        }
        match width {
            None => width = Some(record.len()),
            Some(w) if w != record.len() => {
                return refuse(format!(
                    "record {line} has {} fields where the first has {w}; RFC 4180 records \
                     all have the same number",
                    record.len()
                ))
            }
            Some(_) => {}
        }
        let fields = record.iter().map(|f| Val::utf8_str(f.to_owned()));
        if header && names.is_none() {
            let row: Vec<Val> = fields.collect();
            for (i, name) in row.iter().enumerate() {
                if row[..i].contains(name) {
                    return refuse(format!(
                        "the header names the column {name} twice; an object cannot hold both \
                         (use header=false for arrays)"
                    ));
                }
            }
            names = Some(row);
            continue;
        }
        if rows.len() == MAX_CSV_ROWS {
            return refuse(format!(
                "the CSV has more than MAX_CSV_ROWS ({MAX_CSV_ROWS}) records"
            ));
        }
        rows.push(match &names {
            Some(names) => Val::from_map(names.iter().cloned().zip(fields))
                .expect("string keys always make an object"),
            None => fields.collect(),
        });
    }
    Ok(Val::from_iter(rows).to_string())
}

fn describe(e: csv::Error) -> String {
    match e.kind() {
        csv::ErrorKind::Utf8 { pos, .. } => format!(
            "record {} is not UTF-8",
            pos.as_ref().map_or(0, |p| p.record() + 1)
        ),
        _ => format!("not RFC 4180 CSV: {e}"),
    }
}

/// Write JSON as RFC 4180 CSV (CRLF line ends, fields quoted only when they must be).
///
/// The JSON is an array of FLAT objects, or of arrays. Objects: the header is the union of
/// their keys in the order first seen, and a missing key is an empty field. Arrays: no header,
/// one record each. A string is written as its text, a number, boolean as its JSON spelling,
/// null as an empty field, and a nested array or object is refused (CSV has nowhere to put it).
/// The reverse of [`csv_to_json`] for its own output; types do not survive (`36` and `"36"`
/// write the same), which is why the transreptor declares itself a projection.
///
/// ```
/// let json = br#"[{"name":"Ada","age":36},{"name":"Hopper, Grace","note":null}]"#;
/// assert_eq!(
///     ikigai_json::json_to_csv(json).unwrap(),
///     "name,age,note\r\nAda,36,\r\n\"Hopper, Grace\",,\r\n"
/// );
/// ```
pub fn json_to_csv(bytes: &[u8]) -> Result<String, CsvRefusal> {
    if bytes.len() > MAX_CSV_BYTES {
        return refuse(format!(
            "the JSON is {} bytes, more than MAX_CSV_BYTES ({MAX_CSV_BYTES})",
            bytes.len()
        ));
    }
    // An array of flat records nests two deep; the scan stops at three.
    if crate::scan::nesting(bytes, 2) > 2 {
        return refuse(
            "the JSON nests deeper than an array of flat records; CSV has nowhere to put a \
             nested value",
        );
    }
    let value = jaq_json::read::parse_single(bytes).map_err(|e| CsvRefusal {
        detail: format!("not a JSON document: {e}"),
    })?;
    let Val::Arr(rows) = value else {
        return refuse("the JSON is not an array of records");
    };
    if rows.len() > MAX_CSV_ROWS {
        return refuse(format!(
            "the JSON has more than MAX_CSV_ROWS ({MAX_CSV_ROWS}) records"
        ));
    }
    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::CRLF)
        .flexible(true)
        .from_writer(Vec::new());
    let objects = rows.iter().all(|r| matches!(r, Val::Obj(_)));
    if objects && !rows.is_empty() {
        let mut header: Vec<Val> = Vec::new();
        for row in rows.iter() {
            if let Val::Obj(o) = row {
                for k in o.keys() {
                    if !header.contains(k) {
                        header.push(k.clone());
                    }
                }
            }
        }
        if header.len() > MAX_CSV_COLUMNS {
            return refuse(format!(
                "the records have {} distinct keys, more than MAX_CSV_COLUMNS ({MAX_CSV_COLUMNS})",
                header.len()
            ));
        }
        let names: Vec<String> = header.iter().map(field).collect::<Result<_, _>>()?;
        writer.write_record(&names).map_err(io)?;
        for row in rows.iter() {
            let Val::Obj(o) = row else { unreachable!() };
            let fields: Vec<String> = header
                .iter()
                .map(|k| o.get(k).map_or(Ok(String::new()), field))
                .collect::<Result<_, _>>()?;
            writer.write_record(&fields).map_err(io)?;
        }
    } else {
        for (i, row) in rows.iter().enumerate() {
            let Val::Arr(a) = row else {
                return refuse(format!(
                    "record {} is neither an object nor an array; the records must all be one \
                     or the other",
                    i + 1
                ));
            };
            if a.len() > MAX_CSV_COLUMNS {
                return refuse(format!(
                    "record {} has {} fields, more than MAX_CSV_COLUMNS ({MAX_CSV_COLUMNS})",
                    i + 1,
                    a.len()
                ));
            }
            let fields: Vec<String> = a.iter().map(field).collect::<Result<_, _>>()?;
            writer.write_record(&fields).map_err(io)?;
        }
    }
    let bytes = writer.into_inner().map_err(|e| CsvRefusal {
        detail: format!("writing CSV: {e}"),
    })?;
    Ok(String::from_utf8(bytes).expect("CSV written from UTF-8 fields is UTF-8"))
}

fn io(e: csv::Error) -> CsvRefusal {
    CsvRefusal {
        detail: format!("writing CSV: {e}"),
    }
}

/// One CSV field from one JSON value.
fn field(v: &Val) -> Result<String, CsvRefusal> {
    match v {
        Val::Null => Ok(String::new()),
        Val::TStr(_) | Val::BStr(_) => {
            Ok(String::from_utf8_lossy(v.as_bytes().expect("a string has bytes")).into_owned())
        }
        Val::Arr(_) | Val::Obj(_) => refuse(format!(
            "a record holds the nested value {}; CSV has nowhere to put it",
            short(v)
        )),
        _ => Ok(v.to_string()),
    }
}

fn short(v: &Val) -> String {
    let s = v.to_string();
    if s.len() > 40 {
        format!("{}…", s.chars().take(40).collect::<String>())
    } else {
        s
    }
}
