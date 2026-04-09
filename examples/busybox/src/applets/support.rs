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
  cksum_crc32_finalize(cksum_crc32_update(0, data), data.len() as u64)
}

pub fn cksum_crc32_update(mut crc: u32, data: &[u8]) -> u32 {
  for &byte in data {
    crc = cksum_crc32_step(crc, byte);
  }
  crc
}

pub fn cksum_crc32_finalize(mut crc: u32, mut len: u64) -> u32 {
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

fn write_wrapped_chunk(
  lio: &Lio,
  output: &Resource,
  encoded: &[u8],
  width: usize,
  line_len: &mut usize,
) -> io::Result<()> {
  let mut out = Vec::with_capacity(encoded.len() + encoded.len() / width + 1);
  for &byte in encoded {
    if *line_len == width {
      out.push(b'\n');
      *line_len = 0;
    }
    out.push(byte);
    *line_len += 1;
  }
  if !out.is_empty() {
    io_util::write_all(lio, output, out)?;
  }
  Ok(())
}

pub fn stream_base64_output(
  lio: &Lio,
  input: &Resource,
  output: &Resource,
  width: usize,
) -> io::Result<()> {
  stream_encoded_output(lio, input, output, width, 3, encode_base64)
}

pub fn stream_base32_output(
  lio: &Lio,
  input: &Resource,
  output: &Resource,
  width: usize,
) -> io::Result<()> {
  stream_encoded_output(lio, input, output, width, 5, encode_base32)
}

fn stream_encoded_output<F>(
  lio: &Lio,
  input: &Resource,
  output: &Resource,
  width: usize,
  group_size: usize,
  encode: F,
) -> io::Result<()>
where
  F: Fn(&[u8]) -> String,
{
  let mut buf = vec![0u8; 8192];
  let mut carry = Vec::new();
  let mut line_len = 0usize;
  let mut wrote_any = false;

  loop {
    let rx = api::read(input, buf).with_lio(lio).send();
    let (result, returned_buf) = io_util::run(lio, rx);
    buf = returned_buf;
    let n = result? as usize;
    if n == 0 {
      break;
    }

    carry.extend_from_slice(&buf[..n]);
    let usable = carry.len() / group_size * group_size;
    if usable > 0 {
      let encoded = encode(&carry[..usable]);
      write_wrapped_chunk(
        lio,
        output,
        encoded.as_bytes(),
        width,
        &mut line_len,
      )?;
      carry.drain(..usable);
      wrote_any = true;
    }
  }

  if !carry.is_empty() {
    let encoded = encode(&carry);
    write_wrapped_chunk(lio, output, encoded.as_bytes(), width, &mut line_len)?;
    wrote_any = true;
  }

  if !wrote_any || line_len > 0 {
    io_util::write_all(lio, output, b"\n".to_vec())?;
  }

  Ok(())
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
  raw.c_cflag |= libc::CS8 as libc::tcflag_t;
  raw.c_lflag &=
    !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG) as libc::tcflag_t;
  raw.c_cc[libc::VMIN] = 0;
  raw.c_cc[libc::VTIME] = 1;
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
  let mut used = read_tty_bytes(fd, &mut buf[..1])?;
  if used == 0 {
    return Ok(None);
  }

  if buf[0] != 0x1b {
    return Ok(std::str::from_utf8(&buf[..1]).ok().map(ToOwned::to_owned));
  }

  while used < buf.len() {
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
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
      return Ok(0);
    }
    return Err(err);
  }
  Ok(n as usize)
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
