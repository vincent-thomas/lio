use std::io;

use lio::{Lio, api::resource::Resource};

pub struct AppContext {
  lio: Lio,
}

impl AppContext {
  pub fn new() -> io::Result<Self> {
    Ok(Self { lio: Lio::new(64)? })
  }

  pub fn lio(&self) -> &Lio {
    &self.lio
  }

  pub fn stdin(&self) -> Resource {
    Resource::stdin()
  }

  pub fn stdout(&self) -> Resource {
    Resource::stdout()
  }

  pub fn stderr(&self) -> Resource {
    Resource::stderr()
  }

  pub fn cwd(&self) -> Resource {
    Resource::cwd()
  }
}
