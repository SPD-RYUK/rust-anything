mod common;

use std::{path::Path, process::Command};

use rust_anything::{
    codegen_java::{DRIVER_JAVA, ERROR_JAVA, JSON_JAVA},
    harness::DiffOutcome,
};

#[test]
fn java_replay_distinguishes_null_from_a_rejected_argument() {
    if !common::have("java") || !common::have("javac") {
        return;
    }

    let root = std::env::temp_dir().join(format!("rust-anything-java-null-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let result = replay(&root);
    std::fs::remove_dir_all(root).unwrap();

    let diff = result.unwrap();
    assert_eq!((diff.passed, diff.failed), (1, 1));
    assert_eq!(diff.mismatches[0]["actual"]["invalid"], true);
}

#[test]
fn java_error_lookup_matches_across_json_typed_and_wire_paths() {
    if !common::have("java") || !common::have("javac") {
        return;
    }

    let root = std::env::temp_dir().join(format!("rust-anything-java-err-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let result = check_error_lookup(&root);
    std::fs::remove_dir_all(root).unwrap();
    result.unwrap();
}

fn check_error_lookup(root: &Path) -> anyhow::Result<()> {
    std::fs::write(root.join("RustAnythingError.java"), ERROR_JAVA)?;
    std::fs::write(
        root.join("CheckErrors.java"),
        r#"import rustanything.RustAnythingError;

public class CheckErrors {
    private static void checkBoth(String type, String msg, Class<?> expected) {
        RuntimeException fromJson = RustAnythingError.from(type + ": " + msg);
        RuntimeException fromReal = new RustAnythingError(type, msg).real();
        RuntimeException fromSlot;
        try {
            String[] slot = RustAnythingError.slot();
            slot[0] = type;
            slot[1] = msg;
            RustAnythingError.check(slot);
            throw new AssertionError("expected exception for " + type);
        } catch (RuntimeException e) {
            fromSlot = e;
        }
        if (fromJson.getClass() != expected || fromReal.getClass() != expected || fromSlot.getClass() != expected) {
            throw new AssertionError("class mismatch for " + type + ": "
                + fromJson.getClass() + " / " + fromReal.getClass() + " / " + fromSlot.getClass());
        }
        if (!fromJson.getMessage().equals(fromReal.getMessage())
            || !fromJson.getMessage().equals(fromSlot.getMessage())) {
            throw new AssertionError("message mismatch for " + type);
        }
    }

    public static void main(String[] args) {
        checkBoth("IllegalArgumentException", "bad", IllegalArgumentException.class);
        checkBoth("NoSuchElementException", "missing", java.util.NoSuchElementException.class);
        checkBoth("DateTimeException", "empty year", java.time.DateTimeException.class);
        checkBoth("java.time.DateTimeException", "empty year", java.time.DateTimeException.class);
        checkBoth("RejectedExecutionException", "busy", java.util.concurrent.RejectedExecutionException.class);
        checkBoth("NoSuchException", "oops", RuntimeException.class);
        RuntimeException plain = RustAnythingError.from("bad input: 42");
        if (plain.getClass() != RuntimeException.class || !plain.getMessage().equals("bad input: 42")) {
            throw new AssertionError("unexpected plain error: " + plain);
        }
    }
}
"#,
    )?;

    run(Command::new("javac").current_dir(root).args([
        "-d",
        ".",
        "RustAnythingError.java",
        "CheckErrors.java",
    ]))?;
    run(Command::new("java").current_dir(root).args(["-cp", ".", "CheckErrors"]))?;
    Ok(())
}

fn replay(root: &Path) -> anyhow::Result<DiffOutcome> {
    std::fs::write(root.join("RustAnythingDriver.java"), DRIVER_JAVA)?;
    std::fs::write(root.join("RustAnythingJson.java"), JSON_JAVA)?;
    std::fs::write(
        root.join("Port.java"),
        "public class Port { public static String value(int input) { return null; } }",
    )?;
    std::fs::write(root.join("targets.json"), r#"{"value":["Port","value"]}"#)?;
    std::fs::write(
        root.join("cases.jsonl"),
        r#"{"fn":"value","args":[1],"ok":null}
{"fn":"value","args":["previously accepted"],"ok":null}
"#,
    )?;

    run(Command::new("javac").current_dir(root).args([
        "-d",
        ".",
        "RustAnythingDriver.java",
        "RustAnythingJson.java",
        "Port.java",
    ]))?;
    run(Command::new("java").current_dir(root).args([
        "-cp",
        ".",
        "rustanything.RustAnythingDriver",
        "diff",
        "targets.json",
        "cases.jsonl",
        "diff.json",
    ]))?;
    Ok(serde_json::from_slice(&std::fs::read(
        root.join("diff.json"),
    )?)?)
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let output = cmd.output()?;
    anyhow::ensure!(
        output.status.success(),
        "{cmd:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
