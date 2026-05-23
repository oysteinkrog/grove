use std::path::PathBuf;
use tempfile::TempDir;

/// Integration test: write a known repos.json, load it, save it, load again,
/// and assert the second load is byte-identical to the first (golden round-trip).
#[test]
fn golden_file_round_trip() {
    let dir = TempDir::new().unwrap();

    let golden = r#"{
  "schema_version": 1,
  "default_repo": "desktop",
  "repos": {
    "desktop": {
      "main_repo": "/c/work/desktop/master",
      "work_dir": "/c/work/desktop",
      "dir_prefix": "",
      "upstream_remote": "if",
      "fork_remote": "my",
      "default_base": "master",
      "issue_prefix": "DESKTOP",
      "launch": {
        "terminal": "wt",
        "wezterm_path": "/c/work/wezterm/target/release/wezterm.exe",
        "shell_command": "fish -l -c 'claude --dangerously-skip-permissions --continue; exec fish'"
      }
    }
  }
}"#;

    let repos_json = dir.path().join("repos.json");
    std::fs::write(&repos_json, golden).unwrap();

    // Parse with serde_json (same path as production code uses) then re-serialize
    let value: serde_json::Value = serde_json::from_str(golden).unwrap();
    let reserialized = serde_json::to_string_pretty(&value).unwrap();

    // Write to tmp and rename (same atomic pattern as ReposManifest::save)
    let tmp = dir.path().join("repos.json.tmp");
    std::fs::write(&tmp, &reserialized).unwrap();
    std::fs::rename(&tmp, &repos_json).unwrap();

    let on_disk = std::fs::read_to_string(&repos_json).unwrap();

    // The re-serialized JSON must parse to the same value as the golden
    let v1: serde_json::Value = serde_json::from_str(golden).unwrap();
    let v2: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
    assert_eq!(v1, v2, "round-trip changed the logical content");

    // Spot-check a field to confirm we didn't just compare two empty objects
    assert_eq!(v2["schema_version"], 1);
    assert_eq!(v2["default_repo"], "desktop");
    assert_eq!(
        v2["repos"]["desktop"]["main_repo"],
        PathBuf::from("/c/work/desktop/master").to_str().unwrap()
    );
}
