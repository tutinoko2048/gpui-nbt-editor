//! Loading NBT files via `nbtx`: decompression + endianness detection,
//! plus display helpers for `nbtx::Value`.

use std::io::{self, Read, Write};

use flate2::{
    read::{GzDecoder, ZlibDecoder},
    write::{GzEncoder, ZlibEncoder},
};
use nbtx::{BigEndian, LittleEndian, NetworkLittleEndian, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Gzip,
    Zlib,
}

impl Compression {
    pub fn label(self) -> &'static str {
        match self {
            Compression::None => "Uncompressed",
            Compression::Gzip => "GZip",
            Compression::Zlib => "Zlib",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Java,
    Bedrock,
    BedrockNetwork,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Java => "Java (BE)",
            Format::Bedrock => "Bedrock (LE)",
            Format::BedrockNetwork => "Bedrock (Network)",
        }
    }
}

pub struct NbtFile {
    pub root_name: String,
    pub root: Value,
    pub compression: Compression,
    pub format: Format,
    /// Bedrock level.dat storage version, when the file had its 8-byte header.
    pub header_version: Option<[u8; 4]>,
}

pub fn parse(bytes: &[u8]) -> io::Result<NbtFile> {
    let (data, compression) = decompress(bytes)?;

    // Bedrock level.dat starts with an 8-byte header (version, length).
    // The version can itself be 10, so check the length field instead of the first byte.
    let has_header = data.len() > 8
        && data[8] == 10
        && u32::from_le_bytes([data[4], data[5], data[6], data[7]]) as usize == data.len() - 8;
    let header_version = has_header.then(|| [data[0], data[1], data[2], data[3]]);
    let body = if has_header { &data[8..] } else { &data[..] };

    // Accept the first endianness that parses and consumes the whole input.
    let mut last_err = None;
    for format in [Format::Java, Format::Bedrock, Format::BedrockNetwork] {
        let mut reader = body;
        let result = match format {
            Format::Java => nbtx::from_bytes::<BigEndian, Value>(&mut reader),
            Format::Bedrock => nbtx::from_bytes::<LittleEndian, Value>(&mut reader),
            Format::BedrockNetwork => nbtx::from_bytes::<NetworkLittleEndian, Value>(&mut reader),
        };
        match result {
            Ok(root) if reader.is_empty() => {
                return Ok(NbtFile {
                    root_name: root_name(body, format),
                    root,
                    compression,
                    format,
                    header_version,
                });
            }
            Ok(_) => last_err = Some(format!("{}: trailing data", format.label())),
            Err(err) => last_err = Some(format!("{}: {err}", format.label())),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        last_err.unwrap_or_else(|| "not an NBT file".into()),
    ))
}

fn decompress(bytes: &[u8]) -> io::Result<(Vec<u8>, Compression)> {
    let mut out = Vec::new();
    match bytes {
        [0x1f, 0x8b, ..] => {
            GzDecoder::new(bytes).read_to_end(&mut out)?;
            Ok((out, Compression::Gzip))
        }
        [0x78, ..] => {
            ZlibDecoder::new(bytes).read_to_end(&mut out)?;
            Ok((out, Compression::Zlib))
        }
        _ => Ok((bytes.to_vec(), Compression::None)),
    }
}

/// nbtx discards the root tag's name, so read it from the header ourselves.
fn root_name(body: &[u8], format: Format) -> String {
    let len = match (format, body.get(1..3)) {
        (Format::Java, Some(b)) => u16::from_be_bytes([b[0], b[1]]) as usize,
        (Format::Bedrock, Some(b)) => u16::from_le_bytes([b[0], b[1]]) as usize,
        _ => return String::new(),
    };
    body.get(3..3 + len)
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}

pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Byte(_) => "Byte",
        Value::Short(_) => "Short",
        Value::Int(_) => "Int",
        Value::Long(_) => "Long",
        Value::Float(_) => "Float",
        Value::Double(_) => "Double",
        Value::ByteArray(_) => "Byte[]",
        Value::String(_) => "String",
        Value::List(_) => "List",
        Value::Compound(_) => "Compound",
        Value::IntArray(_) => "Int[]",
        Value::LongArray(_) => "Long[]",
    }
}

/// Short single-line value preview for the tree.
pub fn preview(v: &Value) -> String {
    match v {
        Value::Byte(n) => n.to_string(),
        Value::Short(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Long(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Double(n) => n.to_string(),
        Value::String(s) => format!("\"{}\"", s.chars().take(60).collect::<String>()),
        Value::List(l) => format!("{} entries", l.len()),
        Value::Compound(c) => format!("{} entries", c.len()),
        Value::ByteArray(a) => format!("{} bytes", a.len()),
        Value::IntArray(a) => format!("{} ints", a.len()),
        Value::LongArray(a) => format!("{} longs", a.len()),
    }
}

/// Full value for the detail panel.
pub fn full_value(v: &Value) -> String {
    fn join<T: ToString>(v: &[T]) -> String {
        let shown: Vec<String> = v.iter().take(2048).map(T::to_string).collect();
        let more = if v.len() > 2048 { format!(", … (+{})", v.len() - 2048) } else { String::new() };
        format!("[{}{}]", shown.join(", "), more)
    }
    match v {
        Value::String(s) => s.clone(),
        Value::ByteArray(a) => join(a),
        Value::IntArray(a) => join(a),
        Value::LongArray(a) => join(a),
        _ => preview(v),
    }
}

pub fn is_container(v: &Value) -> bool {
    matches!(v, Value::List(_) | Value::Compound(_))
}

pub fn count(v: &Value) -> usize {
    1 + match v {
        Value::List(l) => l.iter().map(count).sum(),
        Value::Compound(c) => c.values().map(count).sum(),
        _ => 0,
    }
}

/// Serializes back to bytes in the same format and compression it was read with.
pub fn encode(file: &NbtFile) -> io::Result<Vec<u8>> {
    let to_io = |e: nbtx::Error| io::Error::new(io::ErrorKind::InvalidData, e.to_string());
    let mut body = match file.format {
        Format::Java => nbtx::to_be_bytes(&file.root).map_err(to_io)?,
        Format::Bedrock => nbtx::to_le_bytes(&file.root).map_err(to_io)?,
        Format::BedrockNetwork => nbtx::to_net_bytes(&file.root).map_err(to_io)?,
    };

    // nbtx always writes an empty root name; put the original one back.
    let name = file.root_name.as_bytes();
    if !name.is_empty() && file.format != Format::BedrockNetwork {
        let len = u16::try_from(name.len()).map_err(|_| io::Error::other("root name too long"))?;
        let len = match file.format {
            Format::Java => len.to_be_bytes(),
            _ => len.to_le_bytes(),
        };
        body.splice(1..3, len.iter().chain(name).copied());
    }

    if let Some(version) = file.header_version {
        let len = (body.len() as u32).to_le_bytes();
        body.splice(0..0, version.into_iter().chain(len));
    }

    match file.compression {
        Compression::None => Ok(body),
        Compression::Gzip => {
            let mut enc = GzEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&body)?;
            enc.finish()
        }
        Compression::Zlib => {
            let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            enc.write_all(&body)?;
            enc.finish()
        }
    }
}

/// One step of a path from the root to a tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

pub fn get<'a>(root: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    path.iter().try_fold(root, |v, seg| match (v, seg) {
        (Value::Compound(c), Seg::Key(k)) => c.get(k),
        (Value::List(l), Seg::Index(i)) => l.get(*i),
        _ => None,
    })
}

pub fn get_mut<'a>(root: &'a mut Value, path: &[Seg]) -> Option<&'a mut Value> {
    path.iter().try_fold(root, |v, seg| match (v, seg) {
        (Value::Compound(c), Seg::Key(k)) => c.get_mut(k),
        (Value::List(l), Seg::Index(i)) => l.get_mut(*i),
        _ => None,
    })
}

/// Tag types that can be created from the UI.
pub const NEW_TYPES: [&str; 9] =
    ["Byte", "Short", "Int", "Long", "Float", "Double", "String", "List", "Compound"];

/// The text shown in the value editor for a scalar tag.
pub fn edit_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::List(_) | Value::Compound(_) => None,
        Value::ByteArray(_) | Value::IntArray(_) | Value::LongArray(_) => None,
        _ => Some(preview(v)),
    }
}

/// Parses user input into a tag of the given type. Empty input gives the
/// type's zero value.
pub fn parse_value(kind: &str, text: &str) -> Result<Value, String> {
    let t = text.trim();
    let num = if t.is_empty() { "0" } else { t };
    let err = |_| format!("\"{t}\" is not a valid {kind}");
    Ok(match kind {
        "Byte" => Value::Byte(num.parse().map_err(err)?),
        "Short" => Value::Short(num.parse().map_err(err)?),
        "Int" => Value::Int(num.parse().map_err(err)?),
        "Long" => Value::Long(num.parse().map_err(err)?),
        "Float" => Value::Float(num.parse().map_err(|_| format!("\"{t}\" is not a valid {kind}"))?),
        "Double" => Value::Double(num.parse().map_err(|_| format!("\"{t}\" is not a valid {kind}"))?),
        "String" => Value::String(text.to_string()),
        "List" => Value::List(Vec::new()),
        "Compound" => Value::Compound(Default::default()),
        _ => return Err(format!("{kind} cannot be edited")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, io::Write};

    fn sample() -> Value {
        Value::Compound(HashMap::from([
            ("name".to_string(), Value::String("Bananrama".into())),
            ("list".to_string(), Value::List(vec![Value::Int(1), Value::Int(2)])),
        ]))
    }

    #[test]
    fn parses_gzip_java() {
        let raw = nbtx::to_be_bytes(&sample()).unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&raw).unwrap();
        let file = parse(&gz.finish().unwrap()).unwrap();
        assert_eq!(file.compression, Compression::Gzip);
        assert_eq!(file.format, Format::Java);
        assert_eq!(count(&file.root), 5);
    }

    #[test]
    fn parses_bedrock_le() {
        let raw = nbtx::to_le_bytes(&sample()).unwrap();
        let file = parse(&raw).unwrap();
        assert_eq!(file.format, Format::Bedrock);
        assert_eq!(count(&file.root), 5);
    }

    #[test]
    fn edit_and_roundtrip_keeps_name_and_header() {
        let mut raw = nbtx::to_le_bytes(&sample()).unwrap();
        raw.splice(1..3, [2, 0, b'h', b'i']);
        let len = (raw.len() as u32).to_le_bytes();
        raw.splice(0..0, [10, 0, 0, 0].into_iter().chain(len));

        let mut file = parse(&raw).unwrap();
        assert_eq!(file.root_name, "hi");
        assert_eq!(file.header_version, Some([10, 0, 0, 0]));

        let path = [Seg::Key("list".into()), Seg::Index(1)];
        *get_mut(&mut file.root, &path).unwrap() = parse_value("Int", "42").unwrap();
        if let Value::Compound(c) = &mut file.root {
            c.insert("added".into(), parse_value("Short", "").unwrap());
        }

        let again = parse(&encode(&file).unwrap()).unwrap();
        assert_eq!(again.root_name, "hi");
        assert_eq!(again.header_version, Some([10, 0, 0, 0]));
        assert!(matches!(get(&again.root, &path), Some(Value::Int(42))));
        assert!(matches!(get(&again.root, &[Seg::Key("added".into())]), Some(Value::Short(0))));
    }

    #[test]
    fn rejects_bad_numbers() {
        assert!(parse_value("Byte", "300").is_err());
        assert!(parse_value("Int", "abc").is_err());
    }
}
