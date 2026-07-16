use std::process::Command;

#[test]
fn missing_llm_api_base_error_mentions_environment_and_flag() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentic-server"))
        .env_remove("LLM_API_BASE")
        .output()
        .expect("agentic-server must run");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr must be UTF-8");
    assert!(
        stderr.contains("LLM_API_BASE (or --llm-api-base)"),
        "unexpected error message: {stderr}"
    );
}
