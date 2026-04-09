#[cfg(test)]
use std::path::Path;

#[cfg(test)]
pub(super) fn matches_globs(path: &str, globs: &[String]) -> bool {
  globs.is_empty()
    || globs.iter().any(|glob| {
      glob_matches(glob, path)
        || Path::new(path)
          .file_name()
          .and_then(|value| value.to_str())
          .map(|name| glob_matches(glob, name))
          .unwrap_or(false)
    })
}

pub(super) fn glob_matches(pattern: &str, text: &str) -> bool {
  let pattern = pattern.as_bytes();
  let text = text.as_bytes();
  let mut pattern_index = 0usize;
  let mut text_index = 0usize;
  let mut star_index = None;
  let mut retry_text_index = 0usize;

  while text_index < text.len() {
    if pattern_index < pattern.len()
      && (pattern[pattern_index] == b'?'
        || pattern[pattern_index] == text[text_index])
    {
      pattern_index += 1;
      text_index += 1;
      continue;
    }

    if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
      star_index = Some(pattern_index);
      pattern_index += 1;
      retry_text_index = text_index;
      continue;
    }

    if let Some(star) = star_index {
      pattern_index = star + 1;
      retry_text_index += 1;
      text_index = retry_text_index;
      continue;
    }

    return false;
  }

  while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
    pattern_index += 1;
  }

  pattern_index == pattern.len()
}
