//! The command line both runners take, a subset of `wasmtime run`'s.

pub struct Args {
  /// `--dir HOST::GUEST`
  pub dirs: Vec<(String, String)>,
  /// `--env NAME=VALUE`
  pub env: Vec<(String, String)>,
  /// `--inherit-network`
  pub network: bool,
  /// `--allow-ip-name-lookup`
  pub name_lookup: bool,
  /// `--blocking`: the comparison host does file I/O on the calling thread.
  pub blocking: bool,
  /// `--stats`: print how stream reads and writes were done.
  pub stats: bool,
  pub wasm: String,
  /// The guest's arguments, the program first.
  pub guest: Vec<String>,
}

pub fn parse() -> Result<Args, String> {
  let mut args = Args {
    dirs: Vec::new(),
    env: Vec::new(),
    network: false,
    name_lookup: false,
    blocking: false,
    stats: false,
    wasm: String::new(),
    guest: Vec::new(),
  };
  let mut given = std::env::args().skip(1);
  while let Some(arg) = given.next() {
    match arg.as_str() {
      "--dir" => {
        let dir = given.next().ok_or("--dir takes HOST::GUEST")?;
        let (host, guest) = dir.split_once("::").unwrap_or((&dir, &dir));
        args.dirs.push((host.to_owned(), guest.to_owned()));
      }
      "--env" => {
        let var = given.next().ok_or("--env takes NAME=VALUE")?;
        let (name, value) =
          var.split_once('=').ok_or("--env takes NAME=VALUE")?;
        args.env.push((name.to_owned(), value.to_owned()));
      }
      "--inherit-network" => args.network = true,
      "--allow-ip-name-lookup" => args.name_lookup = true,
      "--blocking" => args.blocking = true,
      "--stats" => args.stats = true,
      flag if flag.starts_with("--") => {
        return Err(format!("unknown flag {flag}"));
      }
      _ => {
        args.wasm = arg.clone();
        args.guest.push(arg);
        args.guest.extend(given);
        return Ok(args);
      }
    }
  }
  Err(String::from(
    "usage: [--dir HOST::GUEST] [--env NAME=VALUE] [--inherit-network] [--allow-ip-name-lookup] COMPONENT [ARGS...]",
  ))
}

pub fn config() -> wasmtime::Result<wasmtime::Config> {
  let mut config = wasmtime::Config::new();
  config.wasm_component_model_async(true);
  // Both runners skip compiling a component they have compiled before.
  config.cache(Some(wasmtime::Cache::from_file(None)?));
  Ok(config)
}
