//! Bounded reader for the SCIP protobuf subset Graphr consumes.
//!
//! Only `Index.documents`, `Document.relative_path`/`occurrences`, and
//! `Occurrence.range`/`symbol`/`symbol_roles` are read; every other field is
//! skipped by wire type. Malformed input is rejected, never truncated.

use crate::evidence::evidence_path_is_safe;

pub const DOCUMENT_LIMIT: usize = 100_000;
pub const OCCURRENCE_LIMIT: usize = 4_000_000;
const SYMBOL_BYTE_LIMIT: usize = 4096;
const DEFINITION_ROLE: u64 = 0x1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    pub path: String,
    pub occurrences: Vec<Occurrence>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Occurrence {
    /// One-based line of the occurrence start.
    pub line: u32,
    pub symbol: String,
    pub definition: bool,
}

pub fn decode(bytes: &[u8]) -> Result<Vec<Document>, String> {
    let mut reader = Reader { bytes };
    let mut documents = Vec::new();
    let mut occurrences = 0_usize;
    while let Some((field, wire)) = reader.field()? {
        if field != 2 {
            reader.skip(wire)?;
            continue;
        }
        if documents.len() == DOCUMENT_LIMIT {
            return Err("SCIP index has too many documents".into());
        }
        let document = document(reader.bytes_field(wire)?, &mut occurrences)?;
        documents.push(document);
    }
    Ok(documents)
}

fn document(bytes: &[u8], total: &mut usize) -> Result<Document, String> {
    let mut reader = Reader { bytes };
    let mut path = None;
    let mut occurrences = Vec::new();
    while let Some((field, wire)) = reader.field()? {
        match field {
            1 => {
                let value = std::str::from_utf8(reader.bytes_field(wire)?)
                    .map_err(|_| "SCIP document path is not valid UTF-8".to_owned())?;
                if !evidence_path_is_safe(value) {
                    return Err("SCIP document path is unsafe".into());
                }
                path = Some(value.to_owned());
            }
            2 => {
                if let Some(occurrence) = occurrence(reader.bytes_field(wire)?)? {
                    if *total == OCCURRENCE_LIMIT {
                        return Err("SCIP index has too many occurrences".into());
                    }
                    *total += 1;
                    occurrences.push(occurrence);
                }
            }
            _ => reader.skip(wire)?,
        }
    }
    Ok(Document {
        path: path.ok_or_else(|| "SCIP document has no relative path".to_owned())?,
        occurrences,
    })
}

/// Returns `None` for local or empty symbols, which never cross documents.
fn occurrence(bytes: &[u8]) -> Result<Option<Occurrence>, String> {
    let mut reader = Reader { bytes };
    let mut range = Vec::with_capacity(4);
    let mut symbol = None;
    let mut roles = 0;
    while let Some((field, wire)) = reader.field()? {
        match (field, wire) {
            (1, Wire::Len) => {
                let mut packed = Reader {
                    bytes: reader.bytes_field(wire)?,
                };
                while !packed.bytes.is_empty() {
                    push_range(&mut range, packed.varint()?)?;
                }
            }
            (1, Wire::Varint) => push_range(&mut range, reader.varint()?)?,
            (1, _) => return Err("SCIP occurrence range has an invalid wire type".into()),
            (2, _) => {
                let value = reader.bytes_field(wire)?;
                if value.len() > SYMBOL_BYTE_LIMIT {
                    return Err("SCIP symbol exceeds its size limit".into());
                }
                symbol = Some(
                    std::str::from_utf8(value)
                        .map_err(|_| "SCIP symbol is not valid UTF-8".to_owned())?,
                );
            }
            (3, Wire::Varint) => roles = reader.varint()?,
            (3, _) => return Err("SCIP symbol roles have an invalid wire type".into()),
            _ => reader.skip(wire)?,
        }
    }
    if !matches!(range.len(), 3 | 4) {
        return Err("SCIP occurrence range is invalid".into());
    }
    let line = range[0]
        .checked_add(1)
        .ok_or_else(|| "SCIP occurrence line is out of range".to_owned())?;
    Ok(symbol
        .filter(|symbol| !symbol.is_empty() && !symbol.starts_with("local "))
        .map(|symbol| Occurrence {
            line,
            symbol: symbol.to_owned(),
            definition: roles & DEFINITION_ROLE != 0,
        }))
}

fn push_range(range: &mut Vec<u32>, value: u64) -> Result<(), String> {
    if range.len() == 4 {
        return Err("SCIP occurrence range is invalid".into());
    }
    let value = u32::try_from(value)
        .ok()
        .filter(|value| i32::try_from(*value).is_ok())
        .ok_or_else(|| "SCIP occurrence range is out of range".to_owned())?;
    range.push(value);
    Ok(())
}

/// Name of the last SCIP descriptor when it can name a Graphr node: a type
/// (`Name#`), term (`name.`), method (`name().`, `name(+1).`), or macro (`name!`).
pub fn symbol_name(symbol: &str) -> Option<&str> {
    if symbol.starts_with("local ") {
        return None;
    }
    let name = if let Some(rest) = symbol.strip_suffix(").") {
        &rest[..rest.rfind('(')?]
    } else {
        symbol
            .strip_suffix('.')
            .or_else(|| symbol.strip_suffix('#'))
            .or_else(|| symbol.strip_suffix('!'))?
    };
    let name = if let Some(escaped) = name.strip_suffix('`') {
        let start = escaped.rfind('`')?;
        let value = &escaped[start + 1..];
        // Doubled backticks escape a literal backtick; Graphr names never contain one.
        if start > 0 && escaped.as_bytes()[start - 1] == b'`' {
            return None;
        }
        value
    } else {
        let start = name
            .rfind(|character: char| {
                !(character.is_alphanumeric() || matches!(character, '_' | '+' | '-' | '$'))
            })
            .map_or(0, |index| index + 1);
        &name[start..]
    };
    (!name.is_empty()).then_some(name)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Wire {
    Varint,
    Fixed64,
    Len,
    Fixed32,
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn varint(&mut self) -> Result<u64, String> {
        let mut value = 0_u64;
        for index in 0..10 {
            let (&byte, rest) = self
                .bytes
                .split_first()
                .ok_or_else(|| "SCIP varint is truncated".to_owned())?;
            self.bytes = rest;
            if index == 9 && byte > 1 {
                return Err("SCIP varint overflows".into());
            }
            value |= u64::from(byte & 0x7f) << (7 * index);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err("SCIP varint overflows".into())
    }

    fn field(&mut self) -> Result<Option<(u32, Wire)>, String> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        let key = self.varint()?;
        let field = u32::try_from(key >> 3)
            .ok()
            .filter(|field| (1..1 << 29).contains(field))
            .ok_or_else(|| "SCIP field number is invalid".to_owned())?;
        let wire = match key & 7 {
            0 => Wire::Varint,
            1 => Wire::Fixed64,
            2 => Wire::Len,
            5 => Wire::Fixed32,
            _ => return Err("SCIP wire type is unsupported".into()),
        };
        Ok(Some((field, wire)))
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        if length > self.bytes.len() {
            return Err("SCIP field length exceeds its message".into());
        }
        let (value, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        Ok(value)
    }

    fn bytes_field(&mut self, wire: Wire) -> Result<&'a [u8], String> {
        if wire != Wire::Len {
            return Err("SCIP field has an invalid wire type".into());
        }
        let length = usize::try_from(self.varint()?)
            .map_err(|_| "SCIP field length exceeds its message".to_owned())?;
        self.take(length)
    }

    fn skip(&mut self, wire: Wire) -> Result<(), String> {
        match wire {
            Wire::Varint => self.varint().map(drop),
            Wire::Fixed64 => self.take(8).map(drop),
            Wire::Len => self.bytes_field(wire).map(drop),
            Wire::Fixed32 => self.take(4).map(drop),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut value: u64, out: &mut Vec<u8>) {
        while value >= 0x80 {
            out.push((value as u8) | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    fn len_field(field: u64, value: &[u8], out: &mut Vec<u8>) {
        varint(field << 3 | 2, out);
        varint(value.len() as u64, out);
        out.extend_from_slice(value);
    }

    fn occurrence_bytes(range: &[u64], symbol: &str, roles: u64) -> Vec<u8> {
        let mut packed = Vec::new();
        for value in range {
            varint(*value, &mut packed);
        }
        let mut out = Vec::new();
        len_field(1, &packed, &mut out);
        len_field(2, symbol.as_bytes(), &mut out);
        varint(3 << 3, &mut out);
        varint(roles, &mut out);
        out
    }

    #[test]
    fn decodes_documents_and_skips_unknown_fields() {
        let mut document = Vec::new();
        len_field(4, b"rust", &mut document);
        len_field(1, b"src/lib.rs", &mut document);
        len_field(
            2,
            &occurrence_bytes(&[1, 7, 17], "rust-analyzer cargo x 0.1.0 Alpha#run().", 1),
            &mut document,
        );
        let mut reference = occurrence_bytes(&[9, 6, 9, 16], "scip-x m p v Store#get().", 8);
        varint(5 << 3 | 5, &mut reference);
        reference.extend_from_slice(&[0; 4]);
        varint(6 << 3 | 1, &mut reference);
        reference.extend_from_slice(&[0; 8]);
        len_field(2, &reference, &mut document);
        len_field(
            2,
            &occurrence_bytes(&[2, 0, 3], "local 0", 1),
            &mut document,
        );
        varint(7 << 3, &mut document);
        varint(u64::MAX, &mut document);
        let mut index = Vec::new();
        len_field(1, b"metadata", &mut index);
        len_field(2, &document, &mut index);
        len_field(3, b"external", &mut index);

        assert_eq!(
            decode(&index).unwrap(),
            vec![Document {
                path: "src/lib.rs".into(),
                occurrences: vec![
                    Occurrence {
                        line: 2,
                        symbol: "rust-analyzer cargo x 0.1.0 Alpha#run().".into(),
                        definition: true,
                    },
                    Occurrence {
                        line: 10,
                        symbol: "scip-x m p v Store#get().".into(),
                        definition: false,
                    },
                ],
            }]
        );
    }

    #[test]
    fn decodes_committed_rust_analyzer_fixture() {
        let documents = decode(include_bytes!("../tests/fixtures/scip/index.scip")).unwrap();
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].path, "src/lib.rs");
        let reference = documents[0]
            .occurrences
            .iter()
            .find(|occurrence| {
                !occurrence.definition && occurrence.symbol.ends_with("alpha_only().")
            })
            .unwrap();
        assert_eq!(reference.line, 10);
        assert_eq!(symbol_name(&reference.symbol), Some("alpha_only"));
    }

    #[test]
    fn rejects_truncated_varints() {
        assert_eq!(
            decode(&[0x12, 0x80]).unwrap_err(),
            "SCIP varint is truncated"
        );
        assert_eq!(decode(&[0x80]).unwrap_err(), "SCIP varint is truncated");
        let mut overflow = vec![0x08];
        overflow.extend_from_slice(&[0xff; 9]);
        overflow.push(0x02);
        assert_eq!(decode(&overflow).unwrap_err(), "SCIP varint overflows");
    }

    #[test]
    fn rejects_oversized_lengths_and_malformed_messages() {
        let mut index = Vec::new();
        varint(2 << 3 | 2, &mut index);
        varint(u64::from(u32::MAX), &mut index);
        index.extend_from_slice(b"short");
        assert_eq!(
            decode(&index).unwrap_err(),
            "SCIP field length exceeds its message"
        );
        assert_eq!(
            decode(&[0x13]).unwrap_err(),
            "SCIP wire type is unsupported"
        );
        assert_eq!(decode(&[0x00]).unwrap_err(), "SCIP field number is invalid");

        let mut bad_range = Vec::new();
        len_field(1, b"src/lib.rs", &mut bad_range);
        len_field(
            2,
            &occurrence_bytes(&[1, 2], "s p m v A#", 0),
            &mut bad_range,
        );
        let mut index = Vec::new();
        len_field(2, &bad_range, &mut index);
        assert_eq!(
            decode(&index).unwrap_err(),
            "SCIP occurrence range is invalid"
        );

        let mut negative = Vec::new();
        len_field(1, b"src/lib.rs", &mut negative);
        len_field(
            2,
            &occurrence_bytes(&[u64::MAX, 0, 1], "s p m v A#", 0),
            &mut negative,
        );
        let mut index = Vec::new();
        len_field(2, &negative, &mut index);
        assert_eq!(
            decode(&index).unwrap_err(),
            "SCIP occurrence range is out of range"
        );

        let mut unsafe_path = Vec::new();
        len_field(1, b"../outside.rs", &mut unsafe_path);
        let mut index = Vec::new();
        len_field(2, &unsafe_path, &mut index);
        assert_eq!(decode(&index).unwrap_err(), "SCIP document path is unsafe");
    }

    #[test]
    fn symbol_name_reads_last_descriptor() {
        for (symbol, name) in [
            ("rust-analyzer cargo x 0.1.0 Store#get().", Some("get")),
            ("scip-python python pkg 1.0 `pkg.mod`/Cls#m(+1).", Some("m")),
            ("scip-typescript npm p 1.0 src/`a.ts`/Store#", Some("Store")),
            ("scip-typescript npm p 1.0 src/`a.ts`/value.", Some("value")),
            (
                "rust-analyzer cargo x 0.1.0 make_thing!",
                Some("make_thing"),
            ),
            ("cxx . . $ `operator()`(abc).", Some("operator()")),
            ("rust-analyzer cargo x 0.1.0 module/", None),
            ("rust-analyzer cargo x 0.1.0 f().(param)", None),
            ("local 4", None),
        ] {
            assert_eq!(symbol_name(symbol), name, "{symbol}");
        }
    }
}
