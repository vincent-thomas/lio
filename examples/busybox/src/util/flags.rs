use std::{collections::HashMap, io};

#[derive(Debug, Clone, Copy)]
pub struct FlagSpec<'a> {
  pub name: &'a str,
  pub short: &'a [char],
  pub long: &'a [&'a str],
  pub takes_value: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedFlags {
  values: HashMap<String, Vec<String>>,
  present: HashMap<String, usize>,
  positional: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FlagParser<'a> {
  applet: &'a str,
  specs: &'a [FlagSpec<'a>],
}

impl<'a> FlagParser<'a> {
  pub fn new(applet: &'a str, specs: &'a [FlagSpec<'a>]) -> Self {
    Self { applet, specs }
  }

  pub fn parse(&self, args: &[String]) -> io::Result<ParsedFlags> {
    let mut parsed = ParsedFlags::default();
    let mut index = 0usize;

    while let Some(arg) = args.get(index) {
      if arg == "--" {
        parsed.positional.extend(args[index + 1..].iter().cloned());
        return Ok(parsed);
      }
      if !arg.starts_with('-') || arg == "-" {
        parsed.positional.extend(args[index..].iter().cloned());
        return Ok(parsed);
      }

      if let Some(long) = arg.strip_prefix("--") {
        let (name, inline_value) = match long.split_once('=') {
          Some((name, value)) => (name, Some(value)),
          None => (long, None),
        };
        let spec =
          self.find_long(name).ok_or_else(|| invalid_flag(self.applet, arg))?;
        parsed.mark_present(spec.name);
        if spec.takes_value {
          let value = match inline_value {
            Some(value) => value.to_string(),
            None => {
              index += 1;
              args
                .get(index)
                .cloned()
                .ok_or_else(|| missing_value(self.applet, arg))?
            }
          };
          parsed.push_value(spec.name, value);
        } else if inline_value.is_some() {
          return Err(invalid_flag(self.applet, arg));
        }

        index += 1;
        continue;
      }

      let mut chars = arg[1..].chars().peekable();
      while let Some(ch) = chars.next() {
        let spec =
          self.find_short(ch).ok_or_else(|| invalid_flag(self.applet, arg))?;
        parsed.mark_present(spec.name);

        if spec.takes_value {
          let value = if chars.peek().is_some() {
            chars.collect()
          } else {
            index += 1;
            args
              .get(index)
              .cloned()
              .ok_or_else(|| missing_value(self.applet, arg))?
          };
          parsed.push_value(spec.name, value);
          break;
        }
      }

      index += 1;
    }

    Ok(parsed)
  }

  fn find_short(&self, short: char) -> Option<&FlagSpec<'a>> {
    self.specs.iter().find(|spec| spec.short.contains(&short))
  }

  fn find_long(&self, long: &str) -> Option<&FlagSpec<'a>> {
    self.specs.iter().find(|spec| spec.long.contains(&long))
  }
}

impl ParsedFlags {
  pub fn get_flag_exists(&self, name: &str) -> bool {
    self.present.get(name).copied().unwrap_or(0) > 0
  }

  pub fn get_flag_value(&self, name: &str) -> Option<&str> {
    self.values.get(name).and_then(|values| values.last()).map(String::as_str)
  }

  pub fn get_flag_values(&self, name: &str) -> &[String] {
    self.values.get(name).map(Vec::as_slice).unwrap_or(&[])
  }

  pub fn positional(&self) -> &[String] {
    &self.positional
  }

  fn mark_present(&mut self, name: &str) {
    *self.present.entry(name.to_string()).or_insert(0) += 1;
  }

  fn push_value(&mut self, name: &str, value: String) {
    self.values.entry(name.to_string()).or_default().push(value);
  }
}

fn invalid_flag(applet: &str, flag: &str) -> io::Error {
  io::Error::new(
    io::ErrorKind::InvalidInput,
    format!("{applet}: unrecognized option '{flag}'"),
  )
}

fn missing_value(applet: &str, flag: &str) -> io::Error {
  io::Error::new(
    io::ErrorKind::InvalidInput,
    format!("{applet}: missing value for '{flag}'"),
  )
}

#[cfg(test)]
mod tests {
  use super::*;

  const SPECS: &[FlagSpec<'static>] = &[
    FlagSpec {
      name: "color",
      short: &['C'],
      long: &["color"],
      takes_value: false,
    },
    FlagSpec { name: "all", short: &['a'], long: &["all"], takes_value: false },
    FlagSpec {
      name: "extended",
      short: &['E'],
      long: &["regexp-extended"],
      takes_value: false,
    },
    FlagSpec {
      name: "expr",
      short: &['e'],
      long: &["expression"],
      takes_value: true,
    },
  ];

  #[test]
  fn parses_combined_short_aliases() {
    let parsed = FlagParser::new("test", SPECS)
      .parse(&["-CaE".into(), "file".into()])
      .unwrap();
    assert!(parsed.get_flag_exists("color"));
    assert!(parsed.get_flag_exists("all"));
    assert!(parsed.get_flag_exists("extended"));
    assert_eq!(parsed.positional(), &["file"]);
  }

  #[test]
  fn parses_long_and_short_values() {
    let parsed = FlagParser::new("test", SPECS)
      .parse(&[
        "--expression=one".into(),
        "-e".into(),
        "two".into(),
        "file".into(),
      ])
      .unwrap();
    assert_eq!(parsed.get_flag_value("expr"), Some("two"));
    assert_eq!(
      parsed.get_flag_values("expr"),
      &["one".to_string(), "two".to_string()]
    );
    assert_eq!(parsed.positional(), &["file"]);
  }

  #[test]
  fn rejects_unknown_short_flags() {
    let err = FlagParser::new("test", SPECS).parse(&["-Z".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }

  #[test]
  fn rejects_missing_flag_values() {
    let err = FlagParser::new("test", SPECS).parse(&["-e".into()]).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
  }
}
