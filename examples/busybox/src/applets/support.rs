use std::{ffi::CString, io};

use lio::{
  Lio,
  api::{self, resource::Resource},
};

use crate::util::io as io_util;

#[derive(Debug, Clone)]
pub struct DdSpec {
  pub input: Option<String>,
  pub output: Option<String>,
  pub block_size: usize,
  pub count: Option<usize>,
  pub skip: usize,
  pub seek: usize,
  pub iodepth: usize,
}

impl Default for DdSpec {
  fn default() -> Self {
    Self {
      input: None,
      output: None,
      block_size: 512,
      count: None,
      skip: 0,
      seek: 0,
      iodepth: 1,
    }
  }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DdStats {
  pub full_in: usize,
  pub partial_in: usize,
  pub full_out: usize,
  pub partial_out: usize,
  pub bytes_copied: usize,
}

enum DdPending {
  Read {
    rx: api::io::Receiver<(io::Result<i32>, Vec<u8>)>,
    output_offset: usize,
  },
  Write {
    rx: api::io::Receiver<(io::Result<i32>, Vec<u8>)>,
    expected: usize,
  },
}

#[derive(Debug, Clone, Copy)]
pub enum XargsSeparator {
  Whitespace,
  Nul,
  Delim(char),
}

pub fn parse_dd_count(value: &str, field: &str) -> io::Result<usize> {
  value.parse::<usize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("dd: invalid {field} value '{value}'"),
    )
  })
}

pub fn parse_dd_size(value: &str) -> io::Result<usize> {
  let (number, multiplier) = match value.as_bytes().last().copied() {
    Some(b'k') | Some(b'K') => (&value[..value.len() - 1], 1024usize),
    Some(b'm') | Some(b'M') => (&value[..value.len() - 1], 1024usize * 1024),
    Some(b'g') | Some(b'G') => {
      (&value[..value.len() - 1], 1024usize * 1024 * 1024)
    }
    _ => (value, 1usize),
  };

  let base = number.parse::<usize>().map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      format!("dd: invalid bs value '{value}'"),
    )
  })?;
  base.checked_mul(multiplier).ok_or_else(|| {
    io::Error::new(io::ErrorKind::InvalidInput, "dd: bs overflows usize")
  })
}

pub fn dd_copy_sequential(
  lio: &Lio,
  input: &Resource,
  output: &Resource,
  spec: &DdSpec,
  stats: &mut DdStats,
) -> io::Result<()> {
  let mut input_offset =
    spec.skip.checked_mul(spec.block_size).ok_or_else(|| {
      io::Error::new(io::ErrorKind::InvalidInput, "dd: skip overflow")
    })?;
  let mut output_offset =
    spec.seek.checked_mul(spec.block_size).ok_or_else(|| {
      io::Error::new(io::ErrorKind::InvalidInput, "dd: seek overflow")
    })?;

  if spec.input.is_none() && spec.skip > 0 {
    discard_stdin_prefix(lio, input, input_offset)?;
    input_offset = 0;
  }

  let mut blocks_copied = 0usize;
  while spec.count.is_none_or(|count| blocks_copied < count) {
    let buf = vec![0u8; spec.block_size];
    let (result, mut returned_buf) = if spec.input.is_some() {
      if input_offset > u32::MAX as usize {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "dd: input offset exceeds current lio read_at limit",
        ));
      }
      io_util::run(
        lio,
        api::read_at(input, buf, input_offset as u32).with_lio(lio).send(),
      )
    } else {
      io_util::run(lio, api::read(input, buf).with_lio(lio).send())
    };

    let n = result? as usize;
    if n == 0 {
      break;
    }
    update_dd_input_stats(stats, n, spec.block_size);
    returned_buf.truncate(n);

    if spec.output.is_some() {
      if output_offset > u32::MAX as usize {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "dd: output offset exceeds current lio write_at limit",
        ));
      }
      let (write_result, _) = io_util::run(
        lio,
        api::write_at(output, returned_buf, output_offset as u32)
          .with_lio(lio)
          .send(),
      );
      let written = write_result? as usize;
      if written != n {
        return Err(io::Error::new(
          io::ErrorKind::WriteZero,
          "dd: short write in write_at path",
        ));
      }
      output_offset += written;
    } else {
      io_util::write_all(lio, output, returned_buf)?;
    }

    if spec.input.is_some() {
      input_offset += n;
    }
    update_dd_output_stats(stats, n, spec.block_size);
    blocks_copied += 1;
    if n < spec.block_size {
      break;
    }
  }

  Ok(())
}

pub fn dd_copy_file_to_file(
  lio: &Lio,
  input: &Resource,
  output: &Resource,
  spec: &DdSpec,
  stats: &mut DdStats,
) -> io::Result<()> {
  let skip_bytes = spec.skip.checked_mul(spec.block_size).ok_or_else(|| {
    io::Error::new(io::ErrorKind::InvalidInput, "dd: skip overflow")
  })?;
  let seek_bytes = spec.seek.checked_mul(spec.block_size).ok_or_else(|| {
    io::Error::new(io::ErrorKind::InvalidInput, "dd: seek overflow")
  })?;

  let mut pending: Vec<Option<DdPending>> =
    std::iter::repeat_with(|| None).take(spec.iodepth).collect();
  let mut next_block = 0usize;
  let mut eof_seen = false;

  loop {
    while !eof_seen
      && pending.iter().any(Option::is_none)
      && spec.count.is_none_or(|count| next_block < count)
    {
      let input_offset = skip_bytes
        .checked_add(next_block.checked_mul(spec.block_size).ok_or_else(
          || {
            io::Error::new(
              io::ErrorKind::InvalidInput,
              "dd: input offset overflow",
            )
          },
        )?)
        .ok_or_else(|| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            "dd: input offset overflow",
          )
        })?;
      let output_offset = seek_bytes
        .checked_add(next_block.checked_mul(spec.block_size).ok_or_else(
          || {
            io::Error::new(
              io::ErrorKind::InvalidInput,
              "dd: output offset overflow",
            )
          },
        )?)
        .ok_or_else(|| {
          io::Error::new(
            io::ErrorKind::InvalidInput,
            "dd: output offset overflow",
          )
        })?;

      if input_offset > u32::MAX as usize || output_offset > u32::MAX as usize {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "dd: offset exceeds current lio read_at/write_at limit",
        ));
      }

      let rx =
        api::read_at(input, vec![0u8; spec.block_size], input_offset as u32)
          .with_lio(lio)
          .send();
      let slot = pending
        .iter_mut()
        .find(|entry| entry.is_none())
        .expect("empty slot exists");
      *slot = Some(DdPending::Read { rx, output_offset });
      next_block += 1;
    }

    if pending.iter().all(Option::is_none) {
      break;
    }

    let mut made_progress = false;
    for slot in &mut pending {
      let Some(state) = slot.take() else {
        continue;
      };

      match state {
        DdPending::Read { mut rx, output_offset } => {
          if let Some((result, mut buf)) = rx.try_recv() {
            made_progress = true;
            let n = result? as usize;
            if n == 0 {
              eof_seen = true;
              continue;
            }
            update_dd_input_stats(stats, n, spec.block_size);
            buf.truncate(n);
            let write_rx = api::write_at(output, buf, output_offset as u32)
              .with_lio(lio)
              .send();
            *slot = Some(DdPending::Write { rx: write_rx, expected: n });
            if n < spec.block_size {
              eof_seen = true;
            }
          } else {
            *slot = Some(DdPending::Read { rx, output_offset });
          }
        }
        DdPending::Write { mut rx, expected } => {
          if let Some((result, _buf)) = rx.try_recv() {
            made_progress = true;
            let written = result? as usize;
            if written != expected {
              return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "dd: short write in pipelined write_at path",
              ));
            }
            update_dd_output_stats(stats, written, spec.block_size);
          } else {
            *slot = Some(DdPending::Write { rx, expected });
          }
        }
      }
    }

    if !made_progress && lio.try_run()? == 0 {
      lio.run()?;
    }
  }

  Ok(())
}

fn update_dd_input_stats(stats: &mut DdStats, bytes: usize, block_size: usize) {
  if bytes == block_size {
    stats.full_in += 1;
  } else {
    stats.partial_in += 1;
  }
}

fn update_dd_output_stats(
  stats: &mut DdStats,
  bytes: usize,
  block_size: usize,
) {
  if bytes == block_size {
    stats.full_out += 1;
  } else {
    stats.partial_out += 1;
  }
  stats.bytes_copied += bytes;
}

fn discard_stdin_prefix(
  lio: &Lio,
  input: &Resource,
  mut remaining: usize,
) -> io::Result<()> {
  while remaining > 0 {
    let buf = vec![0u8; remaining.min(8192)];
    let (result, _) =
      io_util::run(lio, api::read(input, buf).with_lio(lio).send());
    let n = result? as usize;
    if n == 0 {
      break;
    }
    remaining = remaining.saturating_sub(n);
  }
  Ok(())
}

pub fn interpret_backslash_escapes(input: &str) -> String {
  let mut out = String::new();
  let mut chars = input.chars();
  while let Some(ch) = chars.next() {
    if ch != '\\' {
      out.push(ch);
      continue;
    }
    match chars.next() {
      Some('a') => out.push('\x07'),
      Some('b') => out.push('\x08'),
      Some('e') | Some('E') => out.push('\x1b'),
      Some('f') => out.push('\x0c'),
      Some('n') => out.push('\n'),
      Some('r') => out.push('\r'),
      Some('t') => out.push('\t'),
      Some('v') => out.push('\x0b'),
      Some('\\') => out.push('\\'),
      Some('0') => out.push('\0'),
      Some(other) => {
        out.push('\\');
        out.push(other);
      }
      None => out.push('\\'),
    }
  }
  out
}

pub fn expand_tr_set(input: &str) -> Vec<u8> {
  let bytes = input.as_bytes();
  let mut out = Vec::new();
  let mut i = 0;

  while i < bytes.len() {
    if i + 2 < bytes.len() && bytes[i + 1] == b'-' && bytes[i] <= bytes[i + 2] {
      for byte in bytes[i]..=bytes[i + 2] {
        out.push(byte);
      }
      i += 3;
    } else {
      out.push(bytes[i]);
      i += 1;
    }
  }

  out
}

pub fn cksum_crc32(data: &[u8]) -> u32 {
  let mut crc = 0u32;
  for &byte in data {
    crc = cksum_crc32_step(crc, byte);
  }
  let mut len = data.len() as u64;
  while len != 0 {
    crc = cksum_crc32_step(crc, (len & 0xff) as u8);
    len >>= 8;
  }
  !crc
}

fn cksum_crc32_step(mut crc: u32, byte: u8) -> u32 {
  crc ^= (byte as u32) << 24;
  for _ in 0..8 {
    crc = if (crc & 0x8000_0000) != 0 {
      (crc << 1) ^ 0x04C1_1DB7
    } else {
      crc << 1
    };
  }
  crc
}

pub fn od_char_repr(byte: u8) -> String {
  match byte {
    b'\n' => "\\n".to_string(),
    b'\r' => "\\r".to_string(),
    b'\t' => "\\t".to_string(),
    b'\0' => "\\0".to_string(),
    b' '..=b'~' => (byte as char).to_string(),
    _ => format!("{:03o}", byte),
  }
}

pub fn encode_base64(data: &[u8]) -> String {
  const ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

  let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
  for chunk in data.chunks(3) {
    let b0 = chunk[0];
    let b1 = *chunk.get(1).unwrap_or(&0);
    let b2 = *chunk.get(2).unwrap_or(&0);

    out.push(ALPHABET[(b0 >> 2) as usize] as char);
    out.push(ALPHABET[(((b0 & 0x03) << 4) | (b1 >> 4)) as usize] as char);
    if chunk.len() > 1 {
      out.push(ALPHABET[(((b1 & 0x0f) << 2) | (b2 >> 6)) as usize] as char);
    } else {
      out.push('=');
    }
    if chunk.len() > 2 {
      out.push(ALPHABET[(b2 & 0x3f) as usize] as char);
    } else {
      out.push('=');
    }
  }
  out
}

pub fn encode_base32(data: &[u8]) -> String {
  const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

  let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
  for chunk in data.chunks(5) {
    let mut buffer = [0u8; 5];
    buffer[..chunk.len()].copy_from_slice(chunk);
    let indices = [
      (buffer[0] >> 3) & 0x1f,
      ((buffer[0] << 2) | (buffer[1] >> 6)) & 0x1f,
      (buffer[1] >> 1) & 0x1f,
      ((buffer[1] << 4) | (buffer[2] >> 4)) & 0x1f,
      ((buffer[2] << 1) | (buffer[3] >> 7)) & 0x1f,
      (buffer[3] >> 2) & 0x1f,
      ((buffer[3] << 3) | (buffer[4] >> 5)) & 0x1f,
      buffer[4] & 0x1f,
    ];

    let output_len = match chunk.len() {
      1 => 2,
      2 => 4,
      3 => 5,
      4 => 7,
      _ => 8,
    };

    for &index in &indices[..output_len] {
      out.push(ALPHABET[index as usize] as char);
    }
    for _ in output_len..8 {
      out.push('=');
    }
  }
  out
}

pub fn write_wrapped_output(
  lio: &Lio,
  encoded: &str,
  width: usize,
) -> io::Result<()> {
  let stdout = Resource::stdout();
  if encoded.is_empty() {
    return io_util::write_all(lio, &stdout, b"\n".to_vec());
  }

  for chunk in encoded.as_bytes().chunks(width) {
    let mut line = chunk.to_vec();
    line.push(b'\n');
    io_util::write_all(lio, &stdout, line)?;
  }
  Ok(())
}

pub fn digest_command<F>(
  lio: &Lio,
  files: &[String],
  digest_fn: F,
) -> io::Result<()>
where
  F: Fn(&[u8]) -> String,
{
  if files.is_empty() {
    let data = io_util::read_to_bytes(lio, None)?;
    return io_util::write_all(
      lio,
      &Resource::stdout(),
      format!("{}  -\n", digest_fn(&data)).into_bytes(),
    );
  }

  for path in files {
    let data = io_util::read_to_bytes(lio, Some(path.as_str()))?;
    io_util::write_all(
      lio,
      &Resource::stdout(),
      format!("{}  {path}\n", digest_fn(&data)).into_bytes(),
    )?;
  }

  Ok(())
}

pub fn hex_digest(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len() * 2);
  for byte in bytes {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

pub fn md5_digest(data: &[u8]) -> [u8; 16] {
  let mut a0: u32 = 0x67452301;
  let mut b0: u32 = 0xefcdab89;
  let mut c0: u32 = 0x98badcfe;
  let mut d0: u32 = 0x10325476;

  const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20,
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4,
    11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
    10, 15, 21,
  ];
  const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a,
    0xa8304613, 0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be,
    0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340,
    0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8,
    0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
    0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
    0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92,
    0xffeff47d, 0x85845dd1, 0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1,
    0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
  ];

  let mut msg = data.to_vec();
  let bit_len = (msg.len() as u64) * 8;
  msg.push(0x80);
  while (msg.len() % 64) != 56 {
    msg.push(0);
  }
  msg.extend_from_slice(&bit_len.to_le_bytes());

  for chunk in msg.chunks_exact(64) {
    let mut m = [0u32; 16];
    for (i, word) in m.iter_mut().enumerate() {
      let base = i * 4;
      *word = u32::from_le_bytes([
        chunk[base],
        chunk[base + 1],
        chunk[base + 2],
        chunk[base + 3],
      ]);
    }

    let mut a = a0;
    let mut b = b0;
    let mut c = c0;
    let mut d = d0;

    for i in 0..64 {
      let (f, g) = if i < 16 {
        ((b & c) | ((!b) & d), i)
      } else if i < 32 {
        ((d & b) | ((!d) & c), (5 * i + 1) % 16)
      } else if i < 48 {
        (b ^ c ^ d, (3 * i + 5) % 16)
      } else {
        (c ^ (b | !d), (7 * i) % 16)
      };

      let tmp = d;
      d = c;
      c = b;
      b = b.wrapping_add(
        a.wrapping_add(f)
          .wrapping_add(K[i])
          .wrapping_add(m[g])
          .rotate_left(S[i]),
      );
      a = tmp;
    }

    a0 = a0.wrapping_add(a);
    b0 = b0.wrapping_add(b);
    c0 = c0.wrapping_add(c);
    d0 = d0.wrapping_add(d);
  }

  let mut out = [0u8; 16];
  out[0..4].copy_from_slice(&a0.to_le_bytes());
  out[4..8].copy_from_slice(&b0.to_le_bytes());
  out[8..12].copy_from_slice(&c0.to_le_bytes());
  out[12..16].copy_from_slice(&d0.to_le_bytes());
  out
}

pub fn sha1_digest(data: &[u8]) -> [u8; 20] {
  let mut h0: u32 = 0x67452301;
  let mut h1: u32 = 0xefcdab89;
  let mut h2: u32 = 0x98badcfe;
  let mut h3: u32 = 0x10325476;
  let mut h4: u32 = 0xc3d2e1f0;

  let mut msg = data.to_vec();
  let bit_len = (msg.len() as u64) * 8;
  msg.push(0x80);
  while (msg.len() % 64) != 56 {
    msg.push(0);
  }
  msg.extend_from_slice(&bit_len.to_be_bytes());

  for chunk in msg.chunks_exact(64) {
    let mut w = [0u32; 80];
    for i in 0..16 {
      let base = i * 4;
      w[i] = u32::from_be_bytes([
        chunk[base],
        chunk[base + 1],
        chunk[base + 2],
        chunk[base + 3],
      ]);
    }
    for i in 16..80 {
      w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }

    let mut a = h0;
    let mut b = h1;
    let mut c = h2;
    let mut d = h3;
    let mut e = h4;

    for (i, &wi) in w.iter().enumerate() {
      let (f, k) = if i < 20 {
        ((b & c) | ((!b) & d), 0x5a827999)
      } else if i < 40 {
        (b ^ c ^ d, 0x6ed9eba1)
      } else if i < 60 {
        ((b & c) | (b & d) | (c & d), 0x8f1bbcdc)
      } else {
        (b ^ c ^ d, 0xca62c1d6)
      };

      let temp = a
        .rotate_left(5)
        .wrapping_add(f)
        .wrapping_add(e)
        .wrapping_add(k)
        .wrapping_add(wi);
      e = d;
      d = c;
      c = b.rotate_left(30);
      b = a;
      a = temp;
    }

    h0 = h0.wrapping_add(a);
    h1 = h1.wrapping_add(b);
    h2 = h2.wrapping_add(c);
    h3 = h3.wrapping_add(d);
    h4 = h4.wrapping_add(e);
  }

  let mut out = [0u8; 20];
  out[0..4].copy_from_slice(&h0.to_be_bytes());
  out[4..8].copy_from_slice(&h1.to_be_bytes());
  out[8..12].copy_from_slice(&h2.to_be_bytes());
  out[12..16].copy_from_slice(&h3.to_be_bytes());
  out[16..20].copy_from_slice(&h4.to_be_bytes());
  out
}

pub fn sha256_digest(data: &[u8]) -> [u8; 32] {
  let mut h: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
    0x1f83d9ab, 0x5be0cd19,
  ];
  const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
    0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
    0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
  ];

  let mut msg = data.to_vec();
  let bit_len = (msg.len() as u64) * 8;
  msg.push(0x80);
  while (msg.len() % 64) != 56 {
    msg.push(0);
  }
  msg.extend_from_slice(&bit_len.to_be_bytes());

  for chunk in msg.chunks_exact(64) {
    let mut w = [0u32; 64];
    for i in 0..16 {
      let base = i * 4;
      w[i] = u32::from_be_bytes([
        chunk[base],
        chunk[base + 1],
        chunk[base + 2],
        chunk[base + 3],
      ]);
    }
    for i in 16..64 {
      let s0 = w[i - 15].rotate_right(7)
        ^ w[i - 15].rotate_right(18)
        ^ (w[i - 15] >> 3);
      let s1 = w[i - 2].rotate_right(17)
        ^ w[i - 2].rotate_right(19)
        ^ (w[i - 2] >> 10);
      w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }

    let mut a = h[0];
    let mut b = h[1];
    let mut c = h[2];
    let mut d = h[3];
    let mut e = h[4];
    let mut f = h[5];
    let mut g = h[6];
    let mut hh = h[7];

    for i in 0..64 {
      let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
      let ch = (e & f) ^ ((!e) & g);
      let temp1 = hh
        .wrapping_add(s1)
        .wrapping_add(ch)
        .wrapping_add(K[i])
        .wrapping_add(w[i]);
      let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
      let maj = (a & b) ^ (a & c) ^ (b & c);
      let temp2 = s0.wrapping_add(maj);

      hh = g;
      g = f;
      f = e;
      e = d.wrapping_add(temp1);
      d = c;
      c = b;
      b = a;
      a = temp1.wrapping_add(temp2);
    }

    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
  }

  let mut out = [0u8; 32];
  for (i, word) in h.iter().enumerate() {
    out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
  }
  out
}

pub fn sha512_digest(data: &[u8]) -> [u8; 64] {
  let mut h: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
  ];
  const K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
  ];

  let mut msg = data.to_vec();
  let bit_len = (msg.len() as u128) * 8;
  msg.push(0x80);
  while (msg.len() % 128) != 112 {
    msg.push(0);
  }
  msg.extend_from_slice(&bit_len.to_be_bytes());

  for chunk in msg.chunks_exact(128) {
    let mut w = [0u64; 80];
    for i in 0..16 {
      let base = i * 8;
      w[i] = u64::from_be_bytes([
        chunk[base],
        chunk[base + 1],
        chunk[base + 2],
        chunk[base + 3],
        chunk[base + 4],
        chunk[base + 5],
        chunk[base + 6],
        chunk[base + 7],
      ]);
    }
    for i in 16..80 {
      let s0 = w[i - 15].rotate_right(1)
        ^ w[i - 15].rotate_right(8)
        ^ (w[i - 15] >> 7);
      let s1 =
        w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
      w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }

    let mut a = h[0];
    let mut b = h[1];
    let mut c = h[2];
    let mut d = h[3];
    let mut e = h[4];
    let mut f = h[5];
    let mut g = h[6];
    let mut hh = h[7];

    for i in 0..80 {
      let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
      let ch = (e & f) ^ ((!e) & g);
      let temp1 = hh
        .wrapping_add(s1)
        .wrapping_add(ch)
        .wrapping_add(K[i])
        .wrapping_add(w[i]);
      let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
      let maj = (a & b) ^ (a & c) ^ (b & c);
      let temp2 = s0.wrapping_add(maj);

      hh = g;
      g = f;
      f = e;
      e = d.wrapping_add(temp1);
      d = c;
      c = b;
      b = a;
      a = temp1.wrapping_add(temp2);
    }

    h[0] = h[0].wrapping_add(a);
    h[1] = h[1].wrapping_add(b);
    h[2] = h[2].wrapping_add(c);
    h[3] = h[3].wrapping_add(d);
    h[4] = h[4].wrapping_add(e);
    h[5] = h[5].wrapping_add(f);
    h[6] = h[6].wrapping_add(g);
    h[7] = h[7].wrapping_add(hh);
  }

  let mut out = [0u8; 64];
  for (i, word) in h.iter().enumerate() {
    out[i * 8..(i + 1) * 8].copy_from_slice(&word.to_be_bytes());
  }
  out
}

pub fn sha3_256_digest(data: &[u8]) -> [u8; 32] {
  const RATE: usize = 136;
  let mut state = [0u64; 25];
  let mut offset = 0;

  while offset + RATE <= data.len() {
    keccak_absorb_block(&mut state, &data[offset..offset + RATE]);
    keccakf1600(&mut state);
    offset += RATE;
  }

  let mut block = [0u8; RATE];
  let remaining = &data[offset..];
  block[..remaining.len()].copy_from_slice(remaining);
  block[remaining.len()] ^= 0x06;
  block[RATE - 1] ^= 0x80;
  keccak_absorb_block(&mut state, &block);
  keccakf1600(&mut state);

  let mut out = [0u8; 32];
  let mut produced = 0usize;
  while produced < out.len() {
    let lane_bytes = state[produced / 8].to_le_bytes();
    let take = (out.len() - produced).min(8);
    out[produced..produced + take].copy_from_slice(&lane_bytes[..take]);
    produced += take;
  }
  out
}

fn keccak_absorb_block(state: &mut [u64; 25], block: &[u8]) {
  for (lane, chunk) in block.chunks_exact(8).enumerate() {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(chunk);
    state[lane] ^= u64::from_le_bytes(bytes);
  }
}

fn keccakf1600(state: &mut [u64; 25]) {
  const RHO: [u32; 25] = [
    0, 1, 62, 28, 27, 36, 44, 6, 55, 20, 3, 10, 43, 25, 39, 41, 45, 15, 21, 8,
    18, 2, 61, 56, 14,
  ];
  const RC: [u64; 24] = [
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808a,
    0x8000000080008000,
    0x000000000000808b,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008a,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000a,
    0x000000008000808b,
    0x800000000000008b,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800a,
    0x800000008000000a,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
  ];

  for &rc in &RC {
    let mut c = [0u64; 5];
    for x in 0..5 {
      c[x] =
        state[x] ^ state[x + 5] ^ state[x + 10] ^ state[x + 15] ^ state[x + 20];
    }
    let mut d = [0u64; 5];
    for x in 0..5 {
      d[x] = c[(x + 4) % 5] ^ c[(x + 1) % 5].rotate_left(1);
    }
    for y in 0..5 {
      for x in 0..5 {
        state[x + 5 * y] ^= d[x];
      }
    }

    let mut b = [0u64; 25];
    for y in 0..5 {
      for x in 0..5 {
        let idx = x + 5 * y;
        let new_x = y;
        let new_y = (2 * x + 3 * y) % 5;
        b[new_x + 5 * new_y] = state[idx].rotate_left(RHO[idx]);
      }
    }

    for y in 0..5 {
      for x in 0..5 {
        state[x + 5 * y] =
          b[x + 5 * y] ^ ((!b[(x + 1) % 5 + 5 * y]) & b[(x + 2) % 5 + 5 * y]);
      }
    }

    state[0] ^= rc;
  }
}

pub fn parse_xargs_items(
  input: &str,
  separator: XargsSeparator,
) -> Vec<String> {
  match separator {
    XargsSeparator::Whitespace => {
      input.split_whitespace().map(str::to_string).collect()
    }
    XargsSeparator::Nul => input
      .split('\0')
      .filter(|item| !item.is_empty())
      .map(str::to_string)
      .collect(),
    XargsSeparator::Delim(ch) => input
      .split(ch)
      .filter(|item| !item.is_empty())
      .map(str::to_string)
      .collect(),
  }
}

pub fn build_xargs_groups(
  input: &str,
  separator: XargsSeparator,
  batch_size: Option<usize>,
  max_lines: Option<usize>,
  replace_token: Option<&str>,
  exact_size: bool,
) -> io::Result<Vec<Vec<String>>> {
  if replace_token.is_some() {
    return Ok(
      input
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| vec![line.to_string()])
        .collect(),
    );
  }

  if let Some(lines_per_command) = max_lines {
    let mut groups = Vec::new();
    let mut current = Vec::new();
    let mut used_lines = 0usize;
    for line in input.lines() {
      if line.is_empty() {
        continue;
      }
      current.extend(parse_xargs_items(line, separator));
      used_lines += 1;
      if used_lines >= lines_per_command {
        groups.push(std::mem::take(&mut current));
        used_lines = 0;
      }
    }
    if !current.is_empty() {
      groups.push(current);
    }
    return Ok(groups);
  }

  let items = parse_xargs_items(input, separator);
  if items.is_empty() {
    return Ok(Vec::new());
  }

  let chunk_size = batch_size.unwrap_or(items.len());
  if exact_size
    && batch_size.is_some()
    && items.len() > chunk_size
    && items.len() % chunk_size != 0
  {
    return Err(io::Error::new(
      io::ErrorKind::InvalidInput,
      "xargs: input item count is not an exact multiple of -n",
    ));
  }
  Ok(items.chunks(chunk_size).map(|chunk| chunk.to_vec()).collect())
}

pub fn read_yes_from_tty(lio: &Lio) -> io::Result<Option<char>> {
  Ok(
    read_key_from_tty(lio)?
      .and_then(|s| s.chars().find(|c| !c.is_whitespace())),
  )
}

pub fn tty_size() -> (usize, usize) {
  let tty_path = match CString::new("/dev/tty") {
    Ok(path) => path,
    Err(_) => return (24, 80),
  };
  let fd = unsafe { libc::open(tty_path.as_ptr(), libc::O_RDONLY) };
  if fd < 0 {
    return (24, 80);
  }

  let mut winsize =
    libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
  unsafe {
    let ok = libc::ioctl(fd, libc::TIOCGWINSZ, &mut winsize) == 0;
    libc::close(fd);
    if ok {
      (
        if winsize.ws_row > 0 { winsize.ws_row as usize } else { 24 },
        if winsize.ws_col > 0 { winsize.ws_col as usize } else { 80 },
      )
    } else {
      (24, 80)
    }
  }
}

pub fn read_key_from_tty(lio: &Lio) -> io::Result<Option<String>> {
  let _ = lio;
  let tty_path = CString::new("/dev/tty")?;
  let fd = unsafe { libc::open(tty_path.as_ptr(), libc::O_RDONLY) };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }

  let mut buf = [0u8; 64];
  let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
  let read_result =
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) };
  unsafe {
    libc::close(fd);
  }

  let n = read_result?;
  Ok(std::str::from_utf8(&buf[..n]).ok().map(ToOwned::to_owned))
}

pub fn read_key_from_tty_raw(lio: &Lio) -> io::Result<Option<String>> {
  let _ = lio;
  let tty_path = CString::new("/dev/tty")?;
  let fd = unsafe { libc::open(tty_path.as_ptr(), libc::O_RDONLY) };
  if fd < 0 {
    return Err(io::Error::last_os_error());
  }

  let original = get_termios(fd)?;
  let mut raw = original;
  raw.c_iflag &=
    !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON)
      as libc::tcflag_t;
  raw.c_oflag &= !(libc::OPOST as libc::tcflag_t);
  raw.c_cflag |= libc::CS8 as libc::tcflag_t;
  raw.c_lflag &=
    !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG) as libc::tcflag_t;
  raw.c_cc[libc::VMIN] = 1;
  raw.c_cc[libc::VTIME] = 0;
  set_termios(fd, &raw)?;

  let read_result = read_single_tty_key(fd);

  let restore_result = set_termios(fd, &original);
  unsafe {
    libc::close(fd);
  }
  restore_result?;
  read_result
}

fn read_single_tty_key(fd: i32) -> io::Result<Option<String>> {
  let mut buf = [0u8; 16];
  let used = read_tty_bytes(fd, &mut buf[..1])?;
  if used == 0 {
    return Ok(None);
  }

  if buf[0] != 0x1b {
    return Ok(std::str::from_utf8(&buf[..1]).ok().map(ToOwned::to_owned));
  }

  let mut used = 1usize;
  let mut idle_polls = 0usize;
  while used < buf.len() && idle_polls < 6 {
    if !poll_tty_readable(fd, 10)? {
      idle_polls += 1;
      continue;
    }
    idle_polls = 0;
    let n = read_tty_bytes(fd, &mut buf[used..used + 1])?;
    if n == 0 {
      break;
    }
    used += n;
    if is_escape_sequence_complete(&buf[..used]) {
      break;
    }
  }

  Ok(std::str::from_utf8(&buf[..used]).ok().map(ToOwned::to_owned))
}

fn is_escape_sequence_complete(buf: &[u8]) -> bool {
  if buf.len() <= 1 || buf[0] != 0x1b {
    return true;
  }

  match buf[1] {
    b'[' => buf[2..].last().is_some_and(|byte| (0x40..=0x7e).contains(byte)),
    b'O' => buf.len() >= 3,
    _ => true,
  }
}

fn read_tty_bytes(fd: i32, buf: &mut [u8]) -> io::Result<usize> {
  let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
  if n < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(n as usize)
}

fn poll_tty_readable(fd: i32, timeout_ms: i32) -> io::Result<bool> {
  let mut poll_fd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
  let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };
  if ready < 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(ready > 0 && (poll_fd.revents & libc::POLLIN) != 0)
}

fn get_termios(fd: i32) -> io::Result<libc::termios> {
  let mut termios = unsafe { std::mem::zeroed::<libc::termios>() };
  if unsafe { libc::tcgetattr(fd, &mut termios) } != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(termios)
}

fn set_termios(fd: i32, termios: &libc::termios) -> io::Result<()> {
  if unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) } != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}
