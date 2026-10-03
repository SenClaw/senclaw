//! `pin_senclaw_dirs_in_env` rewrites a relative `SENCLAW_HOME` to the folder
//! the daemon resolved, so a child started in another directory reads the
//! same one. Its own test binary: it mutates the process environment, which
//! the lib tests are careful never to do.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn a_child_in_another_directory_inherits_the_absolute_state_folder() {
    let cwd = std::env::current_dir().unwrap();
    std::env::set_var("SENCLAW_HOME", "./.senclaw-dev");
    std::env::remove_var("SENCLAW_DATA_HOME");

    senclaw::util::paths::pin_senclaw_dirs_in_env();

    let expected = cwd.join(".senclaw-dev");
    assert_eq!(PathBuf::from(std::env::var("SENCLAW_HOME").unwrap()), expected);
    assert_eq!(senclaw::util::paths::senclaw_data_home(), expected.join("data"));
    // An unset data variable stays unset: children derive it the same way.
    assert!(std::env::var("SENCLAW_DATA_HOME").is_err());

    let out = Command::new("sh")
        .args(["-c", "printf %s \"$SENCLAW_HOME\""])
        .current_dir(std::env::temp_dir())
        .output()
        .unwrap();
    assert_eq!(PathBuf::from(String::from_utf8(out.stdout).unwrap()), expected);
}
