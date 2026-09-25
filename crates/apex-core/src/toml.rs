//! A small TOML subset parser for device profiles.
//!
//! Supported: comments, `[table]`, `[a.b]`, `[[array.of.tables]]`, bare and
//! quoted keys, basic/literal strings, integers (dec/hex, `_`), floats, bools
//! and single-line arrays of scalars. That is everything an Apex profile
//! uses; anything else is reported as an error with a line number.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    String(String),
    Integer(i64),
    Float(f64),
    Bool(bool),
    Array(Vec<Value>),
    Table(Table),
}

pub type Table = BTreeMap<String, Value>;

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Integer(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Integer(i) => Some(*i as f64),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    pub fn as_array(&self) -> Option<&Vec<Value>> {
        match self {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }
    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }
}

fn err(line: usize, msg: impl Into<String>) -> Error {
    Error::Config(format!("line {}: {}", line, msg.into()))
}

pub fn parse(src: &str) -> Result<Table> {
    let mut root = Table::new();
    // Path of the table currently receiving key/value pairs.
    let mut current: Vec<PathSeg> = Vec::new();
    for (i, raw) in src.lines().enumerate() {
        let ln = i + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("[[") {
            let name = rest.strip_suffix("]]").ok_or_else(|| err(ln, "unterminated [[table]]"))?;
            let keys = parse_dotted_key(name.trim(), ln)?;
            let (last, parents) = keys.split_last().unwrap();
            let parent = navigate(&mut root, &parents.iter().map(|k| PathSeg::Key(k.clone())).collect::<Vec<_>>(), ln)?;
            let entry = parent.entry(last.clone()).or_insert_with(|| Value::Array(Vec::new()));
            match entry {
                Value::Array(a) => {
                    a.push(Value::Table(Table::new()));
                    let idx = a.len() - 1;
                    current = parents.iter().map(|k| PathSeg::Key(k.clone())).collect();
                    current.push(PathSeg::Index(last.clone(), idx));
                }
                _ => return Err(err(ln, format!("`{last}` is not an array of tables"))),
            }
        } else if let Some(rest) = line.strip_prefix('[') {
            let name = rest.strip_suffix(']').ok_or_else(|| err(ln, "unterminated [table]"))?;
            let keys = parse_dotted_key(name.trim(), ln)?;
            current = keys.into_iter().map(PathSeg::Key).collect();
            navigate(&mut root, &current, ln)?;
        } else {
            let eq = find_eq(line).ok_or_else(|| err(ln, "expected `key = value`"))?;
            let keys = parse_dotted_key(line[..eq].trim(), ln)?;
            let (vstr, rest) = parse_value(line[eq + 1..].trim(), ln)?;
            if !rest.trim().is_empty() {
                return Err(err(ln, format!("trailing characters `{}`", rest.trim())));
            }
            let (last, parents) = keys.split_last().unwrap();
            let mut path = current.clone();
            path.extend(parents.iter().map(|k| PathSeg::Key(k.clone())));
            let t = navigate(&mut root, &path, ln)?;
            if t.insert(last.clone(), vstr).is_some() {
                return Err(err(ln, format!("duplicate key `{last}`")));
            }
        }
    }
    Ok(root)
}

#[derive(Clone, Debug)]
enum PathSeg {
    Key(String),
    Index(String, usize),
}

fn navigate<'a>(root: &'a mut Table, path: &[PathSeg], ln: usize) -> Result<&'a mut Table> {
    let mut t = root;
    for seg in path {
        match seg {
            PathSeg::Key(k) => {
                let v = t.entry(k.clone()).or_insert_with(|| Value::Table(Table::new()));
                t = match v {
                    Value::Table(tt) => tt,
                    Value::Array(a) => match a.last_mut() {
                        Some(Value::Table(tt)) => tt,
                        _ => return Err(err(ln, format!("`{k}` is not a table"))),
                    },
                    _ => return Err(err(ln, format!("`{k}` is not a table"))),
                };
            }
            PathSeg::Index(k, i) => {
                t = match t.get_mut(k) {
                    Some(Value::Array(a)) => match a.get_mut(*i) {
                        Some(Value::Table(tt)) => tt,
                        _ => return Err(err(ln, "bad array index")),
                    },
                    _ => return Err(err(ln, format!("`{k}` is not an array"))),
                };
            }
        }
    }
    Ok(t)
}

fn strip_comment(line: &str) -> &str {
    let mut in_basic = false;
    let mut in_lit = false;
    let mut prev_bs = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' if !in_lit && !prev_bs => in_basic = !in_basic,
            '\'' if !in_basic => in_lit = !in_lit,
            '#' if !in_basic && !in_lit => return &line[..i],
            _ => {}
        }
        prev_bs = c == '\\' && in_basic && !prev_bs;
    }
    line
}

fn find_eq(line: &str) -> Option<usize> {
    let mut in_q: Option<char> = None;
    for (i, c) in line.char_indices() {
        match (c, in_q) {
            ('"' | '\'', None) => in_q = Some(c),
            (q, Some(open)) if q == open => in_q = None,
            ('=', None) => return Some(i),
            _ => {}
        }
    }
    None
}

fn parse_dotted_key(s: &str, ln: usize) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    loop {
        let (k, r) = if rest.starts_with('"') || rest.starts_with('\'') {
            let (v, r) = parse_value(rest, ln)?;
            match v {
                Value::String(s) => (s, r),
                _ => unreachable!(),
            }
        } else {
            let end = rest.find(|c: char| c == '.' || c.is_whitespace()).unwrap_or(rest.len());
            let k = &rest[..end];
            if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                return Err(err(ln, format!("invalid key `{s}`")));
            }
            (k.to_string(), &rest[end..])
        };
        out.push(k);
        let r = r.trim_start();
        if let Some(r2) = r.strip_prefix('.') {
            rest = r2.trim_start();
        } else if r.is_empty() {
            return Ok(out);
        } else {
            return Err(err(ln, format!("invalid key `{s}`")));
        }
    }
}

fn parse_value(s: &str, ln: usize) -> Result<(Value, &str)> {
    let s = s.trim_start();
    if let Some(r) = s.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = r.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => return Ok((Value::String(out), &r[i + 1..])),
                '\\' => {
                    let (_, e) = chars.next().ok_or_else(|| err(ln, "bad escape"))?;
                    out.push(match e {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        '"' => '"',
                        '\\' => '\\',
                        '0' => '\0',
                        'u' => {
                            let hex: String = (0..4).filter_map(|_| chars.next().map(|x| x.1)).collect();
                            char::from_u32(u32::from_str_radix(&hex, 16).map_err(|_| err(ln, "bad \\u"))?)
                                .ok_or_else(|| err(ln, "bad \\u"))?
                        }
                        o => return Err(err(ln, format!("unknown escape \\{o}"))),
                    });
                }
                c => out.push(c),
            }
        }
        return Err(err(ln, "unterminated string"));
    }
    if let Some(r) = s.strip_prefix('\'') {
        let end = r.find('\'').ok_or_else(|| err(ln, "unterminated literal string"))?;
        return Ok((Value::String(r[..end].to_string()), &r[end + 1..]));
    }
    if let Some(mut r) = s.strip_prefix('[') {
        let mut items = Vec::new();
        loop {
            r = r.trim_start();
            if let Some(rr) = r.strip_prefix(']') {
                return Ok((Value::Array(items), rr));
            }
            let (v, rr) = parse_value(r, ln)?;
            items.push(v);
            r = rr.trim_start();
            if let Some(rr) = r.strip_prefix(',') {
                r = rr;
            } else if !r.starts_with(']') {
                return Err(err(ln, "expected `,` or `]` in array"));
            }
        }
    }
    let end = s.find(|c: char| c == ',' || c == ']' || c.is_whitespace()).unwrap_or(s.len());
    let tok = &s[..end];
    let rest = &s[end..];
    let v = match tok {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        _ => {
            let clean = tok.replace('_', "");
            if let Some(h) = clean.strip_prefix("0x") {
                Value::Integer(i64::from_str_radix(h, 16).map_err(|_| err(ln, format!("bad integer `{tok}`")))?)
            } else if let Ok(i) = clean.parse::<i64>() {
                Value::Integer(i)
            } else if let Ok(f) = clean.parse::<f64>() {
                Value::Float(f)
            } else {
                return Err(err(ln, format!("cannot parse value `{tok}` (strings must be quoted)")));
            }
        }
    };
    Ok((v, rest))
}

/// Typed accessors with good error messages.
pub struct View<'a> {
    pub table: &'a Table,
    pub path: String,
}

impl<'a> View<'a> {
    pub fn new(table: &'a Table, path: &str) -> Self {
        View { table, path: path.to_string() }
    }

    fn name(&self, k: &str) -> String {
        if self.path.is_empty() {
            k.to_string()
        } else {
            format!("{}.{}", self.path, k)
        }
    }

    pub fn str(&self, k: &str) -> Result<Option<&'a str>> {
        match self.table.get(k) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(Error::Config(format!("`{}` must be a string", self.name(k)))),
        }
    }

    pub fn int(&self, k: &str) -> Result<Option<i64>> {
        match self.table.get(k) {
            None => Ok(None),
            Some(Value::Integer(i)) => Ok(Some(*i)),
            Some(_) => Err(Error::Config(format!("`{}` must be an integer", self.name(k)))),
        }
    }

    pub fn float(&self, k: &str) -> Result<Option<f64>> {
        match self.table.get(k) {
            None => Ok(None),
            Some(v) => v.as_float().map(Some).ok_or_else(|| Error::Config(format!("`{}` must be a number", self.name(k)))),
        }
    }

    pub fn bool(&self, k: &str) -> Result<Option<bool>> {
        match self.table.get(k) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(Error::Config(format!("`{}` must be true/false", self.name(k)))),
        }
    }

    pub fn table(&self, k: &str) -> Result<Option<View<'a>>> {
        match self.table.get(k) {
            None => Ok(None),
            Some(Value::Table(t)) => Ok(Some(View { table: t, path: self.name(k) })),
            Some(_) => Err(Error::Config(format!("`{}` must be a table", self.name(k)))),
        }
    }

    pub fn tables(&self, k: &str) -> Result<Vec<View<'a>>> {
        match self.table.get(k) {
            None => Ok(Vec::new()),
            Some(Value::Array(a)) => a
                .iter()
                .enumerate()
                .map(|(i, v)| match v {
                    Value::Table(t) => Ok(View { table: t, path: format!("{}[{}]", self.name(k), i) }),
                    _ => Err(Error::Config(format!("`{}` must be [[tables]]", self.name(k)))),
                })
                .collect(),
            Some(_) => Err(Error::Config(format!("`{}` must be [[tables]]", self.name(k)))),
        }
    }

    pub fn strs(&self, k: &str) -> Result<Vec<&'a str>> {
        match self.table.get(k) {
            None => Ok(Vec::new()),
            Some(Value::Array(a)) => {
                a.iter().map(|v| v.as_str().ok_or_else(|| Error::Config(format!("`{}` must be strings", self.name(k))))).collect()
            }
            Some(_) => Err(Error::Config(format!("`{}` must be an array", self.name(k)))),
        }
    }

    /// Reject unknown keys so typos in profiles do not silently do nothing.
    pub fn deny_unknown(&self, known: &[&str]) -> Result<()> {
        for k in self.table.keys() {
            if !known.contains(&k.as_str()) {
                return Err(Error::Config(format!("unknown key `{}`", self.name(k))));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# Apex device profile
name = "Pixel-class phone"   # trailing comment
cpus = 8
memory = "8G"
ratio = 2.5
hex = 0x1f_00

[display]
width = 1080
height = 2400
refresh = 120
modes = ["1080x2400@120", "1080x2400@60"]

[props]
"ro.product.model" = "Apex One"
nested.key = 'lit # not a comment'

[[disk]]
path = "system.img"
readonly = true

[[disk]]
path = "user\"data\".img"
"#;

    #[test]
    fn parses_profile() {
        let t = parse(SAMPLE).unwrap();
        assert_eq!(t["name"].as_str(), Some("Pixel-class phone"));
        assert_eq!(t["cpus"].as_int(), Some(8));
        assert_eq!(t["ratio"].as_float(), Some(2.5));
        assert_eq!(t["hex"].as_int(), Some(0x1f00));
        let d = t["display"].as_table().unwrap();
        assert_eq!(d["refresh"].as_int(), Some(120));
        assert_eq!(d["modes"].as_array().unwrap().len(), 2);
        let p = t["props"].as_table().unwrap();
        assert_eq!(p["ro.product.model"].as_str(), Some("Apex One"));
        assert_eq!(p["nested"].as_table().unwrap()["key"].as_str(), Some("lit # not a comment"));
        let disks = t["disk"].as_array().unwrap();
        assert_eq!(disks.len(), 2);
        assert_eq!(disks[0].as_table().unwrap()["readonly"].as_bool(), Some(true));
        assert_eq!(disks[1].as_table().unwrap()["path"].as_str(), Some("user\"data\".img"));

        let v = View::new(&t, "");
        assert_eq!(v.tables("disk").unwrap().len(), 2);
        assert!(v.int("name").is_err());
        assert!(v.table("display").unwrap().unwrap().deny_unknown(&["width"]).is_err());
    }

    #[test]
    fn reports_errors_with_lines() {
        let e = parse("a = 1\nb = nope\n").unwrap_err().to_string();
        assert!(e.contains("line 2"), "{e}");
        assert!(parse("a = 1\na = 2").is_err());
        assert!(parse("[x\n").is_err());
        assert!(parse("s = \"open").is_err());
        assert!(parse("arr = [1, 2").is_err());
    }
}
