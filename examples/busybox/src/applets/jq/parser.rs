use std::{collections::BTreeMap, io};

use super::*;
use crate::query::{Number, Value};

pub(crate) struct QueryParser<'a> {
  pub(crate) filter: &'a str,
}

pub(crate) fn not_implemented(filter: &str) -> io::Error {
  io::Error::new(
    io::ErrorKind::Unsupported,
    format!("jq: filter support is not implemented yet: {filter}"),
  )
}

impl<'a> QueryParser<'a> {
  #[cfg(feature = "jq")]
  pub(super) fn parse_command(args: &[String]) -> io::Result<JqCommand> {
    let mut raw_output = false;
    let mut compact_output = false;
    let mut sort_keys = false;
    let mut color_output = None;
    let mut slurp = false;
    let mut stream_input = false;
    let mut null_input = false;
    let mut exit_status = false;
    let mut arg_bindings = BTreeMap::new();
    let mut filter_text = None;
    let mut files = Vec::new();
    let mut index = 0;

    while index < args.len() {
      let arg = &args[index];
      if arg == "-r" {
        raw_output = true;
        index += 1;
        continue;
      }
      if arg == "-c" {
        compact_output = true;
        index += 1;
        continue;
      }
      if arg == "-S" {
        sort_keys = true;
        index += 1;
        continue;
      }
      if arg == "-C" {
        color_output = Some(true);
        index += 1;
        continue;
      }
      if arg == "-M" {
        color_output = Some(false);
        index += 1;
        continue;
      }
      if arg == "--slurp" {
        slurp = true;
        index += 1;
        continue;
      }
      if arg == "--stream" {
        stream_input = true;
        index += 1;
        continue;
      }
      if arg == "-n" {
        null_input = true;
        index += 1;
        continue;
      }
      if arg == "-e" {
        exit_status = true;
        index += 1;
        continue;
      }
      if arg == "--arg" {
        if index + 2 >= args.len() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "jq: --arg requires a name and a value",
          ));
        }
        let name = args[index + 1].clone();
        if !Self::is_identifier(&name) {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("jq: invalid variable name: {name}"),
          ));
        }
        arg_bindings.insert(name, Value::String(args[index + 2].clone()));
        index += 3;
        continue;
      }
      if arg == "--argjson" {
        if index + 2 >= args.len() {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "jq: --argjson requires a name and a value",
          ));
        }
        let name = args[index + 1].clone();
        if !Self::is_identifier(&name) {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("jq: invalid variable name: {name}"),
          ));
        }
        let value = parse_json_value(&args[index + 2]).map_err(|_| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
              "jq: invalid JSON value for --argjson: {}",
              args[index + 2]
            ),
          )
        })?;
        arg_bindings.insert(name, value);
        index += 3;
        continue;
      }

      if arg.starts_with('-') && arg != "-" {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          format!("jq: unsupported flag {arg}"),
        ));
      }

      if filter_text.is_none() && Self::looks_like_filter_text(arg) {
        filter_text = Some(arg.clone());
        index += 1;
        continue;
      }

      files.push(arg.clone());
      index += 1;
    }

    let implicit_filter = filter_text.is_none();
    let plan = match filter_text.as_deref() {
      Some(".") | None => QueryPlan::identity(),
      Some(filter) => QueryParser { filter }.parse_plan()?,
    };

    Ok(JqCommand {
      plan,
      implicit_filter,
      raw_output,
      compact_output,
      sort_keys,
      color_output,
      slurp,
      stream_input,
      null_input,
      exit_status,
      args: arg_bindings,
      files,
    })
  }

  pub(crate) fn parse(filter: &'a str) -> io::Result<QueryPlan> {
    Self { filter }.parse_plan()
  }

  fn syntax_error(filter: &str) -> io::Error {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("jq: compilation error: syntax error in filter: {filter}"),
    )
  }

  fn looks_like_filter_text(arg: &str) -> bool {
    arg == "."
      || arg.starts_with('.')
      || arg.starts_with('$')
      || arg.contains('|')
      || arg.contains(',')
      || arg.contains("==")
      || arg.contains("!=")
      || arg == "length"
      || arg == "type"
      || arg == "keys"
      || arg == "keys_unsorted"
      || arg == "values"
      || arg == "empty"
      || arg == "sort"
      || arg == "reverse"
      || arg == "unique"
      || arg == "any"
      || arg == "all"
      || arg == "to_entries"
      || arg == "from_entries"
      || arg == "first"
      || arg == "last"
      || arg == ".."
      || arg.starts_with("has(")
      || arg.starts_with("contains(")
      || arg.starts_with("startswith(")
      || arg.starts_with("endswith(")
      || arg.starts_with("join(")
      || arg.starts_with("unique_by(")
      || arg.starts_with("map(")
      || arg.starts_with("map_values(")
      || arg.starts_with("select(")
  }

  fn split_top_level(
    &self,
    input: &'a str,
    delimiter: char,
  ) -> io::Result<Vec<&'a str>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut in_string = false;
    let mut escape = false;

    for (idx, ch) in input.char_indices() {
      if in_string {
        if escape {
          escape = false;
        } else if ch == '\\' {
          escape = true;
        } else if ch == '"' {
          in_string = false;
        }
        continue;
      }

      match ch {
        '"' => in_string = true,
        '[' => bracket_depth += 1,
        ']' => {
          bracket_depth = bracket_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(input))?
        }
        '{' => brace_depth += 1,
        '}' => {
          brace_depth = brace_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(input))?
        }
        '(' => paren_depth += 1,
        ')' => {
          paren_depth = paren_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(input))?
        }
        _ => {}
      }

      if ch == delimiter
        && bracket_depth == 0
        && brace_depth == 0
        && paren_depth == 0
        && !(delimiter == '|' && input[idx..].starts_with("|="))
      {
        parts.push(&input[start..idx]);
        start = idx + ch.len_utf8();
      }
    }

    if in_string || bracket_depth != 0 || brace_depth != 0 || paren_depth != 0 {
      return Err(Self::syntax_error(input));
    }

    parts.push(&input[start..]);
    Ok(parts)
  }

  pub(super) fn parse_filter_path(
    &self,
    path: &str,
  ) -> io::Result<Vec<Filter>> {
    if path.is_empty() {
      return Err(Self::syntax_error(self.filter));
    }

    let mut steps = Vec::new();
    let bytes = path.as_bytes();
    let mut i = 0usize;
    let mut need_step = true;

    while i < bytes.len() {
      match bytes[i] {
        b'.' => {
          if need_step {
            return Err(Self::syntax_error(self.filter));
          }
          i += 1;
          if i >= bytes.len() {
            return Err(Self::syntax_error(self.filter));
          }
          need_step = true;
          continue;
        }
        b'"' => {
          let string_start = i;
          i += 1;
          let mut escape = false;
          while i < bytes.len() {
            match bytes[i] {
              b'\\' if !escape => escape = true,
              b'"' if !escape => break,
              _ => escape = false,
            }
            i += 1;
          }
          if i >= bytes.len() || bytes[i] != b'"' {
            return Err(Self::syntax_error(self.filter));
          }
          let string_end = i + 1;
          i += 1;
          let optional = if i < bytes.len() && bytes[i] == b'?' {
            i += 1;
            true
          } else {
            false
          };
          let field = parse_json_string(&path[string_start..string_end])
            .map_err(|_| Self::syntax_error(self.filter))?;
          steps.push(Filter::Field { field, optional });
          need_step = false;
          continue;
        }
        b'[' => {
          i += 1;
          if i >= bytes.len() {
            return Err(Self::syntax_error(self.filter));
          }
          if bytes[i] == b']' {
            i += 1;
            let optional = if i < bytes.len() && bytes[i] == b'?' {
              i += 1;
              true
            } else {
              false
            };
            steps.push(Filter::Iterate { optional });
            need_step = false;
            continue;
          }

          if bytes[i] == b'"' {
            let string_start = i;
            i += 1;
            let mut escape = false;
            while i < bytes.len() {
              match bytes[i] {
                b'\\' if !escape => escape = true,
                b'"' if !escape => break,
                _ => escape = false,
              }
              i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'"' {
              return Err(Self::syntax_error(self.filter));
            }
            let string_end = i + 1;
            i += 1;
            if i >= bytes.len() || bytes[i] != b']' {
              return Err(Self::syntax_error(self.filter));
            }
            i += 1;
            let optional = if i < bytes.len() && bytes[i] == b'?' {
              i += 1;
              true
            } else {
              false
            };
            let field = parse_json_string(&path[string_start..string_end])
              .map_err(|_| Self::syntax_error(self.filter))?;
            steps.push(Filter::Field { field, optional });
            need_step = false;
            continue;
          }

          let content_start = i;
          while i < bytes.len() && bytes[i] != b']' {
            i += 1;
          }
          if i >= bytes.len() {
            return Err(Self::syntax_error(self.filter));
          }
          let index = path[content_start..i]
            .parse::<isize>()
            .map_err(|_| Self::syntax_error(self.filter))?;
          i += 1;
          let optional = if i < bytes.len() && bytes[i] == b'?' {
            i += 1;
            true
          } else {
            false
          };
          steps.push(Filter::Index { index, optional });
          need_step = false;
        }
        _ => {
          let start = i;
          while i < bytes.len() && bytes[i] != b'.' && bytes[i] != b'[' {
            i += 1;
          }
          let mut field = &path[start..i];
          let optional = field.ends_with('?');
          if optional {
            field = &field[..field.len() - 1];
          }
          if field.is_empty() {
            return Err(Self::syntax_error(self.filter));
          }
          steps.push(Filter::Field { field: field.to_owned(), optional });
          need_step = false;
        }
      }
    }

    if steps.is_empty() {
      return Err(Self::syntax_error(self.filter));
    }

    Ok(steps)
  }

  fn parse_builtin_call(&self, segment: &'a str) -> io::Result<BuiltinCall> {
    match segment {
      "length" => {
        return Ok(BuiltinCall { builtin: Builtin::Length, arg: None });
      }
      "type" => {
        return Ok(BuiltinCall { builtin: Builtin::Type, arg: None });
      }
      "keys" => {
        return Ok(BuiltinCall { builtin: Builtin::Keys, arg: None });
      }
      "keys_unsorted" => {
        return Ok(BuiltinCall { builtin: Builtin::KeysUnsorted, arg: None });
      }
      "values" => {
        return Ok(BuiltinCall { builtin: Builtin::Values, arg: None });
      }
      "empty" => {
        return Ok(BuiltinCall { builtin: Builtin::Empty, arg: None });
      }
      "sort" => {
        return Ok(BuiltinCall { builtin: Builtin::Sort, arg: None });
      }
      "reverse" => {
        return Ok(BuiltinCall { builtin: Builtin::Reverse, arg: None });
      }
      "unique" => {
        return Ok(BuiltinCall { builtin: Builtin::Unique, arg: None });
      }
      "first" => {
        return Ok(BuiltinCall { builtin: Builtin::First, arg: None });
      }
      "last" => {
        return Ok(BuiltinCall { builtin: Builtin::Last, arg: None });
      }
      "any" => {
        return Ok(BuiltinCall { builtin: Builtin::Any, arg: None });
      }
      "all" => {
        return Ok(BuiltinCall { builtin: Builtin::All, arg: None });
      }
      "to_entries" => {
        return Ok(BuiltinCall { builtin: Builtin::ToEntries, arg: None });
      }
      "from_entries" => {
        return Ok(BuiltinCall { builtin: Builtin::FromEntries, arg: None });
      }
      _ => {}
    }

    if let Some(inner) =
      segment.strip_prefix("has(").and_then(|rest| rest.strip_suffix(')'))
    {
      let inner = inner.trim();
      if inner.starts_with('"') {
        let key = parse_json_string(inner)
          .map_err(|_| Self::syntax_error(self.filter))?;
        return Ok(BuiltinCall {
          builtin: Builtin::HasKey,
          arg: Some(BuiltinArg::Key(key)),
        });
      }
      if let Ok(index) = inner.parse::<isize>() {
        return Ok(BuiltinCall {
          builtin: Builtin::HasIndex,
          arg: Some(BuiltinArg::Index(index)),
        });
      }
      return Ok(BuiltinCall {
        builtin: Builtin::Has,
        arg: Some(BuiltinArg::Plan(Self::parse(inner)?)),
      });
    }

    if let Some(inner) =
      segment.strip_prefix("contains(").and_then(|rest| rest.strip_suffix(')'))
    {
      if let Ok(literal) = parse_json_value(inner.trim()) {
        return Ok(BuiltinCall {
          builtin: Builtin::Contains,
          arg: Some(match literal {
            Value::String(text) => BuiltinArg::String(text),
            other => BuiltinArg::Literal(other),
          }),
        });
      }
      return Ok(BuiltinCall {
        builtin: Builtin::Contains,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    if let Some(inner) = segment
      .strip_prefix("startswith(")
      .and_then(|rest| rest.strip_suffix(')'))
    {
      if let Ok(prefix) = parse_json_string(inner.trim()) {
        return Ok(BuiltinCall {
          builtin: Builtin::StartsWith,
          arg: Some(BuiltinArg::String(prefix)),
        });
      }
      return Ok(BuiltinCall {
        builtin: Builtin::StartsWith,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    if let Some(inner) =
      segment.strip_prefix("endswith(").and_then(|rest| rest.strip_suffix(')'))
    {
      if let Ok(suffix) = parse_json_string(inner.trim()) {
        return Ok(BuiltinCall {
          builtin: Builtin::EndsWith,
          arg: Some(BuiltinArg::String(suffix)),
        });
      }
      return Ok(BuiltinCall {
        builtin: Builtin::EndsWith,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    if let Some(inner) =
      segment.strip_prefix("unique_by(").and_then(|rest| rest.strip_suffix(')'))
    {
      return Ok(BuiltinCall {
        builtin: Builtin::UniqueBy,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    if let Some(inner) =
      segment.strip_prefix("join(").and_then(|rest| rest.strip_suffix(')'))
    {
      if let Ok(separator) = parse_json_string(inner.trim()) {
        return Ok(BuiltinCall {
          builtin: Builtin::Join,
          arg: Some(BuiltinArg::String(separator)),
        });
      }
      return Ok(BuiltinCall {
        builtin: Builtin::Join,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    if let Some(inner) =
      segment.strip_prefix("map(").and_then(|rest| rest.strip_suffix(')'))
    {
      return Ok(BuiltinCall {
        builtin: Builtin::Map,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    if let Some(inner) = segment
      .strip_prefix("map_values(")
      .and_then(|rest| rest.strip_suffix(')'))
    {
      return Ok(BuiltinCall {
        builtin: Builtin::MapValues,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    if let Some(inner) =
      segment.strip_prefix("any(").and_then(|rest| rest.strip_suffix(')'))
    {
      return Ok(BuiltinCall {
        builtin: Builtin::Any,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    if let Some(inner) =
      segment.strip_prefix("all(").and_then(|rest| rest.strip_suffix(')'))
    {
      return Ok(BuiltinCall {
        builtin: Builtin::All,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    if let Some(inner) =
      segment.strip_prefix("select(").and_then(|rest| rest.strip_suffix(')'))
    {
      return Ok(BuiltinCall {
        builtin: Builtin::Select,
        arg: Some(BuiltinArg::Plan(Self::parse(inner.trim())?)),
      });
    }

    Err(not_implemented(self.filter))
  }

  fn parse_primary(&self, segment: &str) -> io::Result<ExprTerm> {
    let segment = segment.trim();
    if segment.is_empty() {
      return Err(Self::syntax_error(self.filter));
    }

    if segment == "." {
      return Ok(ExprTerm::Identity);
    }

    if segment == ".." {
      return Ok(ExprTerm::RecursiveDescent);
    }

    if let Some(name) = segment.strip_prefix('$') {
      if Self::is_identifier(name) {
        return Ok(ExprTerm::Variable(name.to_owned()));
      }
      return Err(Self::syntax_error(self.filter));
    }

    if let Some(path) = segment.strip_prefix('.') {
      return Ok(ExprTerm::Path(self.parse_filter_path(path)?));
    }

    if matches!(
      segment,
      "length"
        | "type"
        | "keys"
        | "keys_unsorted"
        | "values"
        | "empty"
        | "sort"
        | "reverse"
        | "unique"
        | "any"
        | "all"
        | "to_entries"
        | "from_entries"
        | "first"
        | "last"
    ) || segment.starts_with("has(")
      || segment.starts_with("contains(")
      || segment.starts_with("startswith(")
      || segment.starts_with("endswith(")
      || segment.starts_with("join(")
      || segment.starts_with("unique_by(")
      || segment.starts_with("map(")
      || segment.starts_with("map_values(")
      || segment.starts_with("any(")
      || segment.starts_with("all(")
      || segment.starts_with("select(")
    {
      return Ok(ExprTerm::Builtin(self.parse_builtin_call(segment)?));
    }

    if matches!(segment, "true" | "false" | "null")
      || segment.starts_with('"')
      || segment.chars().next().is_some_and(|c| c == '-' || c.is_ascii_digit())
    {
      let value = parse_json_value(segment)
        .map_err(|_| Self::syntax_error(self.filter))?;
      return Ok(ExprTerm::Literal(value));
    }

    if segment.starts_with('[') && segment.ends_with(']') {
      if let Ok(value) = parse_json_value(segment) {
        return Ok(ExprTerm::Literal(value));
      }
      return Ok(ExprTerm::Constructor(self.parse_array_constructor(segment)?));
    }

    if segment.starts_with('{') && segment.ends_with('}') {
      if let Ok(value) = parse_json_value(segment) {
        return Ok(ExprTerm::Literal(value));
      }
      return Ok(ExprTerm::Constructor(
        self.parse_object_constructor(segment)?,
      ));
    }

    Err(not_implemented(self.filter))
  }

  fn parse_unary(&self, segment: &str) -> io::Result<ExprTerm> {
    let segment = segment.trim();
    if let Some(rest) = self.strip_keyword_prefix(segment, "not") {
      return Ok(ExprTerm::Unary {
        op: UnaryOp::Not,
        expr: Box::new(self.parse_unary(rest)?),
      });
    }
    self.parse_primary(segment)
  }

  fn parse_mul_div(&self, segment: &str) -> io::Result<ExprTerm> {
    if let Some((left, op, right)) =
      self.split_operator(segment, &["*", "/"])?
    {
      let op = if op == "*" { BinaryOp::Multiply } else { BinaryOp::Divide };
      return Ok(ExprTerm::Binary {
        left: Box::new(self.parse_mul_div(left)?),
        op,
        right: Box::new(self.parse_unary(right)?),
      });
    }
    self.parse_unary(segment)
  }

  fn parse_add_sub(&self, segment: &str) -> io::Result<ExprTerm> {
    if let Some((left, op, right)) =
      self.split_operator(segment, &["+", "-"])?
    {
      let op = if op == "+" { BinaryOp::Add } else { BinaryOp::Subtract };
      return Ok(ExprTerm::Binary {
        left: Box::new(self.parse_add_sub(left)?),
        op,
        right: Box::new(self.parse_mul_div(right)?),
      });
    }
    self.parse_mul_div(segment)
  }

  fn parse_compare_expr(&self, segment: &str) -> io::Result<ExprTerm> {
    if let Some((left, op, right)) = self.split_comparison(segment)? {
      return Ok(ExprTerm::Compare(Box::new(Comparison {
        left: self.parse_add_sub(left)?,
        op,
        right: self.parse_add_sub(right)?,
      })));
    }
    self.parse_add_sub(segment)
  }

  fn parse_and_expr(&self, segment: &str) -> io::Result<ExprTerm> {
    if let Some((left, right)) = self.split_keyword(segment, "and")? {
      return Ok(ExprTerm::Binary {
        left: Box::new(self.parse_and_expr(left)?),
        op: BinaryOp::And,
        right: Box::new(self.parse_compare_expr(right)?),
      });
    }
    self.parse_compare_expr(segment)
  }

  fn parse_expression(&self, segment: &str) -> io::Result<ExprTerm> {
    if let Some((left, _, right)) = self.split_operator(segment, &["//"])? {
      return Ok(ExprTerm::Binary {
        left: Box::new(self.parse_expression(left)?),
        op: BinaryOp::Coalesce,
        right: Box::new(self.parse_and_expr(right)?),
      });
    }

    if let Some((left, right)) = self.split_keyword(segment, "or")? {
      return Ok(ExprTerm::Binary {
        left: Box::new(self.parse_expression(left)?),
        op: BinaryOp::Or,
        right: Box::new(self.parse_and_expr(right)?),
      });
    }

    self.parse_and_expr(segment)
  }

  fn parse_array_constructor(
    &self,
    segment: &'a str,
  ) -> io::Result<Constructor> {
    let inner = &segment[1..segment.len() - 1];
    let plan = if inner.trim().is_empty() {
      QueryPlan::identity()
    } else {
      Self::parse(inner.trim())?
    };
    Ok(Constructor::Array(plan))
  }

  fn parse_object_constructor(
    &self,
    segment: &'a str,
  ) -> io::Result<Constructor> {
    let inner = &segment[1..segment.len() - 1];
    if inner.trim().is_empty() {
      return Ok(Constructor::Object(Vec::new()));
    }

    let mut entries = Vec::new();
    for entry in self.split_top_level(inner, ',')? {
      let entry = entry.trim();
      if entry.is_empty() {
        return Err(Self::syntax_error(self.filter));
      }

      if let Some((key, value)) = self.split_object_entry(entry)? {
        entries.push(ObjectEntry {
          key: self.parse_object_key(key)?,
          value: Self::parse(value.trim())?,
        });
      } else {
        let key = entry.trim();
        if !Self::is_identifier(key) {
          return Err(Self::syntax_error(self.filter));
        }
        entries.push(ObjectEntry {
          key: key.to_owned(),
          value: QueryPlan::new(vec![Pipeline::new(vec![Stage::Path(vec![
            Filter::Field { field: key.to_owned(), optional: false },
          ])])]),
        });
      }
    }

    Ok(Constructor::Object(entries))
  }

  fn split_object_entry(
    &self,
    entry: &'a str,
  ) -> io::Result<Option<(&'a str, &'a str)>> {
    let mut in_string = false;
    let mut escape = false;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;

    for (idx, ch) in entry.char_indices() {
      if in_string {
        if escape {
          escape = false;
        } else if ch == '\\' {
          escape = true;
        } else if ch == '"' {
          in_string = false;
        }
        continue;
      }

      match ch {
        '"' => in_string = true,
        '[' => bracket_depth += 1,
        ']' => {
          bracket_depth = bracket_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(entry))?
        }
        '{' => brace_depth += 1,
        '}' => {
          brace_depth = brace_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(entry))?
        }
        '(' => paren_depth += 1,
        ')' => {
          paren_depth = paren_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(entry))?
        }
        ':' if bracket_depth == 0 && brace_depth == 0 && paren_depth == 0 => {
          return Ok(Some((&entry[..idx], &entry[idx + 1..])));
        }
        _ => {}
      }
    }

    Ok(None)
  }

  fn parse_object_key(&self, key: &str) -> io::Result<String> {
    let key = key.trim();
    if key.starts_with('"') {
      return parse_json_string(key)
        .map_err(|_| Self::syntax_error(self.filter));
    }
    if Self::is_identifier(key) {
      return Ok(key.to_owned());
    }
    Err(Self::syntax_error(self.filter))
  }

  fn is_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
      Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
      _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
  }

  fn split_comparison(
    &self,
    segment: &'a str,
  ) -> io::Result<Option<(&'a str, CompareOp, &'a str)>> {
    for op in ["==", "!=", "<=", ">=", "<", ">"] {
      let mut in_string = false;
      let mut escape = false;
      let mut bracket_depth = 0usize;
      let mut brace_depth = 0usize;
      let mut paren_depth = 0usize;

      for (idx, ch) in segment.char_indices() {
        if in_string {
          if escape {
            escape = false;
          } else if ch == '\\' {
            escape = true;
          } else if ch == '"' {
            in_string = false;
          }
          continue;
        }

        match ch {
          '"' => in_string = true,
          '[' => bracket_depth += 1,
          ']' => {
            bracket_depth = bracket_depth
              .checked_sub(1)
              .ok_or_else(|| Self::syntax_error(segment))?
          }
          '{' => brace_depth += 1,
          '}' => {
            brace_depth = brace_depth
              .checked_sub(1)
              .ok_or_else(|| Self::syntax_error(segment))?
          }
          '(' => paren_depth += 1,
          ')' => {
            paren_depth = paren_depth
              .checked_sub(1)
              .ok_or_else(|| Self::syntax_error(segment))?
          }
          _ => {}
        }

        if bracket_depth == 0
          && brace_depth == 0
          && paren_depth == 0
          && segment[idx..].starts_with(op)
        {
          let lhs = &segment[..idx];
          let rhs = &segment[idx + op.len()..];
          let op = match op {
            "==" => CompareOp::Equal,
            "!=" => CompareOp::NotEqual,
            "<" => CompareOp::Less,
            "<=" => CompareOp::LessOrEqual,
            ">" => CompareOp::Greater,
            ">=" => CompareOp::GreaterOrEqual,
            _ => unreachable!(),
          };
          return Ok(Some((lhs, op, rhs)));
        }
      }
    }

    Ok(None)
  }

  fn split_operator(
    &self,
    segment: &'a str,
    operators: &'a [&'a str],
  ) -> io::Result<Option<(&'a str, &'a str, &'a str)>> {
    let mut in_string = false;
    let mut escape = false;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut candidate = None;

    for (idx, ch) in segment.char_indices() {
      if in_string {
        if escape {
          escape = false;
        } else if ch == '\\' {
          escape = true;
        } else if ch == '"' {
          in_string = false;
        }
        continue;
      }

      match ch {
        '"' => in_string = true,
        '[' => bracket_depth += 1,
        ']' => {
          bracket_depth = bracket_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(segment))?
        }
        '{' => brace_depth += 1,
        '}' => {
          brace_depth = brace_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(segment))?
        }
        '(' => paren_depth += 1,
        ')' => {
          paren_depth = paren_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(segment))?
        }
        _ => {}
      }

      if bracket_depth == 0 && brace_depth == 0 && paren_depth == 0 {
        for op in operators {
          if segment[idx..].starts_with(op) && !(*op == "-" && idx == 0) {
            candidate =
              Some((&segment[..idx], *op, &segment[idx + op.len()..]));
          }
        }
      }
    }

    Ok(candidate)
  }

  fn split_keyword(
    &self,
    segment: &'a str,
    keyword: &str,
  ) -> io::Result<Option<(&'a str, &'a str)>> {
    let mut in_string = false;
    let mut escape = false;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    let bytes = segment.as_bytes();

    for (idx, ch) in segment.char_indices() {
      if in_string {
        if escape {
          escape = false;
        } else if ch == '\\' {
          escape = true;
        } else if ch == '"' {
          in_string = false;
        }
        continue;
      }

      match ch {
        '"' => in_string = true,
        '[' => bracket_depth += 1,
        ']' => {
          bracket_depth = bracket_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(segment))?
        }
        '{' => brace_depth += 1,
        '}' => {
          brace_depth = brace_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(segment))?
        }
        '(' => paren_depth += 1,
        ')' => {
          paren_depth = paren_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(segment))?
        }
        _ => {}
      }

      if bracket_depth == 0
        && brace_depth == 0
        && paren_depth == 0
        && segment[idx..].starts_with(keyword)
      {
        let before_ok = idx == 0 || bytes[idx - 1].is_ascii_whitespace();
        let end = idx + keyword.len();
        let after_ok = end == bytes.len() || bytes[end].is_ascii_whitespace();
        if before_ok && after_ok {
          return Ok(Some((&segment[..idx], &segment[end..])));
        }
      }
    }

    Ok(None)
  }

  fn strip_keyword_prefix<'b>(
    &self,
    segment: &'b str,
    keyword: &str,
  ) -> Option<&'b str> {
    let rest = segment.strip_prefix(keyword)?;
    if rest.chars().next().is_some_and(|ch| !ch.is_ascii_whitespace()) {
      return None;
    }
    Some(rest.trim_start())
  }

  fn split_assignment(
    &self,
    segment: &'a str,
  ) -> io::Result<Option<(&'a str, AssignmentOp, &'a str)>> {
    let mut in_string = false;
    let mut escape = false;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;

    for (idx, ch) in segment.char_indices() {
      if in_string {
        if escape {
          escape = false;
        } else if ch == '\\' {
          escape = true;
        } else if ch == '"' {
          in_string = false;
        }
        continue;
      }

      match ch {
        '"' => in_string = true,
        '[' => bracket_depth += 1,
        ']' => {
          bracket_depth = bracket_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(segment))?
        }
        '{' => brace_depth += 1,
        '}' => {
          brace_depth = brace_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(segment))?
        }
        '(' => paren_depth += 1,
        ')' => {
          paren_depth = paren_depth
            .checked_sub(1)
            .ok_or_else(|| Self::syntax_error(segment))?
        }
        '|'
          if bracket_depth == 0
            && brace_depth == 0
            && paren_depth == 0
            && segment[idx..].starts_with("|=") =>
        {
          return Ok(Some((
            &segment[..idx],
            AssignmentOp::Update,
            &segment[idx + 2..],
          )));
        }
        '+'
          if bracket_depth == 0
            && brace_depth == 0
            && paren_depth == 0
            && segment[idx..].starts_with("+=") =>
        {
          return Ok(Some((
            &segment[..idx],
            AssignmentOp::Add,
            &segment[idx + 2..],
          )));
        }
        '-'
          if bracket_depth == 0
            && brace_depth == 0
            && paren_depth == 0
            && segment[idx..].starts_with("-=") =>
        {
          return Ok(Some((
            &segment[..idx],
            AssignmentOp::Subtract,
            &segment[idx + 2..],
          )));
        }
        '='
          if bracket_depth == 0
            && brace_depth == 0
            && paren_depth == 0
            && !segment[idx..].starts_with("==")
            && !segment[..idx].ends_with('=')
            && !segment[..idx].ends_with('!')
            && !segment[..idx].ends_with('|')
            && !segment[..idx].ends_with('+')
            && !segment[..idx].ends_with('-') =>
        {
          return Ok(Some((
            &segment[..idx],
            AssignmentOp::Set,
            &segment[idx + 1..],
          )));
        }
        _ => {}
      }
    }

    Ok(None)
  }

  fn split_builtin_suffix(
    &self,
    segment: &'a str,
  ) -> Option<(&'a str, &'a str)> {
    let mut in_string = false;
    let mut escape = false;
    let mut paren_depth = 0usize;

    for (idx, ch) in segment.char_indices() {
      if in_string {
        if escape {
          escape = false;
        } else if ch == '\\' {
          escape = true;
        } else if ch == '"' {
          in_string = false;
        }
        continue;
      }

      match ch {
        '"' => in_string = true,
        '(' => paren_depth += 1,
        ')' => paren_depth = paren_depth.checked_sub(1)?,
        '.' | '[' if paren_depth == 0 && idx > 0 => {
          return Some((&segment[..idx], &segment[idx..]));
        }
        _ => {}
      }
    }

    None
  }

  fn parse_stage_sequence(&self, segment: &'a str) -> io::Result<Vec<Stage>> {
    let segment = segment.trim();
    if segment.is_empty() {
      return Err(Self::syntax_error(self.filter));
    }

    if segment == "." {
      return Ok(Vec::new());
    }

    let has_boolean_expression = self.split_keyword(segment, "and")?.is_some()
      || self.split_keyword(segment, "or")?.is_some()
      || self.strip_keyword_prefix(segment, "not").is_some();
    let has_arithmetic_expression =
      self.split_operator(segment, &["+", "-", "*", "/"])?.is_some();
    let has_comparison_expression = self.split_comparison(segment)?.is_some();

    if has_comparison_expression
      && !has_boolean_expression
      && !has_arithmetic_expression
    {
      let Some((lhs, op, rhs)) = self.split_comparison(segment)? else {
        unreachable!();
      };
      return Ok(vec![Stage::Compare(Comparison {
        left: self.parse_expression(lhs)?,
        op,
        right: self.parse_expression(rhs)?,
      })]);
    }

    if let Some((lhs, op, rhs)) = self.split_assignment(segment)? {
      let lhs = lhs.trim();
      let rhs = rhs.trim();
      let Some(path) = lhs.strip_prefix('.') else {
        return Err(Self::syntax_error(self.filter));
      };
      return Ok(vec![Stage::Assign(Assignment {
        target: self.parse_filter_path(path)?,
        op,
        value: self.parse_expression(rhs)?,
      })]);
    }

    if segment.starts_with('[')
      || segment.starts_with('{')
      || segment.starts_with('"')
      || segment.starts_with('$')
      || segment == ".."
      || segment.starts_with("not ")
      || matches!(segment, "true" | "false" | "null")
      || segment.chars().next().is_some_and(|c| c == '-' || c.is_ascii_digit())
      || has_boolean_expression
      || has_arithmetic_expression
      || has_comparison_expression
    {
      return Ok(vec![Stage::Expr(self.parse_expression(segment)?)]);
    }

    if let Some(path) = segment.strip_prefix('.') {
      return Ok(vec![Stage::Path(self.parse_filter_path(path)?)]);
    }

    if let Some((builtin, suffix)) = self.split_builtin_suffix(segment) {
      return Ok(vec![
        Stage::Builtin(self.parse_builtin_call(builtin.trim())?),
        Stage::Path(self.parse_filter_path(suffix)?),
      ]);
    }

    Ok(vec![match self.parse_expression(segment)? {
      ExprTerm::Identity => return Ok(Vec::new()),
      ExprTerm::Path(filters) => Stage::Path(filters),
      ExprTerm::Builtin(builtin) => Stage::Builtin(builtin),
      expr => Stage::Expr(expr),
    }])
  }

  fn parse_plan(&self) -> io::Result<QueryPlan> {
    let mut pipelines = Vec::new();
    for branch in self.split_top_level(self.filter, ',')? {
      let mut pipeline = Vec::new();
      for segment in self.split_top_level(branch, '|')? {
        pipeline.extend(self.parse_stage_sequence(segment)?);
      }
      pipelines.push(Pipeline::new(pipeline));
    }
    Ok(QueryPlan::new(pipelines))
  }
}

fn parse_json_string(input: &str) -> io::Result<String> {
  let Value::String(text) = parse_json_value(input)? else {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "expected JSON string",
    ));
  };
  Ok(text)
}

fn parse_json_value(input: &str) -> io::Result<Value> {
  let mut parser = JsonValueParser::new(input);
  let value = parser.parse_value()?;
  parser.skip_ws();
  if !parser.is_eof() {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "unexpected trailing characters",
    ));
  }
  Ok(value)
}

struct JsonValueParser<'a> {
  input: &'a str,
  index: usize,
}

impl<'a> JsonValueParser<'a> {
  fn new(input: &'a str) -> Self {
    Self { input, index: 0 }
  }

  fn parse_value(&mut self) -> io::Result<Value> {
    self.skip_ws();
    match self.peek_char() {
      Some('"') => Ok(Value::String(self.parse_string()?)),
      Some('n') => {
        self.expect_keyword("null")?;
        Ok(Value::Null)
      }
      Some('t') => {
        self.expect_keyword("true")?;
        Ok(Value::Bool(true))
      }
      Some('f') => {
        self.expect_keyword("false")?;
        Ok(Value::Bool(false))
      }
      Some('[') => self.parse_array(),
      Some('{') => self.parse_object(),
      Some('-' | '0'..='9') => self.parse_number(),
      _ => {
        Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid JSON value"))
      }
    }
  }

  fn parse_array(&mut self) -> io::Result<Value> {
    self.expect_char('[')?;
    self.skip_ws();
    let mut values = Vec::new();
    if self.consume_char_if(']') {
      return Ok(Value::Array(values));
    }
    loop {
      values.push(self.parse_value()?);
      self.skip_ws();
      if self.consume_char_if(']') {
        break;
      }
      self.expect_char(',')?;
    }
    Ok(Value::Array(values))
  }

  fn parse_object(&mut self) -> io::Result<Value> {
    self.expect_char('{')?;
    self.skip_ws();
    let mut object = indexmap::IndexMap::new();
    if self.consume_char_if('}') {
      return Ok(Value::Object(object));
    }
    loop {
      let key = self.parse_string()?;
      self.skip_ws();
      self.expect_char(':')?;
      let value = self.parse_value()?;
      object.insert(key, value);
      self.skip_ws();
      if self.consume_char_if('}') {
        break;
      }
      self.expect_char(',')?;
    }
    Ok(Value::Object(object))
  }

  fn parse_number(&mut self) -> io::Result<Value> {
    let start = self.index;
    if self.consume_char_if('-') {}
    match self.peek_char() {
      Some('0') => {
        self.index += 1;
      }
      Some('1'..='9') => {
        self.index += 1;
        while matches!(self.peek_char(), Some('0'..='9')) {
          self.index += 1;
        }
      }
      _ => {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "invalid JSON number",
        ));
      }
    }
    let mut is_float = false;
    if self.consume_char_if('.') {
      is_float = true;
      let mut saw_digit = false;
      while matches!(self.peek_char(), Some('0'..='9')) {
        self.index += 1;
        saw_digit = true;
      }
      if !saw_digit {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "invalid JSON number",
        ));
      }
    }
    if matches!(self.peek_char(), Some('e' | 'E')) {
      is_float = true;
      self.index += 1;
      if matches!(self.peek_char(), Some('+' | '-')) {
        self.index += 1;
      }
      let mut saw_digit = false;
      while matches!(self.peek_char(), Some('0'..='9')) {
        self.index += 1;
        saw_digit = true;
      }
      if !saw_digit {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "invalid JSON number",
        ));
      }
    }
    let text = &self.input[start..self.index];
    if is_float {
      let value = text.parse::<f64>().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid JSON number")
      })?;
      Ok(Value::Number(Number::Float(value)))
    } else {
      let value = text.parse::<i64>().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid JSON number")
      })?;
      Ok(Value::Number(Number::Int(value)))
    }
  }

  fn parse_string(&mut self) -> io::Result<String> {
    self.expect_char('"')?;
    let mut output = String::new();
    while let Some(ch) = self.next_char() {
      match ch {
        '"' => return Ok(output),
        '\\' => {
          let escaped = self.next_char().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid escape")
          })?;
          match escaped {
            '"' => output.push('"'),
            '\\' => output.push('\\'),
            '/' => output.push('/'),
            'b' => output.push('\u{0008}'),
            'f' => output.push('\u{000c}'),
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            'u' => {
              let codepoint = self.parse_hex_u16()?;
              let ch =
                char::from_u32(u32::from(codepoint)).ok_or_else(|| {
                  io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid unicode escape",
                  )
                })?;
              output.push(ch);
            }
            _ => {
              return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid escape",
              ));
            }
          }
        }
        ch => output.push(ch),
      }
    }
    Err(io::Error::new(io::ErrorKind::InvalidInput, "unterminated string"))
  }

  fn parse_hex_u16(&mut self) -> io::Result<u16> {
    let mut value = 0u16;
    for _ in 0..4 {
      let ch = self.next_char().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "invalid unicode escape")
      })?;
      value = value
        .checked_mul(16)
        .and_then(|acc| ch.to_digit(16).map(|digit| acc + digit as u16))
        .ok_or_else(|| {
          io::Error::new(io::ErrorKind::InvalidInput, "invalid unicode escape")
        })?;
    }
    Ok(value)
  }

  fn expect_keyword(&mut self, keyword: &str) -> io::Result<()> {
    for expected in keyword.chars() {
      if self.next_char() != Some(expected) {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "unexpected token",
        ));
      }
    }
    Ok(())
  }

  fn expect_char(&mut self, ch: char) -> io::Result<()> {
    self.skip_ws();
    if self.next_char() == Some(ch) {
      Ok(())
    } else {
      Err(io::Error::new(io::ErrorKind::InvalidInput, "unexpected character"))
    }
  }

  fn consume_char_if(&mut self, ch: char) -> bool {
    self.skip_ws();
    if self.peek_char() == Some(ch) {
      self.index += ch.len_utf8();
      true
    } else {
      false
    }
  }

  fn skip_ws(&mut self) {
    while matches!(self.peek_char(), Some(' ' | '\n' | '\r' | '\t')) {
      self.next_char();
    }
  }

  fn peek_char(&self) -> Option<char> {
    self.input[self.index..].chars().next()
  }

  fn next_char(&mut self) -> Option<char> {
    let ch = self.peek_char()?;
    self.index += ch.len_utf8();
    Some(ch)
  }

  fn is_eof(&self) -> bool {
    self.index >= self.input.len()
  }
}

#[cfg(test)]
#[cfg(feature = "jq")]
mod tests {
  use super::*;

  #[test]
  fn parses_filter_path_steps() {
    let path = QueryParser { filter: ".field[0].name?" }
      .parse_filter_path(r#"field[0].name?"#)
      .unwrap();
    assert_eq!(
      path,
      vec![
        Filter::Field { field: "field".into(), optional: false },
        Filter::Index { index: 0, optional: false },
        Filter::Field { field: "name".into(), optional: true }
      ]
    );
  }

  #[test]
  fn parses_filter_path_with_dotted_quoted_key() {
    let path = QueryParser { filter: r#".jobs."vm-test""# }
      .parse_filter_path(r#"jobs."vm-test""#)
      .unwrap();
    assert_eq!(
      path,
      vec![
        Filter::Field { field: "jobs".into(), optional: false },
        Filter::Field { field: "vm-test".into(), optional: false },
      ]
    );
  }

  #[test]
  fn parse_command_rejects_invalid_variable_name() {
    let err = QueryParser::parse_command(&[
      "--arg".into(),
      "1bad".into(),
      "value".into(),
      ".".into(),
    ])
    .unwrap_err();
    assert!(err.to_string().contains("invalid variable name"));
  }

  #[test]
  fn parse_expression_preserves_arithmetic_precedence() {
    let plan = QueryParser::parse(".a + .b * .c").unwrap();
    assert_eq!(plan.pipelines.len(), 1);
    assert_eq!(plan.pipelines[0].stages.len(), 1);
    let Stage::Expr(ExprTerm::Binary { left, op, right }) =
      &plan.pipelines[0].stages[0]
    else {
      panic!("expected top-level binary expression");
    };
    assert_eq!(*op, BinaryOp::Add);
    assert!(matches!(left.as_ref(), ExprTerm::Path(_)));
    assert!(matches!(
      right.as_ref(),
      ExprTerm::Binary { op: BinaryOp::Multiply, .. }
    ));
  }

  #[test]
  fn parse_expression_preserves_boolean_precedence() {
    let plan = QueryParser::parse(".a or .b and .c").unwrap();
    let Stage::Expr(ExprTerm::Binary { left, op, right }) =
      &plan.pipelines[0].stages[0]
    else {
      panic!("expected top-level binary expression");
    };
    assert_eq!(*op, BinaryOp::Or);
    assert!(matches!(left.as_ref(), ExprTerm::Path(_)));
    assert!(matches!(
      right.as_ref(),
      ExprTerm::Binary { op: BinaryOp::And, .. }
    ));
  }

  #[test]
  fn parse_expression_supports_coalesce_operator() {
    let plan = QueryParser::parse(".run // .uses").unwrap();
    let Stage::Expr(ExprTerm::Binary { left, op, right }) =
      &plan.pipelines[0].stages[0]
    else {
      panic!("expected top-level binary expression");
    };
    assert_eq!(*op, BinaryOp::Coalesce);
    assert!(matches!(left.as_ref(), ExprTerm::Path(_)));
    assert!(matches!(right.as_ref(), ExprTerm::Path(_)));
  }

  #[test]
  fn parse_assignment_stage() {
    let plan = QueryParser::parse(".meta.name = $who").unwrap();
    let Stage::Assign(assignment) = &plan.pipelines[0].stages[0] else {
      panic!("expected assignment stage");
    };
    assert_eq!(
      assignment.target,
      vec![
        Filter::Field { field: "meta".into(), optional: false },
        Filter::Field { field: "name".into(), optional: false }
      ]
    );
    assert!(matches!(assignment.value, ExprTerm::Variable(_)));
    assert_eq!(assignment.op, AssignmentOp::Set);
  }

  #[test]
  fn parse_update_assignment_stage() {
    let plan = QueryParser::parse(".meta.count += 1").unwrap();
    let Stage::Assign(assignment) = &plan.pipelines[0].stages[0] else {
      panic!("expected assignment stage");
    };
    assert_eq!(assignment.op, AssignmentOp::Add);
  }

  #[test]
  fn parse_recursive_descent_expression() {
    let plan = QueryParser::parse("..").unwrap();
    assert!(matches!(
      plan.pipelines[0].stages[0],
      Stage::Expr(ExprTerm::RecursiveDescent)
    ));
  }

  #[test]
  fn parse_command_supports_new_flags() {
    let parsed = QueryParser::parse_command(&[
      "-n".into(),
      "-e".into(),
      "--argjson".into(),
      "payload".into(),
      "{\"x\":1}".into(),
      "$payload".into(),
    ])
    .unwrap();
    assert!(parsed.null_input);
    assert!(parsed.exit_status);
    assert_eq!(
      parsed.args.get("payload"),
      Some(&Value::object([("x", Value::from(1_i64))]))
    );
  }

  #[test]
  fn parse_invalid_syntax_reports_error() {
    let err = QueryParser::parse(".items | | length").unwrap_err();
    assert!(err.to_string().contains("syntax error"));
  }
}
