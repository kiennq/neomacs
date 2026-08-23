#![cfg(unix)]

mod support;

#[test]
fn startup_readiness_failure_reports_editor_grid_and_recent_output() {
    let grid = vec!["stuck startup".to_string(), "still loading".to_string()];

    let message =
        support::startup_readiness_failure_message("Neomacs", false, &grid, b"raw pty output")
            .expect("a missed startup predicate should produce diagnostics");

    assert!(message.contains("Neomacs"));
    assert!(message.contains("Grid:\nstuck startup\nstill loading"));
    assert!(message.contains("Recent PTY output:\nraw pty output"));
    assert!(
        support::startup_readiness_failure_message("Neomacs", true, &grid, b"raw pty output")
            .is_none()
    );
}
#[test]
fn scratch_ready_accepts_custom_content_with_visible_mode_line() {
    let grid = vec![
        "custom probe content".to_string(),
        "-UUU:---F1  *scratch*  (Fundamental)".to_string(),
    ];

    assert!(support::scratch_ready(&grid));
    assert!(
        !grid
            .iter()
            .any(|row| row.contains("This buffer is for text that is not saved"))
    );
}
