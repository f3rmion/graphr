use std::process::Command;

#[test]
fn version_is_stable() {
    let version = Command::new(env!("CARGO_BIN_EXE_grapher"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        "grapher 0.1.0\n"
    );
    assert!(version.stderr.is_empty());
}

#[test]
fn invalid_cli_is_concise() {
    let output = Command::new(env!("CARGO_BIN_EXE_grapher"))
        .arg("\x1b[31m\nbad")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Usage: grapher --version\n"
    );
}
