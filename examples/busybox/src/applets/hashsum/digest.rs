use std::{ffi::CString, io};

use lio::{
  Lio,
  api::{self, resource::Resource},
};

use crate::util::io as io_util;

pub fn stream_digest_command<S, Init, Update, Finalize>(
  lio: &Lio,
  files: &[String],
  init: Init,
  update: Update,
  finalize: Finalize,
) -> io::Result<()>
where
  Init: Fn() -> S,
  Update: Fn(&mut S, &[u8]),
  Finalize: Fn(S) -> String,
{
  if files.is_empty() {
    let digest = digest_resource_streaming(
      lio,
      &Resource::stdin(),
      &init,
      &update,
      &finalize,
    )?;
    return io_util::write_all(
      lio,
      &Resource::stdout(),
      format!("{digest}  -\n").into_bytes(),
    );
  }

  for path in files {
    let fd = io_util::run(
      lio,
      api::openat(
        &Resource::cwd(),
        CString::new(path.as_str())?,
        libc::O_RDONLY,
        0,
      )
      .with_lio(lio)
      .send(),
    )?;
    let digest =
      digest_resource_streaming(lio, &fd, &init, &update, &finalize)?;
    io_util::write_all(
      lio,
      &Resource::stdout(),
      format!("{digest}  {path}\n").into_bytes(),
    )?;
  }

  Ok(())
}

fn digest_resource_streaming<S, Init, Update, Finalize>(
  lio: &Lio,
  fd: &Resource,
  init: &Init,
  update: &Update,
  finalize: &Finalize,
) -> io::Result<String>
where
  Init: Fn() -> S,
  Update: Fn(&mut S, &[u8]),
  Finalize: Fn(S) -> String,
{
  let mut state = init();
  let mut buf = vec![0u8; 8192];
  loop {
    let rx = api::read(fd, buf).with_lio(lio).send();
    let (result, returned_buf) = io_util::run(lio, rx);
    buf = returned_buf;
    let n = result? as usize;
    if n == 0 {
      break;
    }
    update(&mut state, &buf[..n]);
  }
  Ok(finalize(state))
}

pub fn hex_digest(bytes: &[u8]) -> String {
  let mut out = String::with_capacity(bytes.len() * 2);
  for byte in bytes {
    out.push_str(&format!("{byte:02x}"));
  }
  out
}

#[cfg(test)]
pub fn md5_digest(data: &[u8]) -> [u8; 16] {
  let mut state = Md5State::new();
  state.update(data);
  state.finalize()
}

pub struct Md5State {
  state: [u32; 4],
  buffer: [u8; 64],
  buffer_len: usize,
  len_bytes: u64,
}

const MD5_S: [u32; 64] = [
  7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5,
  9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11,
  16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10,
  15, 21,
];
const MD5_K: [u32; 64] = [
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

impl Md5State {
  pub fn new() -> Self {
    Self {
      state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476],
      buffer: [0; 64],
      buffer_len: 0,
      len_bytes: 0,
    }
  }

  pub fn update(&mut self, mut data: &[u8]) {
    self.len_bytes = self.len_bytes.wrapping_add(data.len() as u64);

    if self.buffer_len > 0 {
      let take = (64 - self.buffer_len).min(data.len());
      self.buffer[self.buffer_len..self.buffer_len + take]
        .copy_from_slice(&data[..take]);
      self.buffer_len += take;
      data = &data[take..];
      if self.buffer_len == 64 {
        md5_process_block(&mut self.state, &self.buffer);
        self.buffer_len = 0;
      }
    }

    while data.len() >= 64 {
      md5_process_block(&mut self.state, &data[..64]);
      data = &data[64..];
    }

    if !data.is_empty() {
      self.buffer[..data.len()].copy_from_slice(data);
      self.buffer_len = data.len();
    }
  }

  pub fn finalize(mut self) -> [u8; 16] {
    let bit_len = self.len_bytes.wrapping_mul(8);
    self.buffer[self.buffer_len] = 0x80;
    self.buffer_len += 1;

    if self.buffer_len > 56 {
      self.buffer[self.buffer_len..].fill(0);
      md5_process_block(&mut self.state, &self.buffer);
      self.buffer_len = 0;
    }

    self.buffer[self.buffer_len..56].fill(0);
    self.buffer[56..64].copy_from_slice(&bit_len.to_le_bytes());
    md5_process_block(&mut self.state, &self.buffer);

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&self.state[0].to_le_bytes());
    out[4..8].copy_from_slice(&self.state[1].to_le_bytes());
    out[8..12].copy_from_slice(&self.state[2].to_le_bytes());
    out[12..16].copy_from_slice(&self.state[3].to_le_bytes());
    out
  }
}

fn md5_process_block(state: &mut [u32; 4], chunk: &[u8]) {
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

  let mut a = state[0];
  let mut b = state[1];
  let mut c = state[2];
  let mut d = state[3];

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
        .wrapping_add(MD5_K[i])
        .wrapping_add(m[g])
        .rotate_left(MD5_S[i]),
    );
    a = tmp;
  }

  state[0] = state[0].wrapping_add(a);
  state[1] = state[1].wrapping_add(b);
  state[2] = state[2].wrapping_add(c);
  state[3] = state[3].wrapping_add(d);
}

#[cfg(test)]
pub fn sha1_digest(data: &[u8]) -> [u8; 20] {
  let mut state = Sha1State::new();
  state.update(data);
  state.finalize()
}

pub struct Sha1State {
  state: [u32; 5],
  buffer: [u8; 64],
  buffer_len: usize,
  len_bytes: u64,
}

impl Sha1State {
  pub fn new() -> Self {
    Self {
      state: [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476, 0xc3d2e1f0],
      buffer: [0; 64],
      buffer_len: 0,
      len_bytes: 0,
    }
  }

  pub fn update(&mut self, mut data: &[u8]) {
    self.len_bytes = self.len_bytes.wrapping_add(data.len() as u64);
    if self.buffer_len > 0 {
      let take = (64 - self.buffer_len).min(data.len());
      self.buffer[self.buffer_len..self.buffer_len + take]
        .copy_from_slice(&data[..take]);
      self.buffer_len += take;
      data = &data[take..];
      if self.buffer_len == 64 {
        sha1_process_block(&mut self.state, &self.buffer);
        self.buffer_len = 0;
      }
    }
    while data.len() >= 64 {
      sha1_process_block(&mut self.state, &data[..64]);
      data = &data[64..];
    }
    if !data.is_empty() {
      self.buffer[..data.len()].copy_from_slice(data);
      self.buffer_len = data.len();
    }
  }

  pub fn finalize(mut self) -> [u8; 20] {
    let bit_len = self.len_bytes.wrapping_mul(8);
    self.buffer[self.buffer_len] = 0x80;
    self.buffer_len += 1;
    if self.buffer_len > 56 {
      self.buffer[self.buffer_len..].fill(0);
      sha1_process_block(&mut self.state, &self.buffer);
      self.buffer_len = 0;
    }
    self.buffer[self.buffer_len..56].fill(0);
    self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
    sha1_process_block(&mut self.state, &self.buffer);

    let mut out = [0u8; 20];
    out[0..4].copy_from_slice(&self.state[0].to_be_bytes());
    out[4..8].copy_from_slice(&self.state[1].to_be_bytes());
    out[8..12].copy_from_slice(&self.state[2].to_be_bytes());
    out[12..16].copy_from_slice(&self.state[3].to_be_bytes());
    out[16..20].copy_from_slice(&self.state[4].to_be_bytes());
    out
  }
}

fn sha1_process_block(state: &mut [u32; 5], chunk: &[u8]) {
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

  let mut a = state[0];
  let mut b = state[1];
  let mut c = state[2];
  let mut d = state[3];
  let mut e = state[4];

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

  state[0] = state[0].wrapping_add(a);
  state[1] = state[1].wrapping_add(b);
  state[2] = state[2].wrapping_add(c);
  state[3] = state[3].wrapping_add(d);
  state[4] = state[4].wrapping_add(e);
}

#[cfg(test)]
pub fn sha256_digest(data: &[u8]) -> [u8; 32] {
  let mut state = Sha256State::new();
  state.update(data);
  state.finalize()
}

const SHA256_K: [u32; 64] = [
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

pub struct Sha256State {
  state: [u32; 8],
  buffer: [u8; 64],
  buffer_len: usize,
  len_bytes: u64,
}

impl Sha256State {
  pub fn new() -> Self {
    Self {
      state: [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
        0x1f83d9ab, 0x5be0cd19,
      ],
      buffer: [0; 64],
      buffer_len: 0,
      len_bytes: 0,
    }
  }

  pub fn update(&mut self, mut data: &[u8]) {
    self.len_bytes = self.len_bytes.wrapping_add(data.len() as u64);
    if self.buffer_len > 0 {
      let take = (64 - self.buffer_len).min(data.len());
      self.buffer[self.buffer_len..self.buffer_len + take]
        .copy_from_slice(&data[..take]);
      self.buffer_len += take;
      data = &data[take..];
      if self.buffer_len == 64 {
        sha256_process_block(&mut self.state, &self.buffer);
        self.buffer_len = 0;
      }
    }
    while data.len() >= 64 {
      sha256_process_block(&mut self.state, &data[..64]);
      data = &data[64..];
    }
    if !data.is_empty() {
      self.buffer[..data.len()].copy_from_slice(data);
      self.buffer_len = data.len();
    }
  }

  pub fn finalize(mut self) -> [u8; 32] {
    let bit_len = self.len_bytes.wrapping_mul(8);
    self.buffer[self.buffer_len] = 0x80;
    self.buffer_len += 1;
    if self.buffer_len > 56 {
      self.buffer[self.buffer_len..].fill(0);
      sha256_process_block(&mut self.state, &self.buffer);
      self.buffer_len = 0;
    }
    self.buffer[self.buffer_len..56].fill(0);
    self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
    sha256_process_block(&mut self.state, &self.buffer);

    let mut out = [0u8; 32];
    for (i, word) in self.state.iter().enumerate() {
      out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
    }
    out
  }
}

fn sha256_process_block(state: &mut [u32; 8], chunk: &[u8]) {
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
    let s0 =
      w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
    let s1 =
      w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
    w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
  }

  let mut a = state[0];
  let mut b = state[1];
  let mut c = state[2];
  let mut d = state[3];
  let mut e = state[4];
  let mut f = state[5];
  let mut g = state[6];
  let mut hh = state[7];

  for i in 0..64 {
    let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
    let ch = (e & f) ^ ((!e) & g);
    let temp1 = hh
      .wrapping_add(s1)
      .wrapping_add(ch)
      .wrapping_add(SHA256_K[i])
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

  state[0] = state[0].wrapping_add(a);
  state[1] = state[1].wrapping_add(b);
  state[2] = state[2].wrapping_add(c);
  state[3] = state[3].wrapping_add(d);
  state[4] = state[4].wrapping_add(e);
  state[5] = state[5].wrapping_add(f);
  state[6] = state[6].wrapping_add(g);
  state[7] = state[7].wrapping_add(hh);
}

#[cfg(test)]
pub fn sha512_digest(data: &[u8]) -> [u8; 64] {
  let mut state = Sha512State::new();
  state.update(data);
  state.finalize()
}

const SHA512_K: [u64; 80] = [
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

pub struct Sha512State {
  state: [u64; 8],
  buffer: [u8; 128],
  buffer_len: usize,
  len_bytes: u128,
}

impl Sha512State {
  pub fn new() -> Self {
    Self {
      state: [
        0x6a09e667f3bcc908,
        0xbb67ae8584caa73b,
        0x3c6ef372fe94f82b,
        0xa54ff53a5f1d36f1,
        0x510e527fade682d1,
        0x9b05688c2b3e6c1f,
        0x1f83d9abfb41bd6b,
        0x5be0cd19137e2179,
      ],
      buffer: [0; 128],
      buffer_len: 0,
      len_bytes: 0,
    }
  }

  pub fn update(&mut self, mut data: &[u8]) {
    self.len_bytes = self.len_bytes.wrapping_add(data.len() as u128);
    if self.buffer_len > 0 {
      let take = (128 - self.buffer_len).min(data.len());
      self.buffer[self.buffer_len..self.buffer_len + take]
        .copy_from_slice(&data[..take]);
      self.buffer_len += take;
      data = &data[take..];
      if self.buffer_len == 128 {
        sha512_process_block(&mut self.state, &self.buffer);
        self.buffer_len = 0;
      }
    }
    while data.len() >= 128 {
      sha512_process_block(&mut self.state, &data[..128]);
      data = &data[128..];
    }
    if !data.is_empty() {
      self.buffer[..data.len()].copy_from_slice(data);
      self.buffer_len = data.len();
    }
  }

  pub fn finalize(mut self) -> [u8; 64] {
    let bit_len = self.len_bytes.wrapping_mul(8);
    self.buffer[self.buffer_len] = 0x80;
    self.buffer_len += 1;
    if self.buffer_len > 112 {
      self.buffer[self.buffer_len..].fill(0);
      sha512_process_block(&mut self.state, &self.buffer);
      self.buffer_len = 0;
    }
    self.buffer[self.buffer_len..112].fill(0);
    self.buffer[112..128].copy_from_slice(&bit_len.to_be_bytes());
    sha512_process_block(&mut self.state, &self.buffer);

    let mut out = [0u8; 64];
    for (i, word) in self.state.iter().enumerate() {
      out[i * 8..(i + 1) * 8].copy_from_slice(&word.to_be_bytes());
    }
    out
  }
}

fn sha512_process_block(state: &mut [u64; 8], chunk: &[u8]) {
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
    let s0 =
      w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
    let s1 =
      w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
    w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
  }

  let mut a = state[0];
  let mut b = state[1];
  let mut c = state[2];
  let mut d = state[3];
  let mut e = state[4];
  let mut f = state[5];
  let mut g = state[6];
  let mut hh = state[7];

  for i in 0..80 {
    let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
    let ch = (e & f) ^ ((!e) & g);
    let temp1 = hh
      .wrapping_add(s1)
      .wrapping_add(ch)
      .wrapping_add(SHA512_K[i])
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

  state[0] = state[0].wrapping_add(a);
  state[1] = state[1].wrapping_add(b);
  state[2] = state[2].wrapping_add(c);
  state[3] = state[3].wrapping_add(d);
  state[4] = state[4].wrapping_add(e);
  state[5] = state[5].wrapping_add(f);
  state[6] = state[6].wrapping_add(g);
  state[7] = state[7].wrapping_add(hh);
}

#[cfg(test)]
pub fn sha3_256_digest(data: &[u8]) -> [u8; 32] {
  let mut state = Sha3_256State::new();
  state.update(data);
  state.finalize()
}

pub struct Sha3_256State {
  state: [u64; 25],
  buffer: [u8; 136],
  buffer_len: usize,
}

impl Sha3_256State {
  pub fn new() -> Self {
    Self { state: [0; 25], buffer: [0; 136], buffer_len: 0 }
  }

  pub fn update(&mut self, mut data: &[u8]) {
    if self.buffer_len > 0 {
      let take = (136 - self.buffer_len).min(data.len());
      self.buffer[self.buffer_len..self.buffer_len + take]
        .copy_from_slice(&data[..take]);
      self.buffer_len += take;
      data = &data[take..];
      if self.buffer_len == 136 {
        keccak_absorb_block(&mut self.state, &self.buffer);
        keccakf1600(&mut self.state);
        self.buffer_len = 0;
      }
    }

    while data.len() >= 136 {
      keccak_absorb_block(&mut self.state, &data[..136]);
      keccakf1600(&mut self.state);
      data = &data[136..];
    }

    if !data.is_empty() {
      self.buffer[..data.len()].copy_from_slice(data);
      self.buffer_len = data.len();
    }
  }

  pub fn finalize(mut self) -> [u8; 32] {
    let mut block = [0u8; 136];
    block[..self.buffer_len].copy_from_slice(&self.buffer[..self.buffer_len]);
    block[self.buffer_len] ^= 0x06;
    block[135] ^= 0x80;
    keccak_absorb_block(&mut self.state, &block);
    keccakf1600(&mut self.state);

    let mut out = [0u8; 32];
    let mut produced = 0usize;
    while produced < out.len() {
      let lane_bytes = self.state[produced / 8].to_le_bytes();
      let take = (out.len() - produced).min(8);
      out[produced..produced + take].copy_from_slice(&lane_bytes[..take]);
      produced += take;
    }
    out
  }
}

fn keccak_absorb_block(state: &mut [u64; 25], block: &[u8]) {
  for (lane, chunk) in block.as_chunks::<8>().0.iter().enumerate() {
    state[lane] ^= u64::from_le_bytes(*chunk);
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
