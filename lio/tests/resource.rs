mod common;

use common::{file_resource, temp_path};

#[test]
fn will_close() {
  let path = temp_path("close_basic.txt");

  // Open a file
  let resource = file_resource(std::fs::File::create(&path).unwrap());

  assert!(resource.will_close());

  let _r2 = resource.clone();

  assert!(!resource.will_close());
  assert!(!_r2.will_close());
  drop(_r2);

  assert!(resource.will_close());

  drop(resource);

  // Cleanup
  std::fs::remove_file(&path).ok();
}
