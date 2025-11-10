use std::env;

fn main() {
  println!(
    "cargo:rustc-link-arg=-Wl,--error-handling-script={}",
    env::current_exe().unwrap().display()
  );
  println!("cargo:rustc-link-arg=-Tdefmt.x");
  // make sure linkall.x is the last linker script (otherwise might cause problems with flip-link)
  println!("cargo:rustc-link-arg=-Tlinkall.x");
}
