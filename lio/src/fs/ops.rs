// pub(crate) struct OpenatFile(ops::OpenAt);
//
// impl OperationExt for OpenatFile {
//   type Result = io::Result<File>;
// }
//
// impl Operation for OpenatFile {
//   fn run_blocking(&self) -> isize {
//     self.0.run_blocking()
//   }
//   fn meta(&self) -> OpMeta {
//     self.0.meta()
//   }
//   fn result(&mut self, ret: isize) -> *const () {
//     let ptr =
//       self.0.result(ret) as *const <ops::Socket as OperationExt>::Result;
//
//     // SAFETY: The pointer was just created by Accept::result and points to valid Accept::Result
//     let accept_result = unsafe { ptr.read() };
//
//     let socket_result = accept_result.map(File::from_resource);
//
//     // Box the result and return pointer
//     Box::into_raw(Box::new(socket_result)) as *const ()
//   }
//   fn cap(&self) -> i32 {
//     self.0.cap()
//   }
//
//   #[cfg(linux)]
//   fn create_entry(&self) -> io_uring::squeue::Entry {
//     self.0.create_entry()
//   }
// }
//
