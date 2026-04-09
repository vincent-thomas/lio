use std::io;

use crate::query::value::Value;

#[derive(Debug, Clone, Copy, Default)]
pub struct EncodeOptions {
  pub pretty: bool,
}

pub trait ValueDecoder {
  fn decode_value(&self, input: &[u8]) -> io::Result<Value>;
}

pub trait ValueEncoder {
  fn encode_value(
    &self,
    value: &Value,
    options: EncodeOptions,
  ) -> io::Result<Vec<u8>>;
}

#[cfg(feature = "jq")]
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonCodec;

#[cfg(feature = "yq")]
#[derive(Debug, Clone, Copy, Default)]
pub struct YamlCodec;

#[cfg(feature = "jq")]
impl ValueDecoder for JsonCodec {
  fn decode_value(&self, input: &[u8]) -> io::Result<Value> {
    serde_json::from_slice(input)
      .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
  }
}

#[cfg(feature = "jq")]
impl ValueEncoder for JsonCodec {
  fn encode_value(
    &self,
    value: &Value,
    options: EncodeOptions,
  ) -> io::Result<Vec<u8>> {
    if options.pretty {
      serde_json::to_vec_pretty(value).map_err(io::Error::other)
    } else {
      serde_json::to_vec(value).map_err(io::Error::other)
    }
  }
}

#[cfg(feature = "yq")]
impl ValueDecoder for YamlCodec {
  fn decode_value(&self, input: &[u8]) -> io::Result<Value> {
    serde_norway::from_slice(input)
      .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
  }
}

#[cfg(feature = "yq")]
impl ValueEncoder for YamlCodec {
  fn encode_value(
    &self,
    value: &Value,
    options: EncodeOptions,
  ) -> io::Result<Vec<u8>> {
    let text = if options.pretty {
      serde_norway::to_string(value)
    } else {
      serde_norway::to_string(value)
    }
    .map_err(io::Error::other)?;
    Ok(text.into_bytes())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::query::value::Value;

  #[cfg(feature = "jq")]
  #[test]
  fn json_codec_round_trips_value() {
    let value = JsonCodec
      .decode_value(br#"{"name":"lio","count":2,"items":[true,null]}"#)
      .unwrap();

    assert_eq!(value.get_field("name").and_then(Value::as_str), Some("lio"));

    let rendered =
      JsonCodec.encode_value(&value, EncodeOptions { pretty: false }).unwrap();
    assert_eq!(
      String::from_utf8(rendered).unwrap(),
      r#"{"name":"lio","count":2,"items":[true,null]}"#
    );
  }

  #[cfg(feature = "yq")]
  #[test]
  fn yaml_codec_normalizes_yaml_into_shared_value() {
    let value = YamlCodec
      .decode_value(
        b"name: lio\ncount: 2\nitems:\n  - true\n  - null\nnested:\n  owner: vt\n",
      )
      .unwrap();

    assert_eq!(value.get_field("count"), Some(&Value::from(2_i64)));
    assert_eq!(
      value.get_field("nested").and_then(|nested| nested.get_field("owner")),
      Some(&Value::from("vt"))
    );
  }

  #[cfg(feature = "yq")]
  #[test]
  fn yaml_codec_rejects_non_string_mapping_keys() {
    let err = YamlCodec.decode_value(b"? [1, 2]\n: nope\n").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
  }

  #[cfg(feature = "yq")]
  #[test]
  fn yaml_codec_emits_plain_yaml() {
    let value = Value::object([
      ("name", Value::from("lio")),
      ("count", Value::from(2_i64)),
      ("items", Value::array(vec![Value::from(true), Value::Null])),
    ]);

    let output =
      YamlCodec.encode_value(&value, EncodeOptions::default()).unwrap();
    let text = String::from_utf8(output).unwrap();

    assert!(text.contains("name: lio"));
    assert!(text.contains("count: 2"));
    assert!(text.contains("- true"));
  }
}
