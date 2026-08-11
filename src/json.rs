use anyhow::{Context, Result, bail};

pub fn escape_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character < ' ' => {
                escaped.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => escaped.push(character),
        }
    }
    escaped
}

pub fn array<'a>(json: &'a str, key: &str) -> Result<&'a str> {
    let quoted_key = format!("\"{key}\"");
    let section = json
        .split_once(&quoted_key)
        .map(|(_, rest)| rest)
        .with_context(|| format!("missing {key}"))?;
    let start = section
        .find('[')
        .with_context(|| format!("missing {key} array"))?;
    let mut depth = 1_usize;
    let mut in_string = false;
    let mut escaped = false;
    let end = section[start + 1..]
        .char_indices()
        .find_map(|(offset, character)| {
            if in_string {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    in_string = false;
                }
                None
            } else if character == '"' {
                in_string = true;
                None
            } else if character == '[' {
                depth += 1;
                None
            } else if character == ']' {
                depth -= 1;
                (depth == 0).then_some(start + 1 + offset)
            } else {
                None
            }
        })
        .with_context(|| format!("unterminated {key} array"))?;
    Ok(&section[start + 1..end])
}

pub fn object_array<'a>(json: &'a str, key: &str) -> Result<Vec<&'a str>> {
    let values = array(json, key)?;
    let mut objects = Vec::new();
    let mut object_start = None;
    let mut depth = 0_usize;
    let mut in_string = false;
    let mut escaped = false;

    for (offset, character) in values.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }

        match character {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    object_start = Some(offset);
                }
                depth += 1;
            }
            '}' => {
                if depth == 0 {
                    bail!("unexpected closing brace in {key} array");
                }
                depth -= 1;
                if depth == 0 {
                    let start = object_start.context("missing object start")?;
                    objects.push(&values[start..offset + character.len_utf8()]);
                    object_start = None;
                }
            }
            character if depth == 0 && !character.is_whitespace() && character != ',' => {
                bail!("{key} array contains a value that is not an object");
            }
            _ => {}
        }
    }

    if in_string || depth != 0 {
        bail!("unterminated object in {key} array");
    }
    Ok(objects)
}

pub fn optional_unsigned_integer(json: &str, key: &str) -> Result<Option<u64>> {
    let quoted_key = format!("\"{key}\"");
    let Some((_, rest)) = json.split_once(&quoted_key) else {
        return Ok(None);
    };
    let value = rest
        .split_once(':')
        .map(|(_, value)| value.trim_start())
        .with_context(|| format!("missing {key}"))?;
    let digits = value
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    if digits.is_empty() {
        bail!("{key} is not an unsigned integer");
    }
    Ok(Some(digits.parse::<u64>().with_context(|| {
        format!("{key} is outside the supported integer range")
    })?))
}

pub fn unsigned_integer(json: &str, key: &str) -> Result<u64> {
    optional_unsigned_integer(json, key)?.with_context(|| format!("missing {key}"))
}

pub fn unsigned_integer_array(json: &str, key: &str) -> Result<Vec<u64>> {
    let values = array(json, key)?;
    if values.trim().is_empty() {
        return Ok(Vec::new());
    }
    values
        .split(',')
        .enumerate()
        .map(|(index, value)| {
            let value = value.trim();
            if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
                bail!("{key} entry {index} is not an unsigned integer");
            }
            value
                .parse::<u64>()
                .with_context(|| format!("{key} entry {index} is outside the supported range"))
        })
        .collect()
}

pub fn parse_strings(values: &str) -> Result<Vec<String>> {
    let mut characters = values.chars().peekable();
    let mut strings = Vec::new();

    loop {
        while characters
            .peek()
            .is_some_and(|character| character.is_whitespace() || *character == ',')
        {
            characters.next();
        }
        let Some(character) = characters.next() else {
            break;
        };
        if character != '"' {
            bail!("array value is not a JSON string");
        }

        let mut value = String::new();
        let mut closed = false;
        while let Some(character) = characters.next() {
            match character {
                '"' => {
                    closed = true;
                    break;
                }
                '\\' => match characters.next().context("unfinished JSON escape")? {
                    '"' => value.push('"'),
                    '\\' => value.push('\\'),
                    '/' => value.push('/'),
                    'b' => value.push('\u{0008}'),
                    'f' => value.push('\u{000c}'),
                    'n' => value.push('\n'),
                    'r' => value.push('\r'),
                    't' => value.push('\t'),
                    'u' => {
                        let digits = characters.by_ref().take(4).collect::<String>();
                        if digits.len() != 4 {
                            bail!("unfinished JSON Unicode escape");
                        }
                        let codepoint =
                            u32::from_str_radix(&digits, 16).context("invalid Unicode escape")?;
                        value.push(char::from_u32(codepoint).context("invalid Unicode value")?);
                    }
                    escape => bail!("unsupported JSON escape \\{escape}"),
                },
                character if character < ' ' => bail!("control character in JSON string"),
                character => value.push(character),
            }
        }
        if !closed {
            bail!("unterminated JSON string");
        }
        strings.push(value);
    }

    Ok(strings)
}

#[cfg(test)]
mod tests {
    use super::{
        array, escape_string, object_array, optional_unsigned_integer, parse_strings,
        unsigned_integer, unsigned_integer_array,
    };

    #[test]
    fn strings_escape_and_parse() {
        let value = "a\\b\n\"c]d\"";
        let json = format!("{{\"values\":[\"{}\"]}}", escape_string(value));
        assert_eq!(
            parse_strings(array(&json, "values").unwrap()).unwrap(),
            [value]
        );
    }

    #[test]
    fn nested_object_arrays_and_integers_parse() {
        let json = r#"{"version":1,"crossovers":[150,600,2400,7000],"items":[{"size":2048},{"size":512}]}"#;
        let objects = object_array(json, "items").unwrap();
        assert_eq!(objects.len(), 2);
        assert_eq!(unsigned_integer(json, "version").unwrap(), 1);
        assert_eq!(optional_unsigned_integer(json, "missing").unwrap(), None);
        assert_eq!(
            unsigned_integer_array(json, "crossovers").unwrap(),
            [150, 600, 2400, 7000]
        );
        assert_eq!(unsigned_integer(objects[0], "size").unwrap(), 2048);
        assert_eq!(unsigned_integer(objects[1], "size").unwrap(), 512);
    }
}
