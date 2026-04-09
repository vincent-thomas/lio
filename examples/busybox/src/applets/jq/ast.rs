use std::{cmp::Ordering, collections::BTreeMap, io};

use indexmap::IndexMap;

use crate::query::{Number, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Builtin {
  Empty,
  Length,
  Type,
  Keys,
  KeysUnsorted,
  Values,
  Sort,
  Reverse,
  Unique,
  UniqueBy,
  Join,
  First,
  Last,
  Select,
  Has,
  HasKey,
  HasIndex,
  Map,
  Contains,
  StartsWith,
  EndsWith,
  Any,
  All,
  ToEntries,
  FromEntries,
  MapValues,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Stage {
  Path(Vec<Filter>),
  Builtin(BuiltinCall),
  Expr(ExprTerm),
  Compare(Comparison),
  Assign(Assignment),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum BuiltinArg {
  Key(String),
  Index(isize),
  String(String),
  Literal(Value),
  Plan(QueryPlan),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BuiltinCall {
  pub(crate) builtin: Builtin,
  pub(crate) arg: Option<BuiltinArg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareOp {
  Equal,
  NotEqual,
  Less,
  LessOrEqual,
  Greater,
  GreaterOrEqual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinaryOp {
  Add,
  Subtract,
  Multiply,
  Divide,
  And,
  Or,
  Coalesce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnaryOp {
  Not,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ExprTerm {
  Identity,
  RecursiveDescent,
  Path(Vec<Filter>),
  Builtin(BuiltinCall),
  Constructor(Constructor),
  Literal(Value),
  Variable(String),
  Compare(Box<Comparison>),
  Unary { op: UnaryOp, expr: Box<ExprTerm> },
  Binary { left: Box<ExprTerm>, op: BinaryOp, right: Box<ExprTerm> },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Constructor {
  Array(QueryPlan),
  Object(Vec<ObjectEntry>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ObjectEntry {
  pub(crate) key: String,
  pub(crate) value: QueryPlan,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Comparison {
  pub(crate) left: ExprTerm,
  pub(crate) op: CompareOp,
  pub(crate) right: ExprTerm,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Assignment {
  pub(crate) target: Vec<Filter>,
  pub(crate) op: AssignmentOp,
  pub(crate) value: ExprTerm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssignmentOp {
  Set,
  Update,
  Add,
  Subtract,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct QueryPlan {
  pub(crate) pipelines: Vec<Pipeline>,
}

impl QueryPlan {
  pub(crate) fn identity() -> Self {
    Self::default()
  }

  pub(crate) fn new(pipelines: Vec<Pipeline>) -> Self {
    Self { pipelines }
  }

  pub(crate) fn is_identity(&self) -> bool {
    self.pipelines.is_empty()
  }
}

impl PartialEq<Vec<Vec<Stage>>> for QueryPlan {
  fn eq(&self, other: &Vec<Vec<Stage>>) -> bool {
    self.pipelines.len() == other.len()
      && self.pipelines.iter().zip(other).all(|(left, right)| left == right)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Pipeline {
  pub(crate) stages: Vec<Stage>,
}

impl Pipeline {
  pub(crate) fn new(stages: Vec<Stage>) -> Self {
    Self { stages }
  }
}

impl PartialEq<Vec<Stage>> for Pipeline {
  fn eq(&self, other: &Vec<Stage>) -> bool {
    &self.stages == other
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Filter {
  Field { field: String, optional: bool },
  Index { index: isize, optional: bool },
  Iterate { optional: bool },
}

impl QueryPlan {
  pub(crate) fn execute(
    &self,
    value: Value,
    args: &BTreeMap<String, Value>,
  ) -> io::Result<Vec<Value>> {
    QueryExecutor::new(self, args).execute(value)
  }
}

struct QueryExecutor<'a> {
  plan: &'a QueryPlan,
  args: &'a BTreeMap<String, Value>,
}

impl<'a> QueryExecutor<'a> {
  fn new(plan: &'a QueryPlan, args: &'a BTreeMap<String, Value>) -> Self {
    Self { plan, args }
  }

  fn execute(&self, value: Value) -> io::Result<Vec<Value>> {
    if self.plan.is_identity() {
      return Ok(vec![value]);
    }

    let mut output = Vec::new();
    for pipeline in &self.plan.pipelines {
      output.extend(self.execute_pipeline(vec![value.clone()], pipeline)?);
    }
    Ok(output)
  }

  fn execute_pipeline(
    &self,
    mut current: Vec<Value>,
    pipeline: &Pipeline,
  ) -> io::Result<Vec<Value>> {
    for stage in &pipeline.stages {
      current = stage.apply(current, self)?;
    }

    Ok(current)
  }

  fn evaluate_scalar_term(
    &self,
    input: &Value,
    term: &ExprTerm,
  ) -> io::Result<Value> {
    term.evaluate_scalar(input, self)
  }

  fn evaluate_binary_term(
    &self,
    input: &Value,
    left: &ExprTerm,
    op: BinaryOp,
    right: &ExprTerm,
  ) -> io::Result<Value> {
    match op {
      BinaryOp::Coalesce => {
        let left = self.evaluate_coalesce_term(input, left)?;
        if let Some(left) = left.filter(QueryExecutor::is_truthy) {
          return Ok(left);
        }
        let right = self.evaluate_coalesce_term(input, right)?;
        Ok(right.unwrap_or(Value::Null))
      }
      BinaryOp::And => {
        let left = self.evaluate_scalar_term(input, left)?;
        if !Self::is_truthy(&left) {
          return Ok(Value::Bool(false));
        }
        let right = self.evaluate_scalar_term(input, right)?;
        Ok(Value::Bool(Self::is_truthy(&right)))
      }
      BinaryOp::Or => {
        let left = self.evaluate_scalar_term(input, left)?;
        if Self::is_truthy(&left) {
          return Ok(Value::Bool(true));
        }
        let right = self.evaluate_scalar_term(input, right)?;
        Ok(Value::Bool(Self::is_truthy(&right)))
      }
      BinaryOp::Add
      | BinaryOp::Subtract
      | BinaryOp::Multiply
      | BinaryOp::Divide => {
        let left = self.evaluate_scalar_term(input, left)?;
        let right = self.evaluate_scalar_term(input, right)?;
        apply_binary_value_op(op, left, right)
      }
    }
  }

  fn evaluate_coalesce_term(
    &self,
    input: &Value,
    term: &ExprTerm,
  ) -> io::Result<Option<Value>> {
    match term.evaluate(input, self) {
      Ok(values) => Ok(values.into_iter().next()),
      Err(err) if matches!(err.kind(), io::ErrorKind::NotFound) => Ok(None),
      Err(err)
        if err.kind() == io::ErrorKind::InvalidData
          && err.to_string().contains("produced no value") =>
      {
        Ok(None)
      }
      Err(err) => Err(err),
    }
  }

  fn assign_path(
    current: &mut Value,
    path: &[Filter],
    assigned: Value,
  ) -> io::Result<()> {
    if path.is_empty() {
      *current = assigned;
      return Ok(());
    }

    match &path[0] {
      Filter::Field { field, optional } => {
        if *optional {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "jq: optional assignment targets are not supported",
          ));
        }

        let Value::Object(object) = current else {
          return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("jq: cannot assign field on non-object: {field}"),
          ));
        };

        if path.len() == 1 {
          object.insert(field.clone(), assigned);
          return Ok(());
        }

        let next = object.entry(field.clone()).or_insert(Value::Null);
        Self::assign_path(next, &path[1..], assigned)
      }
      Filter::Index { index, optional } => {
        if *optional {
          return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "jq: optional assignment targets are not supported",
          ));
        }

        let Value::Array(array) = current else {
          return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("jq: cannot assign index on non-array: {index}"),
          ));
        };

        let resolved = if *index >= 0 {
          usize::try_from(*index).ok()
        } else {
          let back = index.unsigned_abs();
          array.len().checked_sub(back)
        }
        .ok_or_else(|| {
          io::Error::new(
            io::ErrorKind::NotFound,
            format!("jq: index out of bounds: {index}"),
          )
        })?;

        let Some(next) = array.get_mut(resolved) else {
          return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("jq: index out of bounds: {index}"),
          ));
        };

        if path.len() == 1 {
          *next = assigned;
          return Ok(());
        }

        Self::assign_path(next, &path[1..], assigned)
      }
      Filter::Iterate { .. } => Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "jq: assignment through iteration is not supported",
      )),
    }
  }

  fn value_at_path(input: &Value, path: &[Filter]) -> io::Result<Value> {
    let mut current = input.clone();
    for filter in path {
      let values = filter.apply(current)?;
      current = values.into_iter().next().ok_or_else(|| {
        io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: assignment target produced no value",
        )
      })?;
    }
    Ok(current)
  }

  fn plan_arg_to_value(
    &self,
    input: &Value,
    plan: &QueryPlan,
    name: &str,
  ) -> io::Result<Value> {
    plan.execute(input.clone(), self.args)?.into_iter().next().ok_or_else(
      || {
        io::Error::new(
          io::ErrorKind::InvalidData,
          format!("jq: {name} argument produced no value"),
        )
      },
    )
  }

  fn plan_arg_to_string(
    &self,
    input: &Value,
    plan: &QueryPlan,
    name: &str,
  ) -> io::Result<String> {
    let value = self.plan_arg_to_value(input, plan, name)?;
    let Value::String(text) = value else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("jq: {name} requires string argument"),
      ));
    };
    Ok(text)
  }

  fn is_truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false))
  }
}

pub(crate) fn is_truthy(value: &Value) -> bool {
  QueryExecutor::is_truthy(value)
}

impl Stage {
  fn apply(
    &self,
    current: Vec<Value>,
    executor: &QueryExecutor<'_>,
  ) -> io::Result<Vec<Value>> {
    match self {
      Stage::Path(filters) => filters.apply(current),
      Stage::Builtin(builtin) => {
        let mut next = Vec::new();
        for value in current {
          next.extend(builtin.apply(value, executor)?);
        }
        Ok(next)
      }
      Stage::Expr(expr) => {
        let mut next = Vec::new();
        for value in current {
          next.extend(expr.evaluate(&value, executor)?);
        }
        Ok(next)
      }
      Stage::Compare(compare) => {
        let mut next = Vec::new();
        for value in current {
          next.push(Value::Bool(compare.evaluate(&value, executor)?));
        }
        Ok(next)
      }
      Stage::Assign(assignment) => {
        let mut next = Vec::new();
        for value in current {
          next.push(assignment.apply(value, executor)?);
        }
        Ok(next)
      }
    }
  }
}

trait FilterSequenceExt {
  fn apply(&self, current: Vec<Value>) -> io::Result<Vec<Value>>;
}

impl FilterSequenceExt for [Filter] {
  fn apply(&self, mut current: Vec<Value>) -> io::Result<Vec<Value>> {
    for filter in self {
      let mut next = Vec::new();
      for value in current {
        next.extend(filter.apply(value)?);
      }
      current = next;
    }
    Ok(current)
  }
}

impl Filter {
  fn apply(&self, value: Value) -> io::Result<Vec<Value>> {
    match self {
      Filter::Field { field, optional } => {
        let Value::Object(object) = value else {
          if *optional {
            return Ok(Vec::new());
          }
          return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("jq: cannot index non-object with field {field}"),
          ));
        };
        let Some(value) = object.get(field) else {
          if *optional {
            return Ok(Vec::new());
          }
          return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("jq: field not found: {field}"),
          ));
        };
        Ok(vec![value.clone()])
      }
      Filter::Index { index, optional } => {
        let Value::Array(array) = value else {
          if *optional {
            return Ok(Vec::new());
          }
          return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("jq: cannot index non-array with index {index}"),
          ));
        };
        let resolved_index = if *index >= 0 {
          usize::try_from(*index).ok()
        } else {
          let back = index.unsigned_abs();
          array.len().checked_sub(back)
        };
        let Some(value) = resolved_index.and_then(|idx| array.get(idx)) else {
          if *optional {
            return Ok(Vec::new());
          }
          return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("jq: index out of bounds: {index}"),
          ));
        };
        Ok(vec![value.clone()])
      }
      Filter::Iterate { optional } => match value {
        Value::Array(array) => Ok(array),
        Value::Object(object) => Ok(object.into_values().collect()),
        _ => {
          if *optional {
            Ok(Vec::new())
          } else {
            Err(io::Error::new(
              io::ErrorKind::InvalidData,
              "jq: cannot iterate non-array/object",
            ))
          }
        }
      },
    }
  }
}

impl ExprTerm {
  fn evaluate(
    &self,
    input: &Value,
    executor: &QueryExecutor<'_>,
  ) -> io::Result<Vec<Value>> {
    match self {
      ExprTerm::Identity => Ok(vec![input.clone()]),
      ExprTerm::RecursiveDescent => {
        let mut values = Vec::new();
        collect_recursive_values(input, &mut values);
        Ok(values)
      }
      ExprTerm::Path(filters) => filters.apply(vec![input.clone()]),
      ExprTerm::Builtin(builtin) => builtin.apply(input.clone(), executor),
      ExprTerm::Constructor(constructor) => {
        constructor.evaluate(input, executor)
      }
      ExprTerm::Literal(value) => Ok(vec![value.clone()]),
      ExprTerm::Variable(name) => {
        executor.args.get(name).cloned().map(|value| vec![value]).ok_or_else(
          || {
            io::Error::new(
              io::ErrorKind::NotFound,
              format!("jq: variable not defined: ${name}"),
            )
          },
        )
      }
      ExprTerm::Compare(comparison) => {
        Ok(vec![Value::Bool(comparison.evaluate(input, executor)?)])
      }
      ExprTerm::Unary { op, expr } => {
        let value = expr.evaluate_scalar(input, executor)?;
        Ok(vec![match op {
          UnaryOp::Not => Value::Bool(!QueryExecutor::is_truthy(&value)),
        }])
      }
      ExprTerm::Binary { left, op, right } => {
        Ok(vec![executor.evaluate_binary_term(input, left, *op, right)?])
      }
    }
  }

  fn evaluate_scalar(
    &self,
    input: &Value,
    executor: &QueryExecutor<'_>,
  ) -> io::Result<Value> {
    self.evaluate(input, executor)?.into_iter().next().ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        "jq: expression produced no value",
      )
    })
  }
}

impl Constructor {
  fn evaluate(
    &self,
    input: &Value,
    executor: &QueryExecutor<'_>,
  ) -> io::Result<Vec<Value>> {
    match self {
      Constructor::Array(plan) => {
        Ok(vec![Value::Array(plan.execute(input.clone(), executor.args)?)])
      }
      Constructor::Object(entries) => {
        let mut object = IndexMap::with_capacity(entries.len());
        for entry in entries {
          let mut values = entry.value.execute(input.clone(), executor.args)?;
          let value = match values.len() {
            0 => Value::Null,
            1 => values.pop().unwrap(),
            _ => Value::Array(values),
          };
          object.insert(entry.key.clone(), value);
        }
        Ok(vec![Value::Object(object)])
      }
    }
  }
}

impl Comparison {
  fn evaluate(
    &self,
    input: &Value,
    executor: &QueryExecutor<'_>,
  ) -> io::Result<bool> {
    let left = self.left.evaluate(input, executor)?;
    let right = self.right.evaluate(input, executor)?;

    let left = left.into_iter().next().ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        "jq: comparison left side produced no value",
      )
    })?;
    let right = right.into_iter().next().ok_or_else(|| {
      io::Error::new(
        io::ErrorKind::InvalidData,
        "jq: comparison right side produced no value",
      )
    })?;

    Ok(match self.op {
      CompareOp::Equal => left == right,
      CompareOp::NotEqual => left != right,
      CompareOp::Less => shared_value_cmp(&left, &right).is_lt(),
      CompareOp::LessOrEqual => !shared_value_cmp(&left, &right).is_gt(),
      CompareOp::Greater => shared_value_cmp(&left, &right).is_gt(),
      CompareOp::GreaterOrEqual => !shared_value_cmp(&left, &right).is_lt(),
    })
  }
}

impl Assignment {
  fn apply(
    &self,
    mut input: Value,
    executor: &QueryExecutor<'_>,
  ) -> io::Result<Value> {
    let update_input;
    let base_input = match self.op {
      AssignmentOp::Update => {
        update_input = QueryExecutor::value_at_path(&input, &self.target)?;
        &update_input
      }
      AssignmentOp::Set | AssignmentOp::Add | AssignmentOp::Subtract => &input,
    };
    let assigned = self
      .value
      .evaluate(base_input, executor)?
      .into_iter()
      .next()
      .unwrap_or(Value::Null);
    let assigned = match self.op {
      AssignmentOp::Set | AssignmentOp::Update => assigned,
      AssignmentOp::Add => apply_binary_value_op(
        BinaryOp::Add,
        QueryExecutor::value_at_path(&input, &self.target)?,
        assigned,
      )?,
      AssignmentOp::Subtract => apply_binary_value_op(
        BinaryOp::Subtract,
        QueryExecutor::value_at_path(&input, &self.target)?,
        assigned,
      )?,
    };
    QueryExecutor::assign_path(&mut input, &self.target, assigned)?;
    Ok(input)
  }
}

impl BuiltinCall {
  fn apply(
    &self,
    value: Value,
    executor: &QueryExecutor<'_>,
  ) -> io::Result<Vec<Value>> {
    match self.builtin {
      Builtin::Empty => Ok(Vec::new()),
      Builtin::Length => match value {
        Value::Array(array) => Ok(vec![Value::from(array.len())]),
        Value::Object(object) => Ok(vec![Value::from(object.len())]),
        Value::String(text) => Ok(vec![Value::from(text.chars().count())]),
        Value::Null => Ok(vec![Value::from(0)]),
        Value::Number(number) => Ok(vec![Value::Number(number.abs())]),
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: length requires array, object, string, null, or number",
        )),
      },
      Builtin::Type => Ok(vec![Value::String(value.kind_name().to_owned())]),
      Builtin::Keys => match value {
        Value::Object(object) => {
          let mut keys: Vec<_> = object.keys().cloned().collect();
          keys.sort();
          Ok(vec![Value::Array(keys.into_iter().map(Value::String).collect())])
        }
        Value::Array(array) => {
          Ok(vec![Value::Array((0..array.len()).map(Value::from).collect())])
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: keys requires array or object",
        )),
      },
      Builtin::KeysUnsorted => match value {
        Value::Object(object) => Ok(vec![Value::Array(
          object.keys().cloned().map(Value::String).collect(),
        )]),
        Value::Array(array) => {
          Ok(vec![Value::Array((0..array.len()).map(Value::from).collect())])
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: keys_unsorted requires array or object",
        )),
      },
      Builtin::Values => match value {
        Value::Null | Value::Bool(false) => Ok(Vec::new()),
        other => Ok(vec![other]),
      },
      Builtin::Any => match (&value, self.arg.as_ref()) {
        (Value::Array(array), None) => {
          Ok(vec![Value::Bool(array.iter().any(QueryExecutor::is_truthy))])
        }
        (Value::Array(array), Some(BuiltinArg::Plan(plan))) => {
          let mut any = false;
          for item in array {
            let results = plan.execute(item.clone(), executor.args)?;
            if results.iter().any(QueryExecutor::is_truthy) {
              any = true;
              break;
            }
          }
          Ok(vec![Value::Bool(any)])
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: any requires array input",
        )),
      },
      Builtin::All => match (&value, self.arg.as_ref()) {
        (Value::Array(array), None) => {
          Ok(vec![Value::Bool(array.iter().all(QueryExecutor::is_truthy))])
        }
        (Value::Array(array), Some(BuiltinArg::Plan(plan))) => {
          let mut all = true;
          for item in array {
            let results = plan.execute(item.clone(), executor.args)?;
            if !results.iter().any(QueryExecutor::is_truthy) {
              all = false;
              break;
            }
          }
          Ok(vec![Value::Bool(all)])
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: all requires array input",
        )),
      },
      Builtin::Sort => match value {
        Value::Array(mut array) => {
          array.sort_by(shared_value_cmp);
          Ok(vec![Value::Array(array)])
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: sort requires array input",
        )),
      },
      Builtin::Reverse => match value {
        Value::Array(mut array) => {
          array.reverse();
          Ok(vec![Value::Array(array)])
        }
        Value::String(text) => {
          Ok(vec![Value::String(text.chars().rev().collect())])
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: reverse requires array or string input",
        )),
      },
      Builtin::Unique => match value {
        Value::Array(array) => {
          let mut seen = Vec::<Value>::new();
          let mut unique = Vec::new();
          for item in array {
            if !seen.iter().any(|seen_item| seen_item == &item) {
              seen.push(item.clone());
              unique.push(item);
            }
          }
          Ok(vec![Value::Array(unique)])
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: unique requires array input",
        )),
      },
      Builtin::UniqueBy => match (&value, self.arg.as_ref()) {
        (Value::Array(array), Some(BuiltinArg::Plan(plan))) => {
          let mut seen = Vec::<Value>::new();
          let mut unique = Vec::new();
          for item in array {
            let key = plan
              .execute(item.clone(), executor.args)?
              .into_iter()
              .next()
              .ok_or_else(|| {
                io::Error::new(
                  io::ErrorKind::InvalidData,
                  "jq: unique_by expression produced no value",
                )
              })?;
            if !seen.iter().any(|seen_key| seen_key == &key) {
              seen.push(key);
              unique.push(item.clone());
            }
          }
          Ok(vec![Value::Array(unique)])
        }
        (_, Some(BuiltinArg::Plan(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: unique_by requires array input",
        )),
        _ => Err(super::parser::not_implemented("unique_by")),
      },
      Builtin::Join => match (&value, self.arg.as_ref()) {
        (Value::Array(array), Some(BuiltinArg::String(separator))) => {
          let mut items = Vec::with_capacity(array.len());
          for item in array {
            let Value::String(text) = item else {
              return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jq: join requires array of strings",
              ));
            };
            items.push(text.as_str());
          }
          Ok(vec![Value::String(items.join(separator))])
        }
        (Value::Array(array), Some(BuiltinArg::Plan(plan))) => {
          let separator = executor.plan_arg_to_string(&value, plan, "join")?;
          let mut items = Vec::with_capacity(array.len());
          for item in array {
            let Value::String(text) = item else {
              return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jq: join requires array of strings",
              ));
            };
            items.push(text.as_str());
          }
          Ok(vec![Value::String(items.join(&separator))])
        }
        (_, Some(BuiltinArg::String(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: join requires array input",
        )),
        (_, Some(BuiltinArg::Plan(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: join requires array input",
        )),
        _ => Err(super::parser::not_implemented("join")),
      },
      Builtin::First => match value {
        Value::Array(array) => {
          array.first().cloned().map(|v| vec![v]).ok_or_else(|| {
            io::Error::new(
              io::ErrorKind::NotFound,
              "jq: first requires non-empty array",
            )
          })
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: first requires array input",
        )),
      },
      Builtin::Last => match value {
        Value::Array(array) => {
          array.last().cloned().map(|v| vec![v]).ok_or_else(|| {
            io::Error::new(
              io::ErrorKind::NotFound,
              "jq: last requires non-empty array",
            )
          })
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: last requires array input",
        )),
      },
      Builtin::Select => match self.arg.as_ref() {
        Some(BuiltinArg::Plan(plan)) => {
          let keep = plan.execute(value.clone(), executor.args)?;
          if keep.iter().any(QueryExecutor::is_truthy) {
            Ok(vec![value])
          } else {
            Ok(Vec::new())
          }
        }
        _ => Err(super::parser::not_implemented("select")),
      },
      Builtin::Has => {
        let Some(BuiltinArg::Plan(plan)) = self.arg.as_ref() else {
          return Err(super::parser::not_implemented("has"));
        };
        let key = executor.plan_arg_to_value(&value, plan, "has")?;
        match (value, key) {
          (Value::Object(object), Value::String(key)) => {
            Ok(vec![Value::Bool(object.contains_key(&key))])
          }
          (Value::Array(array), Value::Number(number)) => {
            let Some(index) =
              number.as_i64().and_then(|n| isize::try_from(n).ok())
            else {
              return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jq: has(number) requires integer argument",
              ));
            };
            let present = if index >= 0 {
              usize::try_from(index).ok().is_some_and(|idx| idx < array.len())
            } else {
              let back = index.unsigned_abs();
              array.len().checked_sub(back).is_some()
            };
            Ok(vec![Value::Bool(present)])
          }
          (Value::Object(_), _) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "jq: has on object requires string argument",
          )),
          (Value::Array(_), _) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "jq: has on array requires integer argument",
          )),
          _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "jq: has requires array or object input",
          )),
        }
      }
      Builtin::HasKey => match (&value, self.arg.as_ref()) {
        (Value::Object(object), Some(BuiltinArg::Key(key))) => {
          Ok(vec![Value::Bool(object.contains_key(key))])
        }
        (_, Some(BuiltinArg::Key(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: has(string) requires object input",
        )),
        _ => Err(super::parser::not_implemented("has")),
      },
      Builtin::HasIndex => match (&value, self.arg.as_ref()) {
        (Value::Array(array), Some(BuiltinArg::Index(index))) => {
          let present = if *index >= 0 {
            usize::try_from(*index).ok().is_some_and(|idx| idx < array.len())
          } else {
            let back = index.unsigned_abs();
            array.len().checked_sub(back).is_some()
          };
          Ok(vec![Value::Bool(present)])
        }
        (_, Some(BuiltinArg::Index(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: has(number) requires array input",
        )),
        _ => Err(super::parser::not_implemented("has")),
      },
      Builtin::Map => match (&value, self.arg.as_ref()) {
        (Value::Array(array), Some(BuiltinArg::Plan(plan))) => {
          let mut mapped = Vec::new();
          for element in array {
            mapped.extend(plan.execute(element.clone(), executor.args)?);
          }
          Ok(vec![Value::Array(mapped)])
        }
        (_, Some(BuiltinArg::Plan(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: map requires array input",
        )),
        _ => Err(super::parser::not_implemented("map")),
      },
      Builtin::MapValues => match (&value, self.arg.as_ref()) {
        (Value::Object(object), Some(BuiltinArg::Plan(plan))) => {
          let mut mapped = IndexMap::with_capacity(object.len());
          for (key, item) in object {
            let value = plan
              .execute(item.clone(), executor.args)?
              .into_iter()
              .next()
              .unwrap_or(Value::Null);
            mapped.insert(key.clone(), value);
          }
          Ok(vec![Value::Object(mapped)])
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: map_values requires object input",
        )),
      },
      Builtin::ToEntries => match value {
        Value::Object(object) => Ok(vec![Value::Array(
          object
            .into_iter()
            .map(|(key, value)| {
              Value::Object(IndexMap::from_iter([
                ("key".into(), Value::String(key)),
                ("value".into(), value),
              ]))
            })
            .collect(),
        )]),
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: to_entries requires object input",
        )),
      },
      Builtin::FromEntries => match value {
        Value::Array(entries) => {
          let mut object = IndexMap::new();
          for entry in entries {
            let Value::Object(mut item) = entry else {
              return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jq: from_entries requires array of objects",
              ));
            };
            let Some(Value::String(key)) = item.shift_remove("key") else {
              return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "jq: from_entries objects require string key",
              ));
            };
            let value = item.shift_remove("value").unwrap_or(Value::Null);
            object.insert(key, value);
          }
          Ok(vec![Value::Object(object)])
        }
        _ => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: from_entries requires array input",
        )),
      },
      Builtin::Contains => match (&value, self.arg.as_ref()) {
        (Value::String(text), Some(BuiltinArg::String(needle))) => {
          Ok(vec![Value::Bool(text.contains(needle))])
        }
        (Value::Array(array), Some(BuiltinArg::String(needle))) => {
          Ok(vec![Value::Bool(
            array.iter().any(|item| item == &Value::String(needle.clone())),
          )])
        }
        (Value::Array(array), Some(BuiltinArg::Literal(needle))) => {
          Ok(vec![Value::Bool(array.iter().any(|item| item == needle))])
        }
        (Value::String(text), Some(BuiltinArg::Plan(plan))) => {
          let needle = executor.plan_arg_to_string(&value, plan, "contains")?;
          Ok(vec![Value::Bool(text.contains(&needle))])
        }
        (Value::Array(array), Some(BuiltinArg::Plan(plan))) => {
          let needle = executor.plan_arg_to_value(&value, plan, "contains")?;
          Ok(vec![Value::Bool(array.iter().any(|item| item == &needle))])
        }
        (_, Some(_)) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: contains requires string or array input",
        )),
        _ => Err(super::parser::not_implemented("contains")),
      },
      Builtin::StartsWith => match (&value, self.arg.as_ref()) {
        (Value::String(text), Some(BuiltinArg::String(prefix))) => {
          Ok(vec![Value::Bool(text.starts_with(prefix))])
        }
        (Value::String(text), Some(BuiltinArg::Plan(plan))) => {
          let prefix =
            executor.plan_arg_to_string(&value, plan, "startswith")?;
          Ok(vec![Value::Bool(text.starts_with(&prefix))])
        }
        (_, Some(BuiltinArg::String(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: startswith requires string input",
        )),
        (_, Some(BuiltinArg::Plan(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: startswith requires string input",
        )),
        _ => Err(super::parser::not_implemented("startswith")),
      },
      Builtin::EndsWith => match (&value, self.arg.as_ref()) {
        (Value::String(text), Some(BuiltinArg::String(suffix))) => {
          Ok(vec![Value::Bool(text.ends_with(suffix))])
        }
        (Value::String(text), Some(BuiltinArg::Plan(plan))) => {
          let suffix = executor.plan_arg_to_string(&value, plan, "endswith")?;
          Ok(vec![Value::Bool(text.ends_with(&suffix))])
        }
        (_, Some(BuiltinArg::String(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: endswith requires string input",
        )),
        (_, Some(BuiltinArg::Plan(_))) => Err(io::Error::new(
          io::ErrorKind::InvalidData,
          "jq: endswith requires string input",
        )),
        _ => Err(super::parser::not_implemented("endswith")),
      },
    }
  }
}

fn shared_value_kind_rank(value: &Value) -> u8 {
  match value {
    Value::Null => 0,
    Value::Bool(_) => 1,
    Value::Number(_) => 2,
    Value::String(_) => 3,
    Value::Array(_) => 4,
    Value::Object(_) => 5,
  }
}

fn shared_value_cmp(left: &Value, right: &Value) -> Ordering {
  let rank_cmp =
    shared_value_kind_rank(left).cmp(&shared_value_kind_rank(right));
  if rank_cmp != Ordering::Equal {
    return rank_cmp;
  }

  match (left, right) {
    (Value::Null, Value::Null) => Ordering::Equal,
    (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
    (Value::Number(a), Value::Number(b)) => {
      a.as_f64().partial_cmp(&b.as_f64()).unwrap_or(Ordering::Equal)
    }
    (Value::String(a), Value::String(b)) => a.cmp(b),
    _ => Ordering::Equal,
  }
}

fn collect_recursive_values(value: &Value, out: &mut Vec<Value>) {
  out.push(value.clone());
  match value {
    Value::Array(values) => {
      for value in values {
        collect_recursive_values(value, out);
      }
    }
    Value::Object(object) => {
      for value in object.values() {
        collect_recursive_values(value, out);
      }
    }
    _ => {}
  }
}

fn numeric_value(value: &Value) -> io::Result<f64> {
  value.as_f64().ok_or_else(|| {
    io::Error::new(
      io::ErrorKind::InvalidData,
      "jq: arithmetic requires numeric operands",
    )
  })
}

fn finite_number(value: f64) -> io::Result<Value> {
  if value.is_finite() {
    Ok(Value::Number(Number::Float(value)))
  } else {
    Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "jq: arithmetic produced non-finite result",
    ))
  }
}

fn integer_number(value: &Number) -> Option<i64> {
  value.as_i64()
}

fn apply_binary_value_op(
  op: BinaryOp,
  left: Value,
  right: Value,
) -> io::Result<Value> {
  match op {
    BinaryOp::Add => match (left, right) {
      (Value::Number(left), Value::Number(right)) => {
        if let (Some(left), Some(right)) =
          (integer_number(&left), integer_number(&right))
        {
          return left.checked_add(right).map(Value::from).ok_or_else(|| {
            io::Error::new(
              io::ErrorKind::InvalidData,
              "jq: arithmetic overflowed integer range",
            )
          });
        }
        finite_number(left.as_f64() + right.as_f64())
      }
      (Value::String(mut left), Value::String(right)) => {
        left.push_str(&right);
        Ok(Value::String(left))
      }
      (Value::Array(mut left), Value::Array(right)) => {
        left.extend(right);
        Ok(Value::Array(left))
      }
      (Value::Object(mut left), Value::Object(right)) => {
        left.extend(right);
        Ok(Value::Object(left))
      }
      _ => Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "jq: + requires matching number, string, array, or object operands",
      )),
    },
    BinaryOp::Subtract => {
      if let (Value::Number(left), Value::Number(right)) = (&left, &right) {
        if let (Some(left), Some(right)) =
          (integer_number(left), integer_number(right))
        {
          return left.checked_sub(right).map(Value::from).ok_or_else(|| {
            io::Error::new(
              io::ErrorKind::InvalidData,
              "jq: arithmetic overflowed integer range",
            )
          });
        }
      }
      finite_number(numeric_value(&left)? - numeric_value(&right)?)
    }
    BinaryOp::Multiply => {
      if let (Value::Number(left), Value::Number(right)) = (&left, &right) {
        if let (Some(left), Some(right)) =
          (integer_number(left), integer_number(right))
        {
          return left.checked_mul(right).map(Value::from).ok_or_else(|| {
            io::Error::new(
              io::ErrorKind::InvalidData,
              "jq: arithmetic overflowed integer range",
            )
          });
        }
      }
      finite_number(numeric_value(&left)? * numeric_value(&right)?)
    }
    BinaryOp::Divide => {
      finite_number(numeric_value(&left)? / numeric_value(&right)?)
    }
    BinaryOp::And | BinaryOp::Or | BinaryOp::Coalesce => Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "jq: unsupported binary operator",
    )),
  }
}

#[cfg(test)]
#[cfg(feature = "jq")]
mod tests {
  use std::collections::BTreeMap;

  use crate::applets::jq::parser::QueryParser;
  use crate::applets::jq::render::{
    RenderOptions, render_json, render_json_with_options,
  };
  use crate::query::Value;

  #[test]
  fn arithmetic_expression_computes_numeric_value() {
    let plan = QueryParser::parse(".price * .qty + 1").unwrap();
    let output = render_json(br#"{"price":3,"qty":4}"#, &plan, false).unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "13\n");
  }

  #[test]
  fn assignment_sets_field_from_arg_variable() {
    let mut args = BTreeMap::new();
    args.insert("who".into(), Value::String("vt".into()));
    let output = render_json_with_options(
      br#"{"name":"lio"}"#,
      &QueryParser::parse(".metadata = $who").unwrap(),
      &args,
      RenderOptions { compact_output: true, ..RenderOptions::default() },
    )
    .unwrap();
    assert_eq!(
      String::from_utf8(output).unwrap(),
      "{\"name\":\"lio\",\"metadata\":\"vt\"}\n"
    );
  }

  #[test]
  fn update_assignment_uses_current_value() {
    let output = render_json_with_options(
      br#"{"count":2}"#,
      &QueryParser::parse(".count |= . + 3").unwrap(),
      &BTreeMap::new(),
      RenderOptions { compact_output: true, ..RenderOptions::default() },
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "{\"count\":5}\n");
  }

  #[test]
  fn add_assignment_updates_field() {
    let output = render_json_with_options(
      br#"{"count":2}"#,
      &QueryParser::parse(".count += 3").unwrap(),
      &BTreeMap::new(),
      RenderOptions { compact_output: true, ..RenderOptions::default() },
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "{\"count\":5}\n");
  }

  #[test]
  fn add_expression_concatenates_arrays() {
    let output = render_json(
      br#"null"#,
      &QueryParser::parse("[1,2] + [3]").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "[\n  1,\n  2,\n  3\n]\n");
  }

  #[test]
  fn add_expression_merges_objects() {
    let output = render_json(
      br#"null"#,
      &QueryParser::parse(r#"{"a":1,"b":2} + {"b":3,"c":4}"#).unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(
      String::from_utf8(output).unwrap(),
      "{\n  \"a\": 1,\n  \"b\": 3,\n  \"c\": 4\n}\n"
    );
  }

  #[test]
  fn builtin_keys_returns_sorted_keys() {
    let output = render_json(
      br#"{"obj":{"b":1,"a":2}}"#,
      &QueryParser::parse(".obj | keys").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "[\n  \"a\",\n  \"b\"\n]\n");
  }

  #[test]
  fn builtin_values_suppresses_false() {
    let output =
      render_json(br#"false"#, &QueryParser::parse("values").unwrap(), false)
        .unwrap();
    assert!(output.is_empty());
  }

  #[test]
  fn builtin_length_supports_null_and_number() {
    let output =
      render_json(br#"null"#, &QueryParser::parse("length").unwrap(), false)
        .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "0\n");

    let output =
      render_json(br#"-3"#, &QueryParser::parse("length").unwrap(), false)
        .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "3\n");
  }

  #[test]
  fn builtin_select_filters_stream() {
    let output = render_json(
      br#"{"users":[{"name":"vt"},{"name":"x"}]}"#,
      &QueryParser::parse(r#".users[] | select(.name == "vt")"#).unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(
      String::from_utf8(output).unwrap(),
      "{\n  \"name\": \"vt\"\n}\n"
    );
  }

  #[test]
  fn builtin_map_transforms_array() {
    let output = render_json(
      br#"{"items":[{"name":"a"},{"name":"b"}]}"#,
      &QueryParser::parse(".items | map(.name)").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "[\n  \"a\",\n  \"b\"\n]\n");
  }

  #[test]
  fn builtin_map_values_transforms_object_values() {
    let output = render_json(
      br#"{"a":1,"b":2}"#,
      &QueryParser::parse("map_values(. + 1)").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(
      String::from_utf8(output).unwrap(),
      "{\n  \"a\": 2,\n  \"b\": 3\n}\n"
    );
  }

  #[test]
  fn builtin_any_and_all_work() {
    let output = render_json(
      br#"[true,false,true]"#,
      &QueryParser::parse("any").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "true\n");

    let output = render_json(
      br#"[{"n":1},{"n":2}]"#,
      &QueryParser::parse("all(.n >= 1)").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "true\n");
  }

  #[test]
  fn builtin_to_entries_and_from_entries_round_trip() {
    let output = render_json(
      br#"{"b":2,"a":1}"#,
      &QueryParser::parse("to_entries | from_entries").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(
      String::from_utf8(output).unwrap(),
      "{\n  \"b\": 2,\n  \"a\": 1\n}\n"
    );
  }

  #[test]
  fn builtin_keys_unsorted_preserves_insertion_order() {
    let output = render_json(
      br#"{"b":2,"a":1}"#,
      &QueryParser::parse("keys_unsorted").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "[\n  \"b\",\n  \"a\"\n]\n");
  }

  #[test]
  fn builtin_unique_by_keeps_first_value_per_key() {
    let output = render_json(
      br#"[{"id":1,"name":"a"},{"id":1,"name":"b"},{"id":2,"name":"c"}]"#,
      &QueryParser::parse("unique_by(.id)").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(
      String::from_utf8(output).unwrap(),
      "[\n  {\n    \"id\": 1,\n    \"name\": \"a\"\n  },\n  {\n    \"id\": 2,\n    \"name\": \"c\"\n  }\n]\n"
    );
  }

  #[test]
  fn comparison_over_numbers_works() {
    let output = render_json(
      br#"{"n":3}"#,
      &QueryParser::parse(".n >= 2").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "true\n");
  }

  #[test]
  fn recursive_descent_emits_nested_values() {
    let output = render_json(
      br#"{"a":[1,{"b":2}]}"#,
      &QueryParser::parse("..").unwrap(),
      false,
    )
    .unwrap();
    let output = String::from_utf8(output).unwrap();
    assert!(output.contains(
      "{\n  \"a\": [\n    1,\n    {\n      \"b\": 2\n    }\n  ]\n}\n"
    ));
    assert!(output.contains("1\n"));
    assert!(output.contains("2\n"));
  }

  #[test]
  fn dynamic_has_works_for_object_and_array() {
    let mut args = BTreeMap::new();
    args.insert("key".into(), Value::String("name".into()));
    let output = render_json_with_options(
      br#"{"name":"lio"}"#,
      &QueryParser::parse("has($key)").unwrap(),
      &args,
      RenderOptions::default(),
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "true\n");

    let mut args = BTreeMap::new();
    args.insert("idx".into(), Value::from(1));
    let output = render_json_with_options(
      br#"[1,2,3]"#,
      &QueryParser::parse("has($idx)").unwrap(),
      &args,
      RenderOptions::default(),
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "true\n");
  }

  #[test]
  fn undefined_variable_reports_error() {
    let err = render_json(
      br#"{"name":"lio"}"#,
      &QueryParser::parse("$missing").unwrap(),
      false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("variable not defined"));
  }

  #[test]
  fn wrong_type_builtin_use_reports_error() {
    let err =
      render_json(br#"123"#, &QueryParser::parse("keys").unwrap(), false)
        .unwrap_err();
    assert!(err.to_string().contains("keys requires array or object"));
  }

  #[test]
  fn empty_builtin_suppresses_output() {
    let output =
      render_json(br#"{"a":1}"#, &QueryParser::parse("empty").unwrap(), false)
        .unwrap();
    assert!(output.is_empty());
  }

  #[test]
  fn coalesce_falls_back_when_field_is_missing() {
    let output = render_json_with_options(
      br#"{"uses":"actions/checkout@v6"}"#,
      &QueryParser::parse(".run // .uses").unwrap(),
      &BTreeMap::new(),
      RenderOptions { raw_output: true, ..RenderOptions::default() },
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "actions/checkout@v6\n");
  }

  #[test]
  fn coalesce_prefers_present_left_value() {
    let output = render_json_with_options(
      br#"{"run":"make lint","uses":"actions/checkout@v6"}"#,
      &QueryParser::parse(".run // .uses").unwrap(),
      &BTreeMap::new(),
      RenderOptions { raw_output: true, ..RenderOptions::default() },
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "make lint\n");
  }

  #[test]
  fn iterate_over_object_emits_values() {
    let output = render_json(
      br#"{"jobs":{"a":{"run":"one"},"b":{"run":"two"}}}"#,
      &QueryParser::parse(".jobs[] | .run").unwrap(),
      false,
    )
    .unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "\"one\"\n\"two\"\n");
  }
}
