//! `hexedit` -- a command line front end for the engine.
//!
//! Every subcommand is a thin wrapper over `hexcore`, and each one is something the
//! GUI will need too. That is deliberate: a hex editor whose headless operations are
//! not the same code as its interactive ones ends up with two behaviours, and the
//! interactive one is the untested one.
//!
//! ```text
//! hexedit dump <file> [--width N] [--offset N] [--len N]
//! hexedit hex <file> --offset N [--len N]
//! hexedit find <file> <pattern> [--limit N]
//! hexedit poke <file> <offset> <hexbytes>
//! hexedit patch <file> <offset> <hexbytes> [--expect hex]
//! hexedit structure <file> <offset> --layout <file>
//! hexedit scan <file> --layout <file> [--limit N]
//! ```
//!
//! `patch --expect` is the interesting one: it writes only if the bytes currently
//! there match what you say they are, which is what makes a scripted patch safe to
//! re-run against a file that may since have changed.

use std::process::ExitCode;

use hexcore::{parse_layout, Document, Pattern};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        usage();
        return ExitCode::from(2);
    }
    let result = match args[0].as_str() {
        "dump" => cmd_dump(&args[1..]),
        "hex" => cmd_hex(&args[1..]),
        "find" => cmd_find(&args[1..]),
        "poke" => cmd_poke(&args[1..]),
        "patch" => cmd_patch(&args[1..]),
        "structure" => cmd_structure(&args[1..]),
        "scan" => cmd_scan(&args[1..]),
        "-h" | "--help" | "help" => {
            usage();
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command `{other}`")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hexedit: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!(
        "hexedit -- the Rak hex editor's engine, from the command line

  dump <file> [--width N] [--offset N] [--len N]   rows of offset / hex / text
  hex <file> --offset N [--len N]                   one span as hex
  find <file> <pattern> [--limit N]                 hex or wildcard search
  poke <file> <offset> <hexbytes>                   edit in memory, print, do not save
  patch <file> <offset> <hexbytes> [--expect hex]  edit and save, optionally guarded
  structure <file> <offset> --layout <file>        decode a layout at an offset
  scan <file> --layout <file> [--limit N]           find a layout's magic values

A pattern is hex digits, with or without spaces, and may use `?` for a whole byte or
one nibble: 'de ad be ef', '7f ?? 4c 46', '7f?4'. One line, one field; `?` and `.`
are the same wildcard, and a lone nibble is refused by name.

A layout file is one field per line, `name=type`:

  magic=7f454c46     u8  u16le  u16be  u32le  u32be  u64le  u64be
  i8 i16le i16be i32le i32be i64le i64be          signed
  bits:4              n bits from the next byte        bytes:4   n raw bytes
  hdr=magic:7f45 plus any of the above as a nested group

  # lines and text after # are comments"
    );
}

/// `--flag value` lookup that also accepts `--flag=value`, because both spellings
/// turn up in muscle memory and neither is worth an error message.
struct Flags {
    map: Vec<(String, String)>,
    positional: Vec<String>,
}

impl Flags {
    fn parse(args: &[String], valued: &[&str]) -> Result<Flags, String> {
        let mut map = Vec::new();
        let mut positional = Vec::new();
        let mut i = 0;
        while i < args.len() {
            let a = &args[i];
            if let Some(name) = a.strip_prefix("--") {
                if let Some((k, v)) = name.split_once('=') {
                    map.push((k.to_string(), v.to_string()));
                    i += 1;
                    continue;
                }
                if valued.contains(&name) {
                    let v = args
                        .get(i + 1)
                        .ok_or_else(|| format!("--{name} needs a value"))?;
                    map.push((name.to_string(), v.clone()));
                    i += 2;
                    continue;
                }
                return Err(format!("unknown flag --{name}"));
            }
            positional.push(a.clone());
            i += 1;
        }
        Ok(Flags { map, positional })
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.map
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn num(&self, name: &str, default: u64) -> Result<u64, String> {
        match self.get(name) {
            None => Ok(default),
            Some(v) => parse_number(v).ok_or_else(|| format!("--{name}: `{v}` is not a number")),
        }
    }
}

/// Accept `0x`-prefixed hex, plain hex, and decimal.
///
/// The same leniency as the pattern parser, and for the same reason: a user reading
/// an offset out of a hex dump will type it in hex.
fn parse_number(s: &str) -> Option<u64> {
    let t = s.trim().replace('_', "");
    if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).ok()
    } else if t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty() {
        t.parse::<u64>().ok()
    } else {
        // Bare hex with letters in it, e.g. an offset copied as `deadbeef`.
        u64::from_str_radix(&t, 16).ok()
    }
}

fn open(path: &str, read_only: bool) -> Result<Document, String> {
    Document::open(path, read_only).map_err(|e| e.to_string())
}

fn cmd_dump(args: &[String]) -> Result<(), String> {
    let f = Flags::parse(args, &["width", "offset", "len"])?;
    let path = f.positional.first().ok_or("dump needs a file")?;
    let width = f.num("width", hexcore::DEFAULT_WIDTH as u64)? as usize;
    let offset = f.num("offset", 0)?;
    let doc = open(path, true)?;
    let end = match f.get("len") {
        Some(_) => (offset + f.num("len", 0)?).min(doc.len()),
        None => doc.len(),
    };
    if offset > doc.len() {
        return Err(format!("offset {offset:#x} is past the end of a {:#x}-byte file", doc.len()));
    }
    let bytes = doc.slice(offset, end).map_err(|e| e.to_string())?;
    print!(
        "{}",
        hexcore::format_span(&bytes, offset, width)
    );
    Ok(())
}

fn cmd_hex(args: &[String]) -> Result<(), String> {
    let f = Flags::parse(args, &["offset", "len"])?;
    let path = f.positional.first().ok_or("hex needs a file")?;
    let offset = f.num("offset", 0)?;
    let doc = open(path, true)?;
    let len = f.num("len", 256)?;
    let end = (offset + len).min(doc.len());
    let bytes = doc
        .slice(offset.min(doc.len()), end)
        .map_err(|e| e.to_string())?;
    for b in &bytes {
        print!("{b:02x}");
    }
    println!();
    Ok(())
}

fn cmd_find(args: &[String]) -> Result<(), String> {
    let f = Flags::parse(args, &["limit"])?;
    let path = f.positional.first().ok_or("find needs a file")?;
    let pat_text = f.positional.get(1).ok_or("find needs a pattern")?;
    let pattern = Pattern::parse(pat_text).map_err(|e| e.to_string())?;
    let limit = f.num("limit", 256)? as usize;
    let doc = open(path, true)?;
    for hit in pattern.find_in(&doc, limit).map_err(|e| e.to_string())? {
        // Show the bytes so a hit can be judged without opening the file.
        let ctx = doc
            .slice(hit, (hit + pattern.len() as u64).min(doc.len()))
            .map_err(|e| e.to_string())?;
        let hex: String = ctx.iter().map(|b| format!("{b:02X}")).collect();
        println!("{hit:#010x}  {hex}");
    }
    Ok(())
}

fn cmd_poke(args: &[String]) -> Result<(), String> {
    let f = Flags::parse(args, &[])?;
    let path = f.positional.first().ok_or("poke needs a file")?;
    let offset = parse_number(f.positional.get(1).ok_or("poke needs an offset")?)
        .ok_or("poke: that offset is not a number")?;
    let bytes_text = f.positional.get(2).ok_or("poke needs some bytes")?;
    let pattern = Pattern::parse(bytes_text).map_err(|e| e.to_string())?;
    if pattern.has_wildcards() {
        return Err("poke needs whole bytes: a wildcard has no value to write".to_string());
    }
    let mut doc = open(path, true)?;
    // Read it as a literal so the exact bytes are known.
    let cleaned: String = bytes_text.chars().filter(|c| !c.is_whitespace()).collect();
    let mut bytes = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.as_bytes().chunks(2) {
        let s = std::str::from_utf8(pair).map_err(|_| "bad hex".to_string())?;
        bytes.push(u8::from_str_radix(s, 16).map_err(|_| format!("`{s}` is not hex"))?);
    }
    doc.write(offset, &bytes).map_err(|e| e.to_string())?;
    println!(
        "not saved: {} byte(s) edited at {offset:#x} in memory only",
        bytes.len()
    );
    Ok(())
}

fn cmd_patch(args: &[String]) -> Result<(), String> {
    let f = Flags::parse(args, &["expect"])?;
    let path = f.positional.first().ok_or("patch needs a file")?;
    let offset = parse_number(f.positional.get(1).ok_or("patch needs an offset")?)
        .ok_or("patch: that offset is not a number")?;
    let cleaned: String = f
        .positional
        .get(2)
        .ok_or("patch needs some bytes")?
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let mut bytes = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.as_bytes().chunks(2) {
        let s = std::str::from_utf8(pair).map_err(|_| "bad hex".to_string())?;
        bytes.push(u8::from_str_radix(s, 16).map_err(|_| format!("`{s}` is not hex"))?);
    }

    let mut doc = open(path, false)?;
    if let Some(expect) = f.get("expect") {
        let want = parse_hex_bytes(expect)?;
        if want.len() != bytes.len() {
            return Err(format!(
                "--expect is {} byte(s) but the patch is {}",
                want.len(),
                bytes.len()
            ));
        }
        let have = doc.slice(offset, offset + want.len() as u64).map_err(|e| e.to_string())?;
        if have != want {
            return Err(format!(
                "refused: {offset:#x} holds {}, not the expected {}",
                hex_of(&have),
                hex_of(&want)
            ));
        }
    }
    doc.write(offset, &bytes).map_err(|e| e.to_string())?;
    let n = doc.save().map_err(|e| e.to_string())?;
    println!("wrote {} byte(s) at {offset:#x}", n);
    Ok(())
}

fn cmd_structure(args: &[String]) -> Result<(), String> {
    let f = Flags::parse(args, &["layout"])?;
    let path = f.positional.first().ok_or("structure needs a file")?;
    let offset = parse_number(f.positional.get(1).ok_or("structure needs an offset")?)
        .ok_or("structure: that offset is not a number")?;
    let layout_path = f.get("layout").ok_or("structure needs --layout <file>")?;
    let layout = parse_layout(&std::fs::read_to_string(layout_path).map_err(|e| e.to_string())?)?;
    let doc = open(path, true)?;
    print!("{}", layout.decode(&doc, offset).map_err(|e| e.to_string())?.render());
    Ok(())
}

fn cmd_scan(args: &[String]) -> Result<(), String> {
    let f = Flags::parse(args, &["layout", "limit"])?;
    let path = f.positional.first().ok_or("scan needs a file")?;
    let layout_path = f.get("layout").ok_or("scan needs --layout <file>")?;
    let limit = f.num("limit", 256)? as usize;
    let layout = parse_layout(&std::fs::read_to_string(layout_path).map_err(|e| e.to_string())?)?;
    let doc = open(path, true)?;
    for hit in layout.scan(&doc, limit).map_err(|e| e.to_string())? {
        println!("{hit:#010x}");
    }
    Ok(())
}

fn parse_hex_bytes(text: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if cleaned.is_empty() || cleaned.len() % 2 != 0 {
        return Err(format!("`{text}` is not whole bytes of hex"));
    }
    let mut out = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.as_bytes().chunks(2) {
        let s = std::str::from_utf8(pair).map_err(|_| "bad hex".to_string())?;
        out.push(u8::from_str_radix(s, 16).map_err(|_| format!("`{s}` is not hex"))?);
    }
    Ok(out)
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}