//! Standalone CLI fixture: exercise the real runner without daemon work.
#![allow(dead_code)]

#[path = "../common/kit.rs"]
mod kit;

fn command(_: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    panic!("fixture tests must not spawn processes")
}

fn helper_main() {
    if std::env::args().nth(1).as_deref() == Some("fixture-helper") {
        println!("helper dispatched");
        std::process::exit(0);
    }
}

fn enter_test_scope(_: usize) {}
fn end_test_scope() {}

fn alpha() {
    println!("ran alpha");
}

fn alphabet() {
    println!("ran alphabet");
}

fn beta() {
    println!("ran beta");
}

fn fails() {
    panic!("fixture failure");
}

fn main() {
    kit::main(&[
        ("alpha", alpha),
        ("alphabet", alphabet),
        ("beta", beta),
        ("fails", fails),
    ]);
}
