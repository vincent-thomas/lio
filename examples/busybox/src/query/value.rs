use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
  Null,
  Bool,
  Number,
  String,
  Array,
  Object,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Number {
  Int(i64),
  Float(f64),
}

impl Number {
  pub fn as_i64(&self) -> Option<i64> {
    match self {
      Self::Int(value) => Some(*value),
      Self::Float(value) => {
        if value.fract() == 0.0
          && *value >= i64::MIN as f64
          && *value <= i64::MAX as f64
        {
          Some(*value as i64)
        } else {
          None
        }
      }
    }
  }

  pub fn as_f64(&self) -> f64 {
    match self {
      Self::Int(value) => *value as f64,
      Self::Float(value) => *value,
    }
  }

  pub fn is_int(&self) -> bool {
    matches!(self, Self::Int(_))
  }

  pub fn abs(&self) -> Self {
    match self {
      Self::Int(value) => Self::Int(value.checked_abs().unwrap_or(*value)),
      Self::Float(value) => Self::Float(value.abs()),
    }
  }
}

impl fmt::Display for Number {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Int(value) => write!(f, "{value}"),
      Self::Float(value) => write!(f, "{value}"),
    }
  }
}

impl From<i64> for Number {
  fn from(value: i64) -> Self {
    Self::Int(value)
  }
}

impl From<f64> for Number {
  fn from(value: f64) -> Self {
    Self::Float(value)
  }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
  #[default]
  Null,
  Bool(bool),
  Number(Number),
  String(String),
  Array(Vec<Value>),
  Object(IndexMap<String, Value>),
}

impl Value {
  pub fn kind(&self) -> ValueKind {
    match self {
      Self::Null => ValueKind::Null,
      Self::Bool(_) => ValueKind::Bool,
      Self::Number(_) => ValueKind::Number,
      Self::String(_) => ValueKind::String,
      Self::Array(_) => ValueKind::Array,
      Self::Object(_) => ValueKind::Object,
    }
  }

  pub fn kind_name(&self) -> &'static str {
    match self.kind() {
      ValueKind::Null => "null",
      ValueKind::Bool => "boolean",
      ValueKind::Number => "number",
      ValueKind::String => "string",
      ValueKind::Array => "array",
      ValueKind::Object => "object",
    }
  }

  pub fn is_truthy(&self) -> bool {
    !matches!(self, Self::Null | Self::Bool(false))
  }

  pub fn as_bool(&self) -> Option<bool> {
    match self {
      Self::Bool(value) => Some(*value),
      _ => None,
    }
  }

  pub fn as_number(&self) -> Option<&Number> {
    match self {
      Self::Number(value) => Some(value),
      _ => None,
    }
  }

  pub fn as_str(&self) -> Option<&str> {
    match self {
      Self::String(value) => Some(value),
      _ => None,
    }
  }

  pub fn as_array(&self) -> Option<&[Value]> {
    match self {
      Self::Array(values) => Some(values),
      _ => None,
    }
  }

  pub fn as_object(&self) -> Option<&IndexMap<String, Value>> {
    match self {
      Self::Object(values) => Some(values),
      _ => None,
    }
  }

  pub fn get_field(&self, key: &str) -> Option<&Value> {
    self.as_object().and_then(|object| object.get(key))
  }

  pub fn get_index(&self, index: usize) -> Option<&Value> {
    self.as_array().and_then(|array| array.get(index))
  }

  pub fn array(values: impl Into<Vec<Value>>) -> Self {
    Self::Array(values.into())
  }

  pub fn object(
    entries: impl IntoIterator<Item = (impl Into<String>, Value)>,
  ) -> Self {
    Self::Object(
      entries.into_iter().map(|(key, value)| (key.into(), value)).collect(),
    )
  }

  pub fn as_i64(&self) -> Option<i64> {
    self.as_number().and_then(Number::as_i64)
  }

  pub fn as_f64(&self) -> Option<f64> {
    self.as_number().map(Number::as_f64)
  }
}

impl From<bool> for Value {
  fn from(value: bool) -> Self {
    Self::Bool(value)
  }
}

impl From<i64> for Value {
  fn from(value: i64) -> Self {
    Self::Number(Number::Int(value))
  }
}

impl From<i32> for Value {
  fn from(value: i32) -> Self {
    Self::Number(Number::Int(i64::from(value)))
  }
}

impl From<u64> for Value {
  fn from(value: u64) -> Self {
    Self::Number(Number::Int(value as i64))
  }
}

impl From<f64> for Value {
  fn from(value: f64) -> Self {
    Self::Number(Number::Float(value))
  }
}

impl From<usize> for Value {
  fn from(value: usize) -> Self {
    Self::Number(Number::Int(value as i64))
  }
}

impl From<isize> for Value {
  fn from(value: isize) -> Self {
    Self::Number(Number::Int(value as i64))
  }
}

impl From<String> for Value {
  fn from(value: String) -> Self {
    Self::String(value)
  }
}

impl From<&str> for Value {
  fn from(value: &str) -> Self {
    Self::String(value.to_owned())
  }
}

impl From<Vec<Value>> for Value {
  fn from(value: Vec<Value>) -> Self {
    Self::Array(value)
  }
}

impl From<IndexMap<String, Value>> for Value {
  fn from(value: IndexMap<String, Value>) -> Self {
    Self::Object(value)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn truthiness_matches_jq_basics() {
    assert!(!Value::Null.is_truthy());
    assert!(!Value::Bool(false).is_truthy());
    assert!(Value::Bool(true).is_truthy());
    assert!(Value::from(0_i64).is_truthy());
    assert!(Value::from("").is_truthy());
  }

  #[test]
  fn object_and_array_helpers_work() {
    let value = Value::object([
      ("name", Value::from("lio")),
      ("items", Value::array(vec![Value::from(1_i64), Value::from(2_i64)])),
    ]);

    assert_eq!(value.get_field("name"), Some(&Value::from("lio")));
    assert_eq!(
      value.get_field("items").and_then(|items| items.get_index(1)),
      Some(&Value::from(2_i64))
    );
  }
}
