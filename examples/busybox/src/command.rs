use std::io;

use crate::app::AppContext;

pub trait Command: Sized + 'static {
  fn name() -> &'static str;

  fn aliases() -> &'static [&'static str] {
    &[]
  }

  fn summary() -> &'static str;

  fn usage() -> &'static str;

  fn parse(args: &[String]) -> io::Result<Self>;

  fn execute(&self, ctx: &AppContext) -> io::Result<()>;

  fn registration() -> Registration {
    Registration::of::<Self>()
  }
}

pub struct Registration {
  name: &'static str,
  aliases: &'static [&'static str],
  summary: &'static str,
  usage: &'static str,
  parse_and_execute: fn(&AppContext, &[String]) -> io::Result<()>,
}

impl Registration {
  pub fn of<C: Command>() -> Self {
    Self {
      name: C::name(),
      aliases: C::aliases(),
      summary: C::summary(),
      usage: C::usage(),
      parse_and_execute: |ctx, args| C::parse(args)?.execute(ctx),
    }
  }

  pub fn name(&self) -> &'static str {
    self.name
  }

  pub fn aliases(&self) -> &'static [&'static str] {
    self.aliases
  }

  pub fn summary(&self) -> &'static str {
    self.summary
  }

  pub fn usage(&self) -> &'static str {
    self.usage
  }

  pub fn parse_and_execute(
    &self,
    ctx: &AppContext,
    args: &[String],
  ) -> io::Result<()> {
    (self.parse_and_execute)(ctx, args)
  }
}
