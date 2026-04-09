use std::io;

use lio::api;

use crate::{
  app::AppContext,
  applets::support::{expand_tr_set, interpret_backslash_escapes},
  command::Command,
  util::{
    flags::{FlagParser, FlagSpec},
    io as io_util,
  },
};

#[derive(Debug, Clone, Default)]
pub struct TrCommand {
  pub delete: bool,
  pub squeeze: bool,
  pub set1: Vec<u8>,
  pub set2: Vec<u8>,
}

impl Command for TrCommand {
  fn name() -> &'static str {
    "tr"
  }
  fn summary() -> &'static str {
    "Translate or delete characters."
  }
  fn usage() -> &'static str {
    "tr [-d] [-s] <set1> [set2]"
  }
  fn parse(args: &[String]) -> io::Result<Self> {
    const SPECS: &[FlagSpec<'static>] = &[
      FlagSpec { name: "delete", short: &['d'], long: &[], takes_value: false },
      FlagSpec {
        name: "squeeze",
        short: &['s'],
        long: &[],
        takes_value: false,
      },
    ];
    let parsed = FlagParser::new("tr", SPECS).parse(args)?;
    let Some(set1_raw) = parsed.positional().first() else {
      return Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        "tr: missing set1",
      ));
    };
    let set1 = expand_tr_set(&interpret_backslash_escapes(set1_raw));
    let delete = parsed.get_flag_exists("delete");
    let set2 = if delete {
      Vec::new()
    } else {
      let Some(raw) = parsed.positional().get(1) else {
        return Err(io::Error::new(
          io::ErrorKind::InvalidInput,
          "tr: missing set2",
        ));
      };
      expand_tr_set(&interpret_backslash_escapes(raw))
    };
    Ok(Self { delete, squeeze: parsed.get_flag_exists("squeeze"), set1, set2 })
  }
  fn execute(&self, ctx: &AppContext) -> io::Result<()> {
    let plan = build_tr_plan(self);
    let stdin = ctx.stdin();
    let stdout = ctx.stdout();
    let mut buf = vec![0u8; 8192];
    let mut output = Vec::with_capacity(8192);
    let mut last_output = None;

    loop {
      let rx = api::read(&stdin, buf).with_lio(ctx.lio()).send();
      let (result, returned_buf) = io_util::run(ctx.lio(), rx);
      buf = returned_buf;
      let n = result? as usize;
      if n == 0 {
        break;
      }

      output.clear();
      for &byte in &buf[..n] {
        let transformed = match plan.map[byte as usize] {
          TrMapping::Delete => continue,
          TrMapping::Emit(value) => value,
          TrMapping::Identity => byte,
        };
        if plan.squeeze[transformed as usize]
          && last_output == Some(transformed)
        {
          continue;
        }
        output.push(transformed);
        last_output = Some(transformed);
      }

      if !output.is_empty() {
        io_util::write_all(ctx.lio(), &stdout, output.clone())?;
      }
    }

    Ok(())
  }
}

#[derive(Clone, Copy)]
enum TrMapping {
  Identity,
  Emit(u8),
  Delete,
}

struct TrPlan {
  map: [TrMapping; 256],
  squeeze: [bool; 256],
}

fn build_tr_plan(command: &TrCommand) -> TrPlan {
  let mut map = [TrMapping::Identity; 256];
  for (index, &byte) in command.set1.iter().enumerate() {
    map[byte as usize] = if command.delete {
      TrMapping::Delete
    } else {
      let transformed = command
        .set2
        .get(index)
        .copied()
        .or_else(|| command.set2.last().copied())
        .unwrap_or(byte);
      TrMapping::Emit(transformed)
    };
  }

  let mut squeeze = [false; 256];
  if command.squeeze {
    let set = if command.delete { &command.set1 } else { &command.set2 };
    for &byte in set {
      squeeze[byte as usize] = true;
    }
  }

  TrPlan { map, squeeze }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn build_tr_plan_maps_and_squeezes_in_constant_time() {
    let command = TrCommand {
      squeeze: true,
      set1: vec![b'a', b'b'],
      set2: vec![b'x'],
      ..Default::default()
    };
    let plan = build_tr_plan(&command);
    assert!(matches!(plan.map[b'a' as usize], TrMapping::Emit(b'x')));
    assert!(matches!(plan.map[b'b' as usize], TrMapping::Emit(b'x')));
    assert!(plan.squeeze[b'x' as usize]);
  }
}
