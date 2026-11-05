//! Loading and saving NBT files: decompression + endianness detection,
//! plus display helpers for `nbtx::Value`.
//!
//! Only `nbtx::Value` is used from nbtx. Its serde reader/writer is not
//! round-trip safe (empty lists lose their header, and Byte/Int/Long arrays
//! turn into lists), so the binary codec lives here.

use std::{
    collections::HashMap,
    io::{self, Read, Write},
};

use flate2::{
    read::{GzDecoder, ZlibDecoder},
    write::{GzEncoder, ZlibEncoder},
};
use nbtx::Value;

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
        let mut r = Reader { buf: body, format };
        match r.root() {
            Ok((root_name, root)) if r.buf.is_empty() => {
                return Ok(NbtFile { root_name, root, compression, format, header_version });
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

const MAX_DEPTH: usize = 512;

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Binary NBT reader. Java is big-endian; Bedrock is little-endian; Bedrock
/// network is little-endian with zigzag varints for Int/Long/lengths and an
/// unsigned varint for string lengths.
struct Reader<'a> {
    buf: &'a [u8],
    format: Format,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let (head, rest) = self.buf.split_first_chunk::<N>().ok_or_else(|| bad("unexpected end of data"))?;
        self.buf = rest;
        Ok(*head)
    }

    fn bytes(&mut self, n: usize) -> io::Result<&[u8]> {
        if n > self.buf.len() {
            return Err(bad("unexpected end of data"));
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take::<1>()?[0])
    }

    fn varint(&mut self, max_bits: u32) -> io::Result<u64> {
        let mut out = 0u64;
        for shift in (0..max_bits).step_by(7) {
            let b = self.u8()?;
            out |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
        }
        Err(bad("varint too long"))
    }

    fn i16(&mut self) -> io::Result<i16> {
        let b = self.take()?;
        Ok(if self.format == Format::Java { i16::from_be_bytes(b) } else { i16::from_le_bytes(b) })
    }

    fn i32(&mut self) -> io::Result<i32> {
        Ok(match self.format {
            Format::Java => i32::from_be_bytes(self.take()?),
            Format::Bedrock => i32::from_le_bytes(self.take()?),
            Format::BedrockNetwork => {
                let n = self.varint(32)? as u32;
                ((n >> 1) as i32) ^ -((n & 1) as i32)
            }
        })
    }

    fn i64(&mut self) -> io::Result<i64> {
        Ok(match self.format {
            Format::Java => i64::from_be_bytes(self.take()?),
            Format::Bedrock => i64::from_le_bytes(self.take()?),
            Format::BedrockNetwork => {
                let n = self.varint(64)?;
                ((n >> 1) as i64) ^ -((n & 1) as i64)
            }
        })
    }

    fn len(&mut self) -> io::Result<usize> {
        usize::try_from(self.i32()?).map_err(|_| bad("negative length"))
    }

    fn string(&mut self) -> io::Result<String> {
        let len = match self.format {
            Format::Java => u16::from_be_bytes(self.take()?) as usize,
            Format::Bedrock => u16::from_le_bytes(self.take()?) as usize,
            Format::BedrockNetwork => self.varint(32)? as usize,
        };
        // Java uses modified UTF-8; lossy keeps plain text intact.
        Ok(String::from_utf8_lossy(self.bytes(len)?).into_owned())
    }

    fn root(&mut self) -> io::Result<(String, Value)> {
        let ty = self.u8()?;
        if ty != 10 && ty != 9 {
            return Err(bad(format!("root tag must be Compound or List, got type {ty}")));
        }
        let name = self.string()?;
        Ok((name, self.payload(ty, 0)?))
    }

    fn payload(&mut self, ty: u8, depth: usize) -> io::Result<Value> {
        if depth > MAX_DEPTH {
            return Err(bad("nesting too deep"));
        }
        let le = self.format != Format::Java;
        Ok(match ty {
            1 => Value::Byte(self.u8()? as i8),
            2 => Value::Short(self.i16()?),
            3 => Value::Int(self.i32()?),
            4 => Value::Long(self.i64()?),
            5 => {
                let b = self.take()?;
                Value::Float(if le { f32::from_le_bytes(b) } else { f32::from_be_bytes(b) })
            }
            6 => {
                let b = self.take()?;
                Value::Double(if le { f64::from_le_bytes(b) } else { f64::from_be_bytes(b) })
            }
            7 => {
                let n = self.len()?;
                Value::ByteArray(self.bytes(n)?.to_vec())
            }
            8 => Value::String(self.string()?),
            9 => {
                let elem = self.u8()?;
                let n = self.len()?;
                if elem == 0 && n > 0 {
                    return Err(bad("non-empty list of End tags"));
                }
                let mut out = Vec::with_capacity(n.min(self.buf.len()));
                for _ in 0..n {
                    out.push(self.payload(elem, depth + 1)?);
                }
                Value::List(out)
            }
            10 => {
                let mut out = HashMap::new();
                loop {
                    let ty = self.u8()?;
                    if ty == 0 {
                        break;
                    }
                    let key = self.string()?;
                    out.insert(key, self.payload(ty, depth + 1)?);
                }
                Value::Compound(out)
            }
            11 => {
                let n = self.len()?;
                let mut out = Vec::with_capacity(n.min(self.buf.len()));
                for _ in 0..n {
                    out.push(self.i32()?);
                }
                Value::IntArray(out)
            }
            12 => {
                let n = self.len()?;
                let mut out = Vec::with_capacity(n.min(self.buf.len()));
                for _ in 0..n {
                    out.push(self.i64()?);
                }
                Value::LongArray(out)
            }
            _ => return Err(bad(format!("unknown tag type {ty}"))),
        })
    }
}

fn tag_id(v: &Value) -> u8 {
    match v {
        Value::Byte(_) => 1,
        Value::Short(_) => 2,
        Value::Int(_) => 3,
        Value::Long(_) => 4,
        Value::Float(_) => 5,
        Value::Double(_) => 6,
        Value::ByteArray(_) => 7,
        Value::String(_) => 8,
        Value::List(_) => 9,
        Value::Compound(_) => 10,
        Value::IntArray(_) => 11,
        Value::LongArray(_) => 12,
    }
}

/// Binary NBT writer, the mirror of [`Reader`].
struct Writer {
    out: Vec<u8>,
    format: Format,
}

impl Writer {
    fn varint(&mut self, mut n: u64) {
        while n >= 0x80 {
            self.out.push((n as u8) | 0x80);
            n >>= 7;
        }
        self.out.push(n as u8);
    }

    fn i16(&mut self, n: i16) {
        let b = if self.format == Format::Java { n.to_be_bytes() } else { n.to_le_bytes() };
        self.out.extend(b);
    }

    fn i32(&mut self, n: i32) {
        match self.format {
            Format::Java => self.out.extend(n.to_be_bytes()),
            Format::Bedrock => self.out.extend(n.to_le_bytes()),
            Format::BedrockNetwork => self.varint(u64::from(((n << 1) ^ (n >> 31)) as u32)),
        }
    }

    fn i64(&mut self, n: i64) {
        match self.format {
            Format::Java => self.out.extend(n.to_be_bytes()),
            Format::Bedrock => self.out.extend(n.to_le_bytes()),
            Format::BedrockNetwork => self.varint(((n << 1) ^ (n >> 63)) as u64),
        }
    }

    fn len(&mut self, n: usize) -> io::Result<()> {
        self.i32(i32::try_from(n).map_err(|_| bad("too many entries"))?);
        Ok(())
    }

    fn string(&mut self, s: &str) -> io::Result<()> {
        match self.format {
            Format::BedrockNetwork => self.varint(s.len() as u64),
            _ => {
                let n = u16::try_from(s.len()).map_err(|_| bad("string longer than 65535 bytes"))?;
                self.i16(n as i16);
            }
        }
        self.out.extend_from_slice(s.as_bytes());
        Ok(())
    }

    fn payload(&mut self, v: &Value) -> io::Result<()> {
        let le = self.format != Format::Java;
        match v {
            Value::Byte(n) => self.out.push(*n as u8),
            Value::Short(n) => self.i16(*n),
            Value::Int(n) => self.i32(*n),
            Value::Long(n) => self.i64(*n),
            Value::Float(n) => self.out.extend(if le { n.to_le_bytes() } else { n.to_be_bytes() }),
            Value::Double(n) => self.out.extend(if le { n.to_le_bytes() } else { n.to_be_bytes() }),
            Value::ByteArray(a) => {
                self.len(a.len())?;
                self.out.extend_from_slice(a);
            }
            Value::String(s) => self.string(s)?,
            Value::List(items) => {
                // An empty list is written with element type End, as Minecraft does.
                let elem = items.first().map_or(0, tag_id);
                if let Some(other) = items.iter().find(|i| tag_id(i) != elem) {
                    return Err(bad(format!(
                        "list mixes {} and {} tags",
                        type_name(&items[0]),
                        type_name(other)
                    )));
                }
                self.out.push(elem);
                self.len(items.len())?;
                for item in items {
                    self.payload(item)?;
                }
            }
            Value::Compound(map) => {
                for (key, value) in map {
                    self.out.push(tag_id(value));
                    self.string(key)?;
                    self.payload(value)?;
                }
                self.out.push(0);
            }
            Value::IntArray(a) => {
                self.len(a.len())?;
                a.iter().for_each(|n| self.i32(*n));
            }
            Value::LongArray(a) => {
                self.len(a.len())?;
                a.iter().for_each(|n| self.i64(*n));
            }
        }
        Ok(())
    }
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
    let mut w = Writer { out: Vec::new(), format: file.format };
    w.out.push(tag_id(&file.root));
    w.string(&file.root_name)?;
    w.payload(&file.root)?;
    let mut body = w.out;

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
    fn roundtrips_empty_lists_and_arrays_in_every_format() {
        let root = Value::Compound(HashMap::from([
            ("entities".to_string(), Value::List(Vec::new())),
            ("bytes".to_string(), Value::ByteArray(vec![1, 255])),
            ("ints".to_string(), Value::IntArray(vec![-1, 0, i32::MAX])),
            ("longs".to_string(), Value::LongArray(vec![i64::MIN, 7])),
            ("pos".to_string(), Value::List(vec![Value::Int(1036), Value::Int(-1002)])),
            ("f".to_string(), Value::Float(1.5)),
        ]));
        for format in [Format::Java, Format::Bedrock, Format::BedrockNetwork] {
            let file = NbtFile {
                root_name: "root".into(),
                root: root.clone(),
                compression: Compression::None,
                format,
                header_version: None,
            };
            let bytes = encode(&file).unwrap();
            let again = parse(&bytes).unwrap();
            assert_eq!(again.format, format);
            assert_eq!(again.root_name, "root");
            assert_eq!(again.root, root, "{format:?}");
        }
    }

    #[test]
    fn empty_list_keeps_its_header() {
        // `entities` from a Bedrock .mcstructure: TAG_List, End, length 0.
        let raw = [10, 0, 0, 9, 8, 0, b'e', b'n', b't', b'i', b't', b'i', b'e', b's', 0, 0, 0, 0, 0, 0];
        let file = parse(&raw).unwrap();
        assert_eq!(file.format, Format::Bedrock);
        assert_eq!(encode(&file).unwrap(), raw);
    }

    /// Round-trips a real file: `NBT_ROUNDTRIP=path cargo test real_file -- --ignored`.
    #[test]
    #[ignore]
    fn real_file_roundtrip() {
        let path = std::env::var("NBT_ROUNDTRIP").unwrap();
        let file = parse(&std::fs::read(&path).unwrap()).unwrap();
        let bytes = encode(&file).unwrap();
        let again = parse(&bytes).unwrap();
        assert_eq!(again.format, file.format);
        assert_eq!(again.root, file.root);
        std::fs::write(format!("{path}.roundtrip"), bytes).unwrap();
    }

    #[test]
    fn rejects_bad_numbers() {
        assert!(parse_value("Byte", "300").is_err());
        assert!(parse_value("Int", "abc").is_err());
    }
}
