//! Checks that both binaries start and describe themselves.

use std::process::Command;

#[test]
fn daemon_help_names_the_daemon() {
    let output = Command::new(env!("CARGO_BIN_EXE_ralphex-macos-runner"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("ralphex-macos-runner"), "{stdout}");
}

#[test]
fn client_help_names_the_client() {
    let output = Command::new(env!("CARGO_BIN_EXE_rxd"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("rxd"), "{stdout}");
}

#[test]
fn client_help_lists_every_subcommand() {
    let output = Command::new(env!("CARGO_BIN_EXE_rxd"))
        .arg("--help")
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    for subcommand in ["attach", "install", "uninstall"] {
        assert!(
            stdout.contains(subcommand),
            "{subcommand} missing: {stdout}"
        );
    }
}
