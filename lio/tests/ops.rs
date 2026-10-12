mod ops {
  mod accept;
  mod bind;
  mod common;
  mod connect;
  mod getcwd;
  mod interval;
  mod linkat;
  mod listen;
  mod mkdirat;
  mod openat;
  mod readdir;
  mod readlinkat;
  mod recv;
  mod recvfrom;
  mod renameat;
  mod rw;
  mod send;
  mod sendto;
  mod shutdown;
  mod sleep;
  mod socket;
  #[cfg(unix)]
  mod spawn;
  mod unlinkat;
}
