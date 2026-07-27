//! YAML emission in the style `yq .` produces.
//!
//! libyaml — and so `serde_yaml_ng` — writes a block sequence flush with the
//! key that owns it, which is valid YAML but hard to read once entries nest a
//! few levels deep. This module indents those sequences instead. Scalars are
//! still rendered by libyaml, one at a time, so quoting and block-scalar
//! decisions are unchanged and only the layout around them differs.

use serde::Serialize;
use serde_yaml_ng::{Error, Mapping, Sequence, Value};

/// Serialize `value` as YAML with block sequences indented under their key.
pub fn to_string<T: Serialize>(value: &T) -> Result<String, Error> {
    let value = serde_yaml_ng::to_value(value)?;
    let mut out = String::new();
    match &value {
        Value::Mapping(map) if !map.is_empty() => write_mapping(&mut out, map, 0, "")?,
        Value::Sequence(seq) if !seq.is_empty() => write_sequence(&mut out, seq, 0, "")?,
        other => {
            out.push_str(&inline(other, 0)?);
            out.push('\n');
        }
    }
    Ok(out)
}

/// `first` stands in for the indent on the first line only, and is always
/// exactly `indent` columns wide: that is how a `- ` marker and the entry it
/// introduces end up in the same column.
fn write_mapping(out: &mut String, map: &Mapping, indent: usize, first: &str) -> Result<(), Error> {
    for (i, (key, value)) in map.iter().enumerate() {
        if i == 0 {
            out.push_str(first);
        } else {
            pad(out, indent);
        }
        // Keys are plain scalars in every export this writes; a multi-line key
        // would need explicit `? ` syntax, which nothing here produces.
        out.push_str(&scalar(key, indent)?);
        out.push(':');

        match value {
            Value::Mapping(map) if !map.is_empty() => {
                out.push('\n');
                let first = " ".repeat(indent + 2);
                write_mapping(out, map, indent + 2, &first)?;
            }
            Value::Sequence(seq) if !seq.is_empty() => {
                out.push('\n');
                let first = " ".repeat(indent + 2);
                write_sequence(out, seq, indent + 2, &first)?;
            }
            other => {
                out.push(' ');
                out.push_str(&inline(other, indent)?);
                out.push('\n');
            }
        }
    }
    Ok(())
}

fn write_sequence(
    out: &mut String,
    seq: &Sequence,
    indent: usize,
    first: &str,
) -> Result<(), Error> {
    for (i, item) in seq.iter().enumerate() {
        let mut prefix = String::with_capacity(indent + 2);
        if i == 0 {
            prefix.push_str(first);
        } else {
            pad(&mut prefix, indent);
        }
        // The marker occupies the two columns before the entry, so an entry at
        // `indent` starts at `indent + 2` however deeply sequences nest.
        let line_indent = indent;
        prefix.push_str("- ");

        match item {
            Value::Mapping(map) if !map.is_empty() => {
                write_mapping(out, map, indent + 2, &prefix)?;
            }
            Value::Sequence(seq) if !seq.is_empty() => {
                write_sequence(out, seq, indent + 2, &prefix)?;
            }
            other => {
                out.push_str(&prefix);
                out.push_str(&inline(other, line_indent)?);
                out.push('\n');
            }
        }
    }
    Ok(())
}

/// A value that follows its `key:` or `- ` on the same line.
fn inline(value: &Value, line_indent: usize) -> Result<String, Error> {
    match value {
        Value::Mapping(map) if map.is_empty() => Ok("{}".to_string()),
        Value::Sequence(seq) if seq.is_empty() => Ok("[]".to_string()),
        other => scalar(other, line_indent),
    }
}

/// Render one scalar the way libyaml would at the top of a document, then move
/// any block-scalar body under the line the scalar actually sits on.
fn scalar(value: &Value, line_indent: usize) -> Result<String, Error> {
    let rendered = serde_yaml_ng::to_string(value)?;
    let rendered = rendered.strip_suffix('\n').unwrap_or(&rendered);

    let mut lines = rendered.lines();
    let mut out = lines.next().unwrap_or_default().to_string();
    for line in lines {
        out.push('\n');
        // libyaml indents a block scalar's body two columns in from column
        // zero; shifting it by the line's own indent keeps it two columns in
        // from wherever the scalar ended up. Blank lines stay blank rather
        // than becoming trailing whitespace.
        if !line.is_empty() {
            pad(&mut out, line_indent);
        }
        out.push_str(line);
    }
    Ok(out)
}

fn pad(out: &mut String, columns: usize) {
    for _ in 0..columns {
        out.push(' ');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture below is the one this module was checked against: its
    /// expectation is the literal output of `yq .` over what serde_yaml_ng
    /// writes for the same value.
    const NESTED: &str = "\
top: 1
seq_of_maps:
- a: 1
  b:
  - x
  - y
- c:
    d: 1
seq_of_seqs:
- - 1
  - 2
- - 3
empty_map: {}
empty_seq: []
multi: \"a\\nb\"
nested:
  deeper:
  - m: \"p\\nq\"
    n:
    - 1
    - 2
  - \"s\\nt\"
";

    #[test]
    fn sequences_are_indented_under_the_key_that_owns_them() {
        let value: Value = serde_yaml_ng::from_str(NESTED).expect("parses");
        assert_eq!(
            to_string(&value).expect("serializes"),
            "\
top: 1
seq_of_maps:
  - a: 1
    b:
      - x
      - y
  - c:
      d: 1
seq_of_seqs:
  - - 1
    - 2
  - - 3
empty_map: {}
empty_seq: []
multi: |-
  a
  b
nested:
  deeper:
    - m: |-
        p
        q
      n:
        - 1
        - 2
    - |-
      s
      t
"
        );
    }

    #[test]
    fn the_result_parses_back_to_the_value_it_came_from() {
        let value: Value = serde_yaml_ng::from_str(NESTED).expect("parses");
        let round_tripped: Value =
            serde_yaml_ng::from_str(&to_string(&value).expect("serializes")).expect("re-parses");
        assert_eq!(round_tripped, value);
    }

    #[test]
    fn scalars_are_quoted_exactly_as_libyaml_quotes_them() {
        // Anything that would otherwise read as a number, null, or structure
        // keeps its quotes. `yes` does not: libyaml follows the YAML 1.2 core
        // schema, in which it is an ordinary string either way.
        let value: Value = serde_yaml_ng::from_str(
            "a: 'yes'\nb: '123'\nc: 'null'\nd: 'x: y'\ne: ''\nf: '  lead'\ng: plain\n",
        )
        .expect("parses");
        assert_eq!(
            to_string(&value).expect("serializes"),
            "a: yes\nb: '123'\nc: 'null'\nd: 'x: y'\ne: ''\nf: '  lead'\ng: plain\n"
        );
    }

    #[test]
    fn a_document_that_is_only_a_scalar_or_an_empty_collection_still_works() {
        assert_eq!(to_string(&Value::Null).expect("serializes"), "null\n");
        assert_eq!(to_string(&"hi").expect("serializes"), "hi\n");
        assert_eq!(to_string(&Mapping::new()).expect("serializes"), "{}\n");
        assert_eq!(to_string(&Sequence::new()).expect("serializes"), "[]\n");
    }
}
