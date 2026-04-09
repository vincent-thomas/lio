use crate::{
  applets,
  command::{Command, Registration},
};

pub struct Registry {
  commands: Vec<Registration>,
}

impl Registry {
  pub fn new() -> Self {
    let commands = vec![
      applets::cat::CatCommand::registration(),
      applets::cp::CpCommand::registration(),
      applets::less::LessCommand::registration(),
      applets::ls::LsCommand::registration(),
      applets::ln::LnCommand::registration(),
      applets::mkdir::MkdirCommand::registration(),
      applets::mktemp::MktempCommand::registration(),
      applets::mv::MvCommand::registration(),
      applets::dd::DdCommand::registration(),
      applets::touch::TouchCommand::registration(),
      applets::tee::TeeCommand::registration(),
      applets::sleep::SleepCommand::registration(),
      applets::clear::ClearCommand::registration(),
      applets::printf::PrintfCommand::registration(),
      applets::pwd::PwdCommand::registration(),
      applets::readlink::ReadlinkCommand::registration(),
      applets::realpath::RealpathCommand::registration(),
      applets::seq::SeqCommand::registration(),
      applets::sed::SedCommand::registration(),
      applets::echo::EchoCommand::registration(),
      #[cfg(feature = "fd")]
      applets::fd::FdCommand::registration(),
      #[cfg(feature = "rg")]
      applets::grep::GrepCommand::registration(),
      applets::dirname::DirnameCommand::registration(),
      applets::basename::BasenameCommand::registration(),
      applets::head::HeadCommand::registration(),
      applets::tail::TailCommand::registration(),
      applets::wc::WcCommand::registration(),
      applets::yes::YesCommand::registration(),
      applets::paste::PasteCommand::registration(),
      applets::uniq::UniqCommand::registration(),
      applets::cut::CutCommand::registration(),
      applets::nl::NlCommand::registration(),
      applets::tac::TacCommand::registration(),
      applets::cmp::CmpCommand::registration(),
      applets::tr::TrCommand::registration(),
      applets::unlink::UnlinkCommand::registration(),
      applets::rev::RevCommand::registration(),
      applets::rm::RmCommand::registration(),
      applets::rmdir::RmdirCommand::registration(),
      applets::cksum::CksumCommand::registration(),
      applets::fold::FoldCommand::registration(),
      applets::hexdump::HexdumpCommand::registration(),
      #[cfg(feature = "jq")]
      applets::jq::JqCommand::registration(),
      #[cfg(feature = "yq")]
      applets::yq::YqCommand::registration(),
      #[cfg(feature = "rg")]
      applets::rg::RgCommand::registration(),
      applets::od::OdCommand::registration(),
      applets::strings::StringsCommand::registration(),
      applets::sort::SortCommand::registration(),
      applets::more::MoreCommand::registration(),
      applets::comm::CommCommand::registration(),
      applets::base64::Base64Command::registration(),
      applets::base32::Base32Command::registration(),
      #[cfg(feature = "hashsum")]
      applets::Md5sumCommand::registration(),
      #[cfg(feature = "hashsum")]
      applets::Sha1sumCommand::registration(),
      #[cfg(feature = "hashsum")]
      applets::Sha256sumCommand::registration(),
      #[cfg(feature = "hashsum")]
      applets::Sha512sumCommand::registration(),
      #[cfg(feature = "hashsum")]
      applets::Sha3sumCommand::registration(),
      applets::test::TestCommand::registration(),
      applets::timeout::TimeoutCommand::registration(),
      applets::watch::WatchCommand::registration(),
      applets::xargs::XargsCommand::registration(),
    ];
    Self { commands }
  }

  pub fn find(&self, name: &str) -> Option<&Registration> {
    self.commands.iter().find(|command| {
      command.name() == name || command.aliases().contains(&name)
    })
  }

  pub fn commands(&self) -> &[Registration] {
    &self.commands
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn finds_registered_command_by_name() {
    let registry = Registry::new();
    let command = registry.find("cat").expect("cat should be registered");
    assert_eq!(command.name(), "cat");
  }

  #[test]
  fn finds_migrated_legacy_command_by_name() {
    let registry = Registry::new();
    let command = registry.find("printf").expect("printf should be registered");
    assert_eq!(command.name(), "printf");
  }

  #[test]
  fn finds_reimplemented_basic_command_by_name() {
    let registry = Registry::new();
    let command = registry.find("echo").expect("echo should be registered");
    assert_eq!(command.name(), "echo");
  }

  #[test]
  fn returns_none_for_unknown_command() {
    let registry = Registry::new();
    assert!(registry.find("missing").is_none());
  }

  #[test]
  fn finds_registered_command_by_alias() {
    let registry = Registry::new();
    let command = registry.find("[").expect("[ should be registered");
    assert_eq!(command.name(), "test");
  }
}
